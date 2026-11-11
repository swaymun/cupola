import Foundation

// MARK: - Wire contract (packages/protocol/schemas/wonder-http-v1.json)

/// A provider-verified child of one Codex Project thread. Project tasks are
/// read-only until attached to a Project conversation.
public struct ProjectSubagentSummary: Codable, Hashable, Identifiable, Sendable {
    public let parentConversationId: String
    public let threadId: String
    public let title: String
    public let agentNickname: String?
    public let agentRole: String?
    public let status: String
    public let isArchived: Bool
    public let canAcceptDirectInput: Bool
    /// Absent from hosts that predate Claude agent tasks; those name their subagents.
    public let namedSubagents: Bool?
    /// `agent`, `bash` (a foreground Bash run), `command` (a background command),
    /// `monitor`, `session` or `task` for Claude tasks, which have no names of their own.
    public let kind: String?
    /// Wonder is driving the session, so a running task can be stopped here.
    public let canStop: Bool?
    /// The session is running in Claude on the Mac, which keeps its own Stop control.
    public let runningElsewhere: Bool?
    public let updatedAt: String?
    /// How long a finished Claude task ran; absent while it runs or when unknown.
    public let durationSeconds: Int?
    public var currentActivity: CurrentActivity? = nil
    /// The provider's current task summary, never its final result.
    public var progressSummary: String? = nil
    public var currentStep: String? {
        guard isRunning && !isArchived else { return nil }
        return progressSummary ?? currentActivity?.label
    }
    public var id: String { threadId }

    /// Codex helpers have nicknames; Claude tasks show their description and a status glyph.
    public var usesAvatar: Bool { namedSubagents ?? true }
    public var isRunning: Bool { ["active", "running", "inProgress"].contains(status) }
    public var isStoppable: Bool { isRunning && canStop == true }
    public var isRunningElsewhere: Bool { isRunning && runningElsewhere == true }
    /// A command Codex left running (a dev server, a watcher). The host gives it
    /// the id `command:<processId>`; it can be stopped but has no history to open.
    public var isRuntimeCommand: Bool { kind == "command" && threadId.hasPrefix("command:") }
    /// A T3 Code subtask (a `delegate_task` child); the host reads its messages from T3 Code.
    public var isT3Subtask: Bool { kind == "t3Task" }
    public var opensHistory: Bool { !isRuntimeCommand }

    /// What Stop asks before ending this task, naming the provider that stops it
    /// ("Codex will stop “npm run dev”.").
    public func stopConfirmation(provider: String?) -> (title: String, action: String, message: String) {
        let command = kind == "command"
        let subject = provider.map { "\($0) will stop" } ?? "This stops"
        return (command ? "Stop this command?" : "Stop this task?",
                command ? "Stop command" : "Stop task",
                "\(subject) \u{201C}\(title)\u{201D}." + (command ? "" : " Work it already finished is kept."))
    }

    /// SF Symbol for the task's state, so Claude tasks need no avatar.
    public var statusSymbol: String {
        switch status {
        case "active", "running", "inProgress": "circle.dotted"
        case "completed": "checkmark.circle"
        case "interrupted", "shutdown": "stop.circle"
        case "failed", "errored": "exclamationmark.triangle"
        default: "questionmark.circle"
        }
    }

    /// What the task is: its agent type or "Background command".
    public var kindLabel: String? {
        guard !usesAvatar else { return nil }
        if let role = agentRole, !role.isEmpty { return role }
        return switch kind {
        case "command": "Background command"
        default: "Agent"
        }
    }

    /// How long a finished task ran, as Claude shows it: "9s", "4m 54s", "1h 2m".
    public var durationLabel: String? {
        guard let seconds = durationSeconds, seconds >= 0, !isRunning else { return nil }
        if seconds < 60 { return "\(seconds)s" }
        if seconds < 3600 { return "\(seconds / 60)m \(seconds % 60)s" }
        return "\(seconds / 3600)h \(seconds % 3600 / 60)m"
    }

    /// The duration in words, for VoiceOver.
    public var durationAccessibilityLabel: String? {
        guard let seconds = durationSeconds, seconds >= 0, !isRunning else { return nil }
        func unit(_ value: Int, _ name: String) -> String? {
            value > 0 ? "\(value) \(name)\(value == 1 ? "" : "s")" : nil
        }
        let parts = seconds < 3600
            ? [unit(seconds / 60, "minute"), unit(seconds % 60, "second")]
            : [unit(seconds / 3600, "hour"), unit(seconds % 3600 / 60, "minute")]
        let words = parts.compactMap { $0 }.joined(separator: " ")
        return "ran for " + (words.isEmpty ? "0 seconds" : words)
    }

    public var statusLabel: String {
        let state: String = switch status {
        case "active", "running", "inProgress": "Running"
        case "idle": "Idle"
        case "pendingInit", "pending", "waiting": "Waiting"
        case "waitingOnApproval": "Waiting for approval"
        case "waitingOnUserInput": "Waiting for input"
        case "completed": "Completed"
        case "interrupted", "shutdown": "Stopped"
        case "failed", "errored": "Failed"
        case "notLoaded": "Status unknown"
        default: "Status unknown"
        }
        return isArchived ? "Archived · \(state)" : state
    }

    public func statusLabel(available: Bool) -> String {
        available ? statusLabel : "Last known: " + statusLabel
    }
}

public struct ProjectSubagentSections: Equatable, Sendable {
    public let running: [ProjectSubagentSummary]
    /// Finished tasks, including failed and stopped ones; each row labels its outcome.
    public let completed: [ProjectSubagentSummary]
}

/// What the composer's agent-task pill says. Claude tasks count as running,
/// and the glyph turns, only while the computer is reachable and its latest
/// read still lists them running (`fresh`); otherwise the pill shows the
/// total, still. The count always has its noun ("240 tasks").
public struct ProjectSubagentPill: Equatable, Sendable {
    /// Claude tasks (no avatars): the running/finished glyph applies.
    public let tasksOnly: Bool
    /// Tasks the computer just reported running; 0 when unconfirmed.
    public let running: Int
    public let total: Int
    public let available: Bool
    /// Last-known running tasks the computer hasn't confirmed: a still glyph, not a check.
    public let unconfirmedRunning: Bool

    public init(agents: [ProjectSubagentSummary], sections: ProjectSubagentSections?, available: Bool, fresh: Set<String>) {
        tasksOnly = !agents.isEmpty && !agents.contains(where: \.usesAvatar)
        let listed = sections?.running ?? agents.filter(\.isRunning)
        running = tasksOnly && available ? listed.filter { fresh.contains($0.threadId) }.count : 0
        total = agents.count
        self.available = available
        unconfirmedRunning = tasksOnly && running == 0 && !listed.isEmpty
    }

    public var spins: Bool { running > 0 }
    /// The number shown alone where a word doesn't fit (a folded Duo's bar, accessibility sizes).
    public var count: Int { spins ? running : total }
    public var text: String { spins ? Self.tasks(running) + " running" : Self.tasks(total) }
    public var symbolName: String {
        guard tasksOnly else { return "sparkles.rectangle.stack" }
        return spins || unconfirmedRunning ? "circle.dotted" : "checkmark.circle"
    }
    public var accessibilityLabel: String {
        let prefix = available ? "" : "Last known: "
        guard tasksOnly else { return prefix + "\(total) Project \(total == 1 ? "agent task" : "agent tasks")" }
        return prefix + (spins
            ? "\(running) agent \(running == 1 ? "task" : "tasks") running"
            : "\(total) agent \(total == 1 ? "task" : "tasks")")
    }
    private static func tasks(_ n: Int) -> String { "\(n.formatted()) \(n == 1 ? "task" : "tasks")" }
}

public extension Array where Element == ProjectSubagentSummary {
    var runningCount: Int { filter(\.isRunning).count }

    /// Running tasks first, then the rest, each by most recent activity. The sort
    /// is stable and keyed on fields that change only when a task changes state,
    /// so a streaming refresh does not reorder rows.
    func sectioned() -> ProjectSubagentSections {
        func recent(_ list: [ProjectSubagentSummary]) -> [ProjectSubagentSummary] {
            list.enumerated().sorted { a, b in
                switch (a.element.updatedAt, b.element.updatedAt) {
                case let (x?, y?) where x != y: return x > y
                case (nil, _?): return false
                case (_?, nil): return true
                default: return a.offset < b.offset
                }
            }.map(\.element)
        }
        return ProjectSubagentSections(running: recent(filter(\.isRunning)), completed: recent(filter { !$0.isRunning }))
    }
}

public struct ProjectSubagentList: Codable, Sendable {
    public let available: Bool
    public let detail: String?
    public let subagents: [ProjectSubagentSummary]
    public let nextCurrentCursor: String?
    public let nextArchivedCursor: String?
}

public struct ProjectSubagentTranscript: Codable, Sendable {
    public let subagent: ProjectSubagentSummary
    public let snapshot: ConversationSnapshot
}

public enum ProjectSubagentPaths {
    private static func component(_ value: String) throws -> String {
        guard !value.isEmpty,
              let escaped = value.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else {
            throw PairingFailure.invalidLink
        }
        return escaped
    }

    public static func roster(parentConversationId: String, archived: Bool? = nil,
                              cursor: String? = nil) throws -> String {
        let base = "/api/v1/project-conversations/\(try component(parentConversationId))/subagents"
        guard let archived else {
            guard cursor == nil else { throw PairingFailure.invalidLink }
            return base
        }
        let path = base + "?archived=\(archived ? "true" : "false")"
        guard let cursor else { return path }
        guard cursor.count <= 2048 else { throw PairingFailure.invalidLink }
        return path + "&cursor=\(try component(cursor))"
    }

    public static func stop(parentConversationId: String, threadId: String) throws -> String {
        try roster(parentConversationId: parentConversationId) + "/\(try component(threadId))/stop"
    }

    /// `kind` is the row's kind from the roster: the host reads a `t3Task` from T3 Code only.
    public static func transcript(parentConversationId: String, threadId: String, cursor: String? = nil,
                                  kind: String? = nil) throws -> String {
        let base = try roster(parentConversationId: parentConversationId)
            + "/\(try component(threadId))/transcript"
        var query: [String] = []
        if let cursor { query.append("cursor=\(try component(cursor))") }
        if kind == "t3Task" { query.append("kind=t3Task") }
        return query.isEmpty ? base : base + "?" + query.joined(separator: "&")
    }
}

public struct ProjectFolder: Codable, Hashable, Identifiable, Sendable {
    public let id: String
    public let path: String
    public let name: String
    public let isPrimary: Bool
    public let isAvailable: Bool
    public init(id: String, path: String, name: String, isPrimary: Bool, isAvailable: Bool) {
        self.id = id; self.path = path; self.name = name; self.isPrimary = isPrimary; self.isAvailable = isAvailable
    }
}

/// A named group of source folders on one paired Mac.
public struct ProjectSummary: Codable, Hashable, Identifiable, Sendable {
    public let id: String
    public let name: String
    /// The computer's own Project for chats that have none of the owner's;
    /// shown with a dashed folder.
    public var isNoProject: Bool { id == "scratch" }
    public let isIncluded: Bool
    public let isPinned: Bool
    public let rootsRevision: Int
    public let folders: [ProjectFolder]
    public let lastFamily: AgentFamily?
    public let lastUsedAt: String?
    public let createdAt: String
    /// This Project's own auto-archive rule; nil follows the computer's.
    public let autoArchive: AutoArchiveRule?
    /// The model last chosen here for each provider, kept by the computer so
    /// every device agrees; nil in a list cached before it existed.
    public let lastModels: [ProjectLastModel]?
    public init(id: String, name: String, isIncluded: Bool = true, isPinned: Bool = false, rootsRevision: Int = 1,
                folders: [ProjectFolder], lastFamily: AgentFamily? = nil, lastUsedAt: String? = nil, createdAt: String = "",
                autoArchive: AutoArchiveRule? = nil, lastModels: [ProjectLastModel]? = nil) {
        self.id = id; self.name = name; self.isIncluded = isIncluded; self.isPinned = isPinned; self.rootsRevision = rootsRevision
        self.folders = folders; self.lastFamily = lastFamily; self.lastUsedAt = lastUsedAt; self.createdAt = createdAt
        self.autoArchive = autoArchive; self.lastModels = lastModels
    }
    public var primaryFolder: ProjectFolder? { folders.first(where: \.isPrimary) ?? folders.first }
    /// The model a new `family` chat in this Project starts on when it has no
    /// choice of its own, ahead of the computer's default.
    public func lastModel(for family: AgentFamily?) -> RememberedModel? {
        guard let family, let last = lastModels?.first(where: { $0.family == family }) else { return nil }
        return RememberedModel(model: last.model, effort: last.effort)
    }
    /// This Project as the computer records the owner choosing `model` for
    /// `family` in it (starting a thread also makes `family` its last
    /// provider), until the computer's next list replaces it.
    public func recording(_ family: AgentFamily, model: String, effort: String?, startsThread: Bool) -> ProjectSummary {
        let others = (lastModels ?? []).filter { $0.family != family }
        return ProjectSummary(id: id, name: name, isIncluded: isIncluded, isPinned: isPinned, rootsRevision: rootsRevision,
                              folders: folders, lastFamily: startsThread ? family : lastFamily, lastUsedAt: lastUsedAt,
                              createdAt: createdAt, autoArchive: autoArchive,
                              lastModels: (others + [ProjectLastModel(family: family, model: model, effort: effort)])
                                  .sorted { $0.family.rawValue < $1.family.rawValue })
    }
}

/// The model and effort the owner last chose for one provider in a Project.
public struct ProjectLastModel: Codable, Hashable, Sendable {
    public let family: AgentFamily
    public let model: String
    public let effort: String?
    public init(family: AgentFamily, model: String, effort: String? = nil) {
        self.family = family; self.model = model; self.effort = effort
    }
}

public struct ProjectFamilyAvailability: Codable, Hashable, Sendable {
    public let family: AgentFamily
    public let available: Bool
    /// False when the Mac's last check found the provider not installed; nil
    /// when the host did not check (or predates the field). See `lists(_:)`.
    public let installed: Bool?
    /// The host's record, or `ProviderCapabilities.unoffered` when it sends
    /// one this app cannot read, so Projects never fail on it.
    public let capabilities: ProviderCapabilities
    public init(family: AgentFamily, available: Bool, installed: Bool? = nil, capabilities: ProviderCapabilities) {
        self.family = family; self.available = available; self.installed = installed; self.capabilities = capabilities
    }
    private enum CodingKeys: String, CodingKey { case family, available, installed, capabilities }
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let family = try container.decode(AgentFamily.self, forKey: .family)
        self.init(family: family, available: try container.decode(Bool.self, forKey: .available),
                  installed: try? container.decodeIfPresent(Bool.self, forKey: .installed),
                  capabilities: (try? container.decodeIfPresent(ProviderCapabilities.self, forKey: .capabilities)) ?? .unoffered)
    }
}

public struct ProjectsResponse: Codable, Sendable {
    public let partial: [ProjectPartialFailure]?
    public let projects: [ProjectSummary]
    public let families: [ProjectFamilyAvailability]
    /// 1 when the host offers native Project-scoped Codex skills.
    public var skillsVersion: Int? = nil
    /// 1 when the host accepts `claudeApproval` and `planMode`; nil on older hosts.
    public let modesVersion: Int?
    /// 1 when the host supports provider-synchronized Codex archiving.
    public let archiveVersion: Int?
    /// 1 when Cupola can permanently delete its saved copy of a chat.
    public var deleteVersion: Int? = nil
    /// 1 when a Project can be deleted from the computer's Cupola copy (its chats go; folders and provider history stay).
    public var projectDeleteVersion: Int? = nil
    /// Pinned threads across included projects, most recent first; nil on older hosts.
    public let pinned: [PinnedProjectThread]?
    /// 1 when each family carries its `capabilities` record.
    public var capabilitiesVersion: Int? = nil
    /// 1 when Project conversations can open a terminal on the Mac; nil on older hosts.
    public var terminalVersion: Int? = nil
    public init(projects: [ProjectSummary], families: [ProjectFamilyAvailability], modesVersion: Int? = nil, pinned: [PinnedProjectThread]? = nil, archiveVersion: Int? = nil, partial: [ProjectPartialFailure]? = nil) {
        self.partial = partial
        self.projects = projects; self.families = families; self.modesVersion = modesVersion; self.pinned = pinned
        self.archiveVersion = archiveVersion
    }
}

/// A pinned thread and the project it belongs to on the same Mac.
public struct PinnedProjectThread: Codable, Hashable, Identifiable, Sendable {
    public let projectId: String
    public let thread: ProjectThreadSummary
    public var id: String { thread.reference }
    public init(projectId: String, thread: ProjectThreadSummary) { self.projectId = projectId; self.thread = thread }
    /// From the offline cache; see `ProjectThreadSummary.saved`.
    public var saved: PinnedProjectThread { PinnedProjectThread(projectId: projectId, thread: thread.saved) }
}

/// How a Claude thread with project access asks before acting. Codex uses
/// its own approval policy; this applies only when `accessMode` is workspace.
public enum ClaudeApproval: String, Codable, CaseIterable, Identifiable, Sendable {
    /// Asks before edits and commands.
    case ask
    /// Edits project files without asking; asks before commands. No longer
    /// offered, but threads that chose it keep it.
    case acceptEdits = "accept_edits"
    /// Edits the project without asking; Claude Code's classifier approves
    /// routine commands and asks about risky ones.
    case auto
    public var id: String { rawValue }
    /// New Claude threads start in Auto.
    public static let standard = ClaudeApproval.auto
}

public struct ProjectPartialFailure: Codable, Hashable, Sendable {
    public let family: AgentFamily
    public let detail: String
}

public struct ProjectCandidate: Codable, Hashable, Identifiable, Sendable {
    public let name: String
    public let folders: [String]
    public let sources: [AgentFamily]
    public let updatedAt: Int?
    public let isIncluded: Bool
    public var id: String { folders.joined(separator: "\n") }
}

public struct ProjectCandidatesResponse: Codable, Sendable {
    public let candidates: [ProjectCandidate]
    public let partial: [ProjectPartialFailure]
}

/// Host-wide Project discovery, shared by every provider on this computer.
public struct ProjectDiscoverySettings: Codable, Equatable, Sendable {
    public let includeWorktrees: Bool
    /// The computer's auto-archive rule; nil on hosts without auto-archive.
    public let autoArchive: AutoArchiveRule?
    /// Whether the computer continues work by itself; nil on hosts without it.
    public let resumeAfterLimit: Bool?
    public let continueAfterRestart: Bool?
    public static let path = "/api/v1/settings/projects"
    public init(includeWorktrees: Bool, autoArchive: AutoArchiveRule? = nil,
                resumeAfterLimit: Bool? = nil, continueAfterRestart: Bool? = nil) {
        self.includeWorktrees = includeWorktrees; self.autoArchive = autoArchive
        self.resumeAfterLimit = resumeAfterLimit; self.continueAfterRestart = continueAfterRestart
    }
    public var continuation: ContinuationSettings? {
        guard let resumeAfterLimit, let continueAfterRestart else { return nil }
        return ContinuationSettings(resumeAfterLimit: resumeAfterLimit, continueAfterRestart: continueAfterRestart)
    }
}

/// When the computer continues work without the owner: a thread a usage
/// limit stopped resumes once the limit resets, and a reply a crash or
/// restart cut off continues. Saved with `PUT ProjectDiscoverySettings.path`.
public struct ContinuationSettings: Codable, Hashable, Sendable {
    public var resumeAfterLimit: Bool
    public var continueAfterRestart: Bool
    public init(resumeAfterLimit: Bool, continueAfterRestart: Bool) {
        self.resumeAfterLimit = resumeAfterLimit; self.continueAfterRestart = continueAfterRestart
    }
}

/// "Resumes at 3:40 PM" for a thread a usage limit stopped (`resumesAt`).
public enum UsageResume {
    public static func date(_ value: String?) -> Date? {
        guard let value else { return nil }
        return try? Date(value, strategy: .iso8601)
    }
    public static func label(_ at: Date, now: Date = Date(), calendar: Calendar = .current, locale: Locale = .current,
                             verb: String = "Resumes") -> String {
        let time = at.formatted(Date.FormatStyle(date: .omitted, time: .shortened, locale: locale, calendar: calendar, timeZone: calendar.timeZone))
        if calendar.isDate(at, inSameDayAs: now) || at <= now { return "\(verb) at \(time)" }
        if let tomorrow = calendar.date(byAdding: .day, value: 1, to: now), calendar.isDate(at, inSameDayAs: tomorrow) {
            return "\(verb) tomorrow at \(time)"
        }
        let day = at.formatted(Date.FormatStyle(locale: locale, calendar: calendar, timeZone: calendar.timeZone).weekday(.abbreviated))
        return "\(verb) \(day) at \(time)"
    }
}

/// Saves only the computer's auto-archive rule (`PUT ProjectDiscoverySettings.path`).
public struct AutoArchiveSettingsUpdate: Encodable, Sendable {
    public let autoArchive: AutoArchiveRule
    public init(_ rule: AutoArchiveRule) { autoArchive = rule }
}

/// When the computer archives Project threads by itself: after `idleDays`
/// without activity (0 = never) and, with `onMerge`, once their pull request
/// merges. Pinned, working and waiting threads are never archived.
public struct AutoArchiveRule: Codable, Hashable, Sendable {
    public let idleDays: Int
    public let onMerge: Bool
    public init(idleDays: Int, onMerge: Bool) { self.idleDays = idleDays; self.onMerge = onMerge }
    /// The choices Settings offers; a different saved value is shown as well.
    public static let dayChoices = [0, 1, 3, 7, 14, 30]
    public static func daysLabel(_ days: Int) -> String {
        switch days {
        case 0: "Never"
        case 1: "After 1 day"
        default: "After \(days) days"
        }
    }
    /// The PATCH body value for a Project: a rule, or null to follow the computer.
    public static func projectField(_ rule: AutoArchiveRule?) -> Any {
        guard let rule else { return NSNull() }
        return ["idleDays": rule.idleDays, "onMerge": rule.onMerge]
    }
}

/// A native Codex or Claude Code thread. `conversationId` is nil until the
/// owner opens it in Wonder, which attaches it without starting work.
public struct ProjectThreadSummary: Codable, Hashable, Identifiable, Sendable {
    public let reference: String
    public let conversationId: String?
    public let title: String
    public let family: AgentFamily
    public let updatedAt: Int
    public let isPinned: Bool
    public let hasUnread: Bool
    public let isWorking: Bool
    /// Branch, or the linked worktree folder when detached; nil on older hosts.
    public let worktreeLabel: String?
    /// The branch checked out in the thread's working folder; nil outside Git
    /// and on older hosts.
    public let branch: String?
    /// Where the thread works, shown after its Project: the live branch, else
    /// the linked worktree's label (a detached worktree has no branch).
    public var locationLabel: String? { branch ?? worktreeLabel }
    /// Positive provider origin or current desktop ownership. Older hosts omit it;
    /// native references alone never establish computer provenance.
    public let isComputerSession: Bool?
    public var computerSessionLabel: String? {
        isComputerSession == true ? "Started on computer or owned by a desktop app" : nil
    }
    public let currentActivity: CurrentActivity?
    /// The agent waits on the Mac itself: a permission prompt or question in
    /// its desktop app, answered there rather than in Wonder. Older Macs omit it.
    public let needsInputOnMac: Bool?
    /// The provider reports a permission wait for this chat (for example in
    /// T3 Code). Unattributed Mac dialogs belong to the host's Computer notice.
    public let waitingOnMacPermission: Bool?
    public var waitsOnMac: Bool { needsInputOnMac == true || waitsOnMacPermission }
    public var waitsOnMacPermission: Bool { waitingOnMacPermission == true }
    public static let macPermissionWait = "Waiting for a permission on your Mac"
    public var currentStep: String? { isWorking ? currentActivity?.label : nil }
    /// The agent still runs background agents, commands or terminals after its
    /// reply ended. Older Macs omit it.
    public let backgroundWork: Bool?
    /// A reply or background work is running: the Working filter and the spinner.
    public var isRunning: Bool { isWorking || backgroundWork == true }
    public static let backgroundWorkLabel = "Running in the background"
    /// The newest automation run that posted into this thread; nil when none
    /// has, and on older Macs. The row shows a clock for it.
    public let automationRun: ThreadAutomationRun?
    /// A usage limit stopped the thread and the computer continues it then
    /// (RFC 3339); nil otherwise and on older Macs.
    public let resumesAt: String?
    /// Archived list only: the thread's home app closed it ("Settled in T3
    /// Code", "Archived in the Claude app"); nil when Cupola or Codex archived
    /// it, and on older Macs.
    public let closedIn: String?
    /// Archived list only: a thread snoozed in T3 Code returns then (RFC 3339).
    public let returnsAt: String?
    /// Archived list only: Restore reopens it in its home app too, when the
    /// computer is connected there.
    public let reopensThere: Bool?
    /// A T3 Code subagent whose parent chat T3 Code no longer has, so it is
    /// listed on its own (a subagent of a known chat shows in that chat's
    /// agent tasks instead); nil otherwise and on older Macs.
    public let isSubagent: Bool?
    public static let subagentLabel = "Subagent"
    /// The Archived list's line for a thread its home app closed: "Settled in
    /// T3 Code", or "Snoozed in T3 Code, returns tomorrow at 9:00 AM".
    public func closedLabel(now: Date = Date(), calendar: Calendar = .current, locale: Locale = .current) -> String? {
        guard let closedIn else { return nil }
        guard let at = UsageResume.date(returnsAt) else { return closedIn }
        return closedIn + ", " + UsageResume.label(at, now: now, calendar: calendar, locale: locale, verb: "returns")
    }
    public var id: String { reference }
    /// The provider's thread (Codex) or session (Claude Code) ID; nil for a
    /// Wonder draft that has no provider thread yet.
    public var nativeID: String? {
        let prefix = family.rawValue + ":"
        guard reference.hasPrefix(prefix), reference.count > prefix.count else { return nil }
        return String(reference.dropFirst(prefix.count))
    }
    public init(reference: String, conversationId: String?, title: String, family: AgentFamily, updatedAt: Int,
                isPinned: Bool = false, hasUnread: Bool = false, isWorking: Bool = false, currentActivity: CurrentActivity? = nil,
                needsInputOnMac: Bool? = nil, waitingOnMacPermission: Bool? = nil, isComputerSession: Bool? = nil, worktreeLabel: String? = nil,
                backgroundWork: Bool? = nil, branch: String? = nil, automationRun: ThreadAutomationRun? = nil, resumesAt: String? = nil,
                closedIn: String? = nil, returnsAt: String? = nil, reopensThere: Bool? = nil, isSubagent: Bool? = nil) {
        self.reference = reference; self.conversationId = conversationId; self.title = title; self.family = family
        self.updatedAt = updatedAt; self.isPinned = isPinned; self.hasUnread = hasUnread; self.isWorking = isWorking; self.currentActivity = currentActivity
        self.worktreeLabel = worktreeLabel; self.branch = branch
        self.isComputerSession = isComputerSession
        self.needsInputOnMac = needsInputOnMac
        self.waitingOnMacPermission = waitingOnMacPermission
        self.backgroundWork = backgroundWork
        self.automationRun = automationRun
        self.resumesAt = resumesAt
        self.closedIn = closedIn; self.returnsAt = returnsAt; self.reopensThere = reopensThere
        self.isSubagent = isSubagent
    }
}

/// An automation's latest run in a thread, as its Chats row shows it.
public struct ThreadAutomationRun: Codable, Hashable, Sendable {
    public let automationId: String
    /// Nil once the automation is deleted.
    public let name: String?
    /// `running`, `completed` or `failed`.
    public let status: String
    public let startedAt: String
    public let finishedAt: String?
    /// Why the run failed, in the Mac's words.
    public let error: String?
    public init(automationId: String, name: String?, status: String, startedAt: String,
                finishedAt: String? = nil, error: String? = nil) {
        self.automationId = automationId; self.name = name; self.status = status
        self.startedAt = startedAt; self.finishedAt = finishedAt; self.error = error
    }
    public var failed: Bool { status == "failed" }
    /// The clock's symbol: a failed run carries an exclamation mark.
    public var symbolName: String { failed ? "clock.badge.exclamationmark" : "clock" }
    /// Spoken with the row, and the clock's label.
    public var label: String {
        let subject = name.map { "Automation " + $0 } ?? "An automation"
        if failed { return subject + " failed" + (error.map { ": " + $0 } ?? "") }
        return status == "running" ? subject + " is running" : subject + " posted here"
    }
}

/// Codex-archived and Wonder-archived (Claude Code) threads of one Project.
public struct ArchivedProjectThreadsPage: Codable, Sendable {
    public let threads: [ProjectThreadSummary]
    public let partial: [ProjectPartialFailure]
    public init(threads: [ProjectThreadSummary], partial: [ProjectPartialFailure]) {
        self.threads = threads; self.partial = partial
    }
}

public struct ProjectThreadsPage: Codable, Sendable {
    public let threads: [ProjectThreadSummary]
    public let nextCursor: String?
    public let partial: [ProjectPartialFailure]
    /// The Mac answered before a provider finished reading its chats, so the
    /// page may lack rows it has not read yet (often all of them). Its cursor
    /// continues the read. Slowness is not a failure, so `partial` stays empty.
    public let loading: Bool
    public init(threads: [ProjectThreadSummary], nextCursor: String?, partial: [ProjectPartialFailure], loading: Bool = false) {
        self.threads = threads; self.nextCursor = nextCursor; self.partial = partial; self.loading = loading
    }
    private enum CodingKeys: String, CodingKey { case threads, nextCursor, partial, loading }
    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        threads = try values.decode([ProjectThreadSummary].self, forKey: .threads)
        nextCursor = try values.decodeIfPresent(String.self, forKey: .nextCursor)
        partial = try values.decode([ProjectPartialFailure].self, forKey: .partial)
        // Macs before the flag never reported a still-reading page.
        loading = try values.decodeIfPresent(Bool.self, forKey: .loading) ?? false
    }
    /// The same answer with `rows` in place of its threads.
    public func replacingThreads(_ rows: [ProjectThreadSummary]) -> ProjectThreadsPage {
        ProjectThreadsPage(threads: rows, nextCursor: nextCursor, partial: partial, loading: loading)
    }
}

/// Provider-honest access choices for a project thread.
public enum ProjectAccessMode: String, Codable, CaseIterable, Identifiable, Sendable {
    case readOnly = "read_only", workspace, fullAccess = "full_access"
    public var id: String { rawValue }
    public func title(for family: AgentFamily) -> String {
        switch self {
        case .readOnly: return "Read only"
        case .workspace: return family == .claude ? "Ask for approval" : "Edit project" // provider identity: wording
        case .fullAccess: return "Full access"
        }
    }
    public func detail(for family: AgentFamily) -> String {
        switch self {
        case .readOnly: return "Reads files and asks before changing anything."
        case .workspace: return family == .claude // provider identity: wording
            ? "Asks before edits and commands."
            : "Edits files in the project’s folders and asks before anything else."
        case .fullAccess: return "Runs without asking, including outside the project."
        }
    }
}

/// The three settings that together describe how much a thread may do.
public struct ProjectAccess: Hashable, Sendable {
    public var accessMode: ProjectAccessMode
    public var claudeApproval: ClaudeApproval?
    public var planMode: Bool
    public init(accessMode: ProjectAccessMode = .workspace, claudeApproval: ClaudeApproval? = nil, planMode: Bool = false) {
        self.accessMode = accessMode; self.claudeApproval = claudeApproval; self.planMode = planMode
    }

    /// The fields a PATCH needs to move from `self` to `next`. Older hosts
    /// reject unknown fields, and a provider with one approval mode rejects
    /// `claudeApproval`.
    public func changes(to next: ProjectAccess, capabilities: ProviderCapabilities, supportsModes: Bool) -> [String: Any] {
        var fields: [String: Any] = [:]
        if capabilities.accessChoices, accessMode != next.accessMode { fields["accessMode"] = next.accessMode.rawValue }
        guard supportsModes else { return fields }
        if capabilities.allowsApprovalChoice, (claudeApproval ?? .standard) != (next.claudeApproval ?? .standard) {
            fields["claudeApproval"] = (next.claudeApproval ?? .standard).rawValue
        }
        if planMode != next.planMode { fields["planMode"] = next.planMode }
        return fields
    }
}

/// One row of a project thread's access menu. Codex offers its three access
/// levels. Claude offers Ask, Auto and Full access, plus Plan; as in Claude
/// Code, none of them sandboxes commands. Hosts without project modes keep the original three levels.
public enum ProjectAccessChoice: String, CaseIterable, Identifiable, Sendable {
    case readOnly, workspace, manual, acceptEdits, auto, plan, fullAccess, providerSettings
    public var id: String { rawValue }

    /// Claude lists its permission modes in Claude Code's order, Plan included.
    /// Read only stays out of Claude's menu unless the thread already uses it,
    /// so its current setting is always shown.
    public static func choices(capabilities: ProviderCapabilities, supportsModes: Bool, current: ProjectAccess) -> [Self] {
        guard capabilities.accessChoices else { return [.providerSettings] }
        guard supportsModes, capabilities.allowsApprovalChoice else { return [.readOnly, .workspace, .fullAccess] }
        let legacy = current.accessMode == .readOnly
        let plan: [Self] = capabilities.plan == .permissionMode ? [.plan] : []
        return (legacy ? [.readOnly] : []) + [.auto, .manual, .acceptEdits] + plan + [.fullAccess]
    }

    public static func selected(for access: ProjectAccess, capabilities: ProviderCapabilities, supportsModes: Bool) -> Self {
        guard capabilities.accessChoices else { return .providerSettings }
        guard supportsModes, capabilities.allowsApprovalChoice else {
            switch access.accessMode {
            case .readOnly: return .readOnly
            case .workspace: return .workspace
            case .fullAccess: return .fullAccess
            }
        }
        if access.planMode, capabilities.plan == .permissionMode { return .plan }
        switch access.accessMode {
        case .readOnly: return .readOnly
        case .fullAccess: return .fullAccess
        case .workspace:
            switch access.claudeApproval ?? .standard {
            case .ask: return .manual
            case .acceptEdits: return .acceptEdits
            case .auto: return .auto
            }
        }
    }

    public func title(for family: AgentFamily, supportsModes: Bool) -> String {
        switch self {
        case .providerSettings: return "\(family.title) settings"
        case .readOnly: return "Read only"
        case .workspace: return supportsModes ? "Auto" : ProjectAccessMode.workspace.title(for: family)
        case .manual: return "Ask"
        case .acceptEdits: return "Accept edits"
        case .auto: return "Auto"
        case .plan: return "Plan"
        case .fullAccess: return "Full access"
        }
    }

    /// A short line that says what the choice allows.
    public func detail(for family: AgentFamily) -> String {
        switch self {
        case .providerSettings: return ""
        case .readOnly: return ProjectAccessMode.readOnly.detail(for: family)
        case .workspace: return ProjectAccessMode.workspace.detail(for: family)
        case .manual: return "Asks before edits and most commands."
        case .acceptEdits: return "Edits files without asking. Asks before commands."
        case .auto: return "Works without asking. Asks before risky actions."
        case .plan: return family == .opencode ? "Uses the Plan agent in OpenCode." : "Plans the work without changing files." // provider identity: wording
        case .fullAccess: return family == .claude // provider identity: wording
            ? "Never asks."
            : ProjectAccessMode.fullAccess.detail(for: family)
        }
    }

    /// Choices that remove the safety net are drawn in warning color.
    public var isElevated: Bool { self == .fullAccess }

    public func result(from access: ProjectAccess, capabilities: ProviderCapabilities, supportsModes: Bool) -> ProjectAccess {
        var next = access
        if !capabilities.accessChoices && self != .plan { return next }
        switch self {
        case .providerSettings: return next
        case .readOnly: next.accessMode = .readOnly
        case .workspace: next.accessMode = .workspace
        case .manual: next.accessMode = .workspace; next.claudeApproval = ClaudeApproval.ask
        case .acceptEdits: next.accessMode = .workspace; next.claudeApproval = .acceptEdits
        case .auto: next.accessMode = .workspace; next.claudeApproval = .auto
        case .fullAccess: next.accessMode = .fullAccess
        // Plan is its own row: choosing it again leaves plan mode.
        case .plan: next.planMode = !access.planMode; return next
        }
        // A permission-mode Plan is one of the modes: any other choice leaves it.
        if supportsModes && capabilities.plan == .permissionMode { next.planMode = false }
        return next
    }
}

public struct ProjectConversationDetail: Codable, Hashable, Sendable {
    /// T3's actual modes, absent for other chats and older computers.
    public let t3CodePermissions: T3CodePermissions?
    public let conversationId: String
    public let projectId: String
    public let projectName: String
    public let title: String
    public let family: AgentFamily
    public let model: String?
    public let effort: String?
    public let serviceTier: String?
    public let accessMode: ProjectAccessMode
    public let workingFolder: String
    public let worktreeLabel: String?
    public let workingFolderName: String
    public let isPinned: Bool
    public let hasUnread: Bool
    public let hasNativeSession: Bool
    /// nil on older hosts; the provider owns this state.
    public let isArchived: Bool?
    public let folderInProject: Bool
    /// The host serves history without admitting its folder to this Project.
    public let readOnlyReason: String?
    public let notice: String?
    /// A usage limit stopped the thread; the computer continues it then.
    public let resumesAt: String?
    /// nil on hosts without project modes; see `ProjectsResponse.modesVersion`.
    public let claudeApproval: ClaudeApproval?
    /// Codex collaboration plan mode, or Claude's plan permission mode.
    public let planMode: Bool?
    public var access: ProjectAccess {
        ProjectAccess(accessMode: accessMode, claudeApproval: claudeApproval, planMode: planMode == true)
    }
}

/// While Wonder on the Mac can't be reached, Send keeps working: messages wait
/// in the thread's outbox (`OutboxMessage`) and go out in order once it is back.
public enum ComposerReachability {
    public static func offlineNotice(macConnected: Bool?, receivingUpdates: Bool = false) -> String? {
        macConnected == false && !receivingUpdates ? "Cupola on your computer isn’t reachable. Messages you send wait here." : nil
    }
}

public struct CreateProjectThreadResponse: Codable, Sendable {
    public let conversation: ProjectThreadSummary
    public let receipt: SendReceipt?
}

public struct DesktopContinuation: Codable, Sendable {
    public struct Option: Codable, Hashable, Identifiable, Sendable {
        public let id: String
        public let title: String
        public let detail: String
        public let command: String
    }
    public let options: [Option]
    public let notes: [String]
    /// The host's reason there is no option (the provider has no Terminal resume).
    public let unavailable: String?
    /// What to say when there is no option, including hosts that send no reason.
    public var unavailableText: String? {
        options.isEmpty ? unavailable ?? "This chat can’t be continued in Terminal yet." : nil
    }
}

// MARK: - New chat routing

/// Where the draft composer sends. Every destination belongs to the draft's
/// selected connection; changing Macs never keeps another Mac's destination.
public enum ChatDestination: Codable, Hashable, Sendable {
    /// A genuinely new provider thread in this project.
    case project(id: String)
}

/// One durable new-chat draft per connection. Picking a destination never
/// starts model work; only an explicit Send does.
public struct NewChatDraft: Codable, Hashable, Sendable {
    public var destination: ChatDestination?
    public var codexSkills: [CodexSkillSelection]?
    public var text: String {
        didSet {
            codexSkills = CodexSkillSelection.retaining(codexSkills, in: text)
            // Editing, including clearing, cannot resolve an uncertain launch.
            // Keep its warning until an explicit resend or confirmed handoff.
        }
    }
    public var family: AgentFamily?
    public var model: String?
    public var effort: String?
    public var serviceTier: String?
    public var accessMode: ProjectAccessMode
    /// Claude only; nil keeps the host default (ask).
    public var claudeApproval: ClaudeApproval?
    /// Starts the thread in plan mode; nil or false starts normally.
    public var planMode: Bool?
    public var folderId: String?
    /// Reserved before dispatch so a lost response retries the same creation.
    public var requestID: String
    /// Set once a project thread's creation request is committed; the draft
    /// then retries the exact frozen request instead of editing it.
    public var submittedBody: String?
    public var submittedDeviceID: String?
    public var rootsRevision: Int?
    public var preparedConversationID: String?
    /// Local bytes live in the draft store, never in preferences or a Mac's
    /// remote attachment references.
    public var attachments: [NewChatAttachment]?
    /// Preview notes stay local until prepare-only creation returns a real
    /// conversation ID. They are bound to this Project and selected folder.
    public var annotations: [PendingNewChatAnnotation]?
    public var lastDictationRequestID: String?
    /// Sent while the Mac was unreachable: the frozen request waits in the
    /// New chat outbox and is created once the Mac is back (see `queue`).
    public var queuedAt: String?
    /// Set before the outbox first asks the Mac to create the thread; an
    /// unanswered attempt keeps its request ID, so it is never edited.
    public var outboxAttempted: Bool?
    /// Why the Mac refused the waiting request, in plain words.
    public var outboxRefusal: String?
    /// Whether Retry can repeat the refused request as it is.
    public var outboxRetryable: Bool?
    /// Start in T3 Code: the computer starts the thread in T3 Code instead
    /// (`T3CodeLaunch`); honored only while it is offered there, and only
    /// for this draft's first message.
    public var startsInT3Code: Bool?
    /// T3's permission choice, kept independently of the native provider's access.
    public var t3RuntimeMode: String?

    /// Nil follows this computer's current default. An explicit per-draft
    /// choice survives draft restoration without changing that default.
    public func usesT3Code(defaultEnabled: Bool, status: T3CodeStatus?, project: ProjectSummary?) -> Bool {
        if let startsInT3Code { return startsInT3Code }
        // An already submitted normal request keeps its original launch path.
        return !isSubmitted && defaultEnabled && status?.state == "connected" && project?.primaryFolder != nil
            && (status?.launchCatalog.map { !$0.families.isEmpty } ?? T3CodeLaunch.runs(family, status: status))
    }
    /// Start in T3 Code lost its answer, so T3 Code may have started this
    /// message (why, in plain words); Send asks before launching it again.
    public var t3LaunchUnconfirmed: String?

    public init(destination: ChatDestination? = nil, text: String = "", family: AgentFamily? = nil,
                model: String? = nil, effort: String? = nil, accessMode: ProjectAccessMode = .workspace,
                folderId: String? = nil, requestID: String = UUID().uuidString.lowercased(), submittedBody: String? = nil) {
        self.destination = destination; self.text = text; self.family = family; self.model = model
        self.effort = effort; self.accessMode = accessMode; self.folderId = folderId
        self.requestID = requestID; self.submittedBody = submittedBody
    }

    public var isSubmitted: Bool { submittedBody != nil }

    /// Choosing a project applies its last-used provider as a convenience. The
    /// choice stays visible and editable; it is never a silent substitution.
    public mutating func choose(_ destination: ChatDestination, project: ProjectSummary? = nil,
                                capabilities: (AgentFamily) -> ProviderCapabilities) {
        guard !isSubmitted else { return }
        if self.destination != destination {
            folderId = nil
            annotations = nil
            if case .project = destination { applyDefaults(of: project, capabilities: capabilities) }
        }
        self.destination = destination
    }

    /// Switching New chat to a Project starts a new draft from that Project's
    /// last provider. A draft saved there before keeps its own choices: a
    /// chat's own model comes before the Project's last one.
    public mutating func arrive(at destination: ChatDestination, project: ProjectSummary?,
                                capabilities: (AgentFamily) -> ProviderCapabilities) {
        guard !isSubmitted else { return }
        if self.destination == destination, case .project = destination {
            if let project { adoptDefaults(of: project, capabilities: capabilities) }
        } else {
            choose(destination, project: project, capabilities: capabilities)
        }
    }

    /// A draft made with its Project already chosen (a Project's New thread,
    /// a link) or saved before its provider was known has no provider yet: it
    /// takes the Project's last provider, as choosing the Project does.
    public mutating func adoptDefaults(of project: ProjectSummary, capabilities: (AgentFamily) -> ProviderCapabilities) {
        guard !isSubmitted, family == nil, destination == .project(id: project.id) else { return }
        applyDefaults(of: project, capabilities: capabilities)
    }

    /// The Project's last-used provider comes first. The model is left to
    /// `ensureModel`: the Project's last choice for that provider, else the
    /// computer's default.
    private mutating func applyDefaults(of project: ProjectSummary?, capabilities: (AgentFamily) -> ProviderCapabilities) {
        let preferred = project?.lastFamily ?? family ?? .codex // provider identity: default provider
        chooseFamily(preferred, capabilities: capabilities(preferred))
        model = nil; effort = nil; serviceTier = nil
        // A new chat in a Project starts at the draft default access, not at
        // the access a draft picked elsewhere.
        if project != nil, capabilities(preferred).accessChoices {
            accessMode = .workspace; claudeApproval = nil; planMode = nil
        }
    }

    /// Provider family is fixed once a thread exists, but a draft may change it.
    public mutating func chooseFamily(_ next: AgentFamily, capabilities: ProviderCapabilities) {
        guard !isSubmitted, family != next else { return }
        family = next; model = nil; effort = nil
        serviceTier = nil; claudeApproval = nil
        if !capabilities.accessChoices { accessMode = .workspace }
    }

    /// A draft with no destination or a project that is gone belongs in the
    /// first included project, keeping its words. A frozen project request is
    /// never moved: it must retry exactly what was committed.
    @discardableResult
    public mutating func settle(in projects: [ProjectSummary], capabilities: (AgentFamily) -> ProviderCapabilities) -> Bool {
        if case .project(let id) = destination, isSubmitted || projects.contains(where: { $0.id == id }) { return false }
        guard let fallback = projects.first else { return false }
        if isSubmitted { rejectSubmission() }
        destination = nil
        choose(.project(id: fallback.id), project: fallback, capabilities: capabilities)
        return true
    }

    /// The access settings this draft will create the thread with.
    public var access: ProjectAccess {
        get { ProjectAccess(accessMode: accessMode, claudeApproval: claudeApproval, planMode: planMode == true) }
        set { accessMode = newValue.accessMode; claudeApproval = newValue.claudeApproval; planMode = newValue.planMode ? true : nil }
    }

    /// Selecting a model starts from its own default reasoning effort.
    public mutating func chooseModel(_ option: AgentOptions.Model) {
        guard !isSubmitted else { return }
        model = option.id; effort = ModelDefaults.effort(for: option); serviceTier = nil
    }

    /// Keeps the draft on a model its provider offers, with an effort that
    /// model supports. `models` is the visible catalog for `family`. The order
    /// is the chat's own valid choice, then `remembered` (the Project's last
    /// model for `family`), then the computer's default.
    public mutating func ensureModel(among models: [AgentOptions.Model], remembered: RememberedModel? = nil) {
        guard !isSubmitted, !models.isEmpty else { return }
        if let current = models.first(where: { $0.id == model }) {
            let valid = ModelDefaults.effort(effort, for: current)
            if valid != effort { effort = valid }
            if let serviceTier,
               !(current.serviceTiers ?? []).contains(where: { $0.id == serviceTier }),
               current.defaultServiceTier != serviceTier { self.serviceTier = nil }
        } else if let remembered, let option = models.first(where: { $0.id == remembered.model }) {
            model = option.id; effort = ModelDefaults.effort(remembered.effort, for: option); serviceTier = nil
        } else if let option = ModelDefaults.defaultModel(in: models) {
            chooseModel(option)
        }
    }

    public mutating func freeze() {
        if submittedBody == nil { submittedBody = text }
    }

    public var attachmentCount: Int { (attachments?.count ?? 0) + (annotations?.count ?? 0) }

    public mutating func addAttachments(_ items: [NewChatAttachment]) throws {
        guard !isSubmitted, attachmentCount + items.count <= 4 else { throw FileFailure.tooLarge }
        attachments = (attachments ?? []) + items
    }

    public mutating func addAnnotation(_ pending: PendingNewChatAnnotation) throws {
        guard !isSubmitted, case .project(let projectID) = destination,
              projectID == pending.annotation.projectId,
              folderId == nil || folderId == pending.folderID,
              pending.draftID == (annotations?.first?.draftID ?? requestID),
              annotations?.first?.rootsRevision == nil || annotations?.first?.rootsRevision == pending.rootsRevision,
              attachmentCount < 4 else { throw FileFailure.integrity }
        folderId = pending.folderID
        annotations = (annotations ?? []) + [pending]
    }

    public mutating func removeAnnotation(id: String) {
        guard !isSubmitted else { return }
        annotations?.removeAll { $0.id == id }
    }

    public mutating func updateAnnotation(id: String, note: String) throws {
        guard !isSubmitted, var current = annotations,
              let index = current.firstIndex(where: { $0.id == id }) else { throw FileFailure.integrity }
        current[index].annotation = try current[index].annotation.replacingNote(note)
        annotations = current
    }

    public mutating func chooseFolder(_ id: String) {
        guard !isSubmitted, folderId != id else { return }
        folderId = id
        annotations = nil
    }

    public mutating func noteWorkspaceRevision(folderID: String, draftID: String,
                                               rootID: String, path: String, sha256: String) {
        guard !isSubmitted, var current = annotations else { return }
        for index in current.indices {
            guard current[index].folderID == folderID, current[index].draftID == draftID,
                  current[index].annotation.rootId == rootID,
                  current[index].annotation.path == path else { continue }
            current[index].sourceChanged = current[index].sourceChanged || current[index].annotation.sourceSha256 != sha256
        }
        annotations = current
    }

    /// Only a definitive rejection permits editing under a fresh request ID.
    public mutating func rejectSubmission() {
        submittedBody = nil; submittedDeviceID = nil; rootsRevision = nil
        preparedConversationID = nil; requestID = UUID().uuidString.lowercased()
    }

    /// After a confirmed creation, start a fresh draft for the same place.
    public mutating func completeSubmission() {
        text = ""; attachments = nil; annotations = nil; startsInT3Code = nil; t3LaunchUnconfirmed = nil; rejectSubmission()
    }

    /// Carry typed text to another Mac without its destinations or settings.
    /// A saved draft on the new Mac wins; its text is never overwritten.
    public static func switching(from current: NewChatDraft, toSaved saved: NewChatDraft?) -> NewChatDraft {
        if let saved, !saved.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || saved.isSubmitted || saved.attachmentCount > 0 { return saved }
        var next = saved ?? NewChatDraft()
        if !current.isSubmitted { next.text = current.text; next.attachments = current.attachments }
        return next
    }
}

/// The model and effort chosen last for a provider, offered to its next thread.
public struct RememberedModel: Codable, Hashable, Sendable {
    public let model: String
    public let effort: String?
    public init(model: String, effort: String?) { self.model = model; self.effort = effort }
}

/// Defaults shared by new chats and existing threads so the composer never
/// shows a model without its reasoning effort.
public enum ModelDefaults {
    /// The host publishes each model's resolved default effort. A Mac that
    /// predates host-resolved defaults may publish none, so keep its old rule:
    /// high, medium, or the first offered.
    public static func effort(for model: AgentOptions.Model) -> String? {
        let offered = model.reasoningEfforts.map(\.id)
        if let preferred = model.defaultReasoningEffort, offered.contains(preferred) { return preferred }
        return ["high", "medium"].first(where: offered.contains) ?? offered.first
    }

    /// `requested` when the model supports it, otherwise the model's default.
    public static func effort(_ requested: String?, for model: AgentOptions.Model) -> String? {
        if let requested, model.reasoningEfforts.contains(where: { $0.id == requested }) { return requested }
        return effort(for: model)
    }

    /// The model the host marked as its default for a provider; the first
    /// visible model only when talking to a host that does not mark one.
    public static func defaultModel(in models: [AgentOptions.Model]) -> AgentOptions.Model? {
        models.first { !$0.hidden && $0.isDefault == true } ?? models.first { !$0.hidden }
    }

    public static func title(of effort: AgentOptions.Choice) -> String {
        ["xhigh", "extra high"].contains(effort.label.lowercased()) || effort.id == "xhigh" ? "Extra high" : effort.label.capitalized
    }
}

public struct NewChatAttachment: Codable, Hashable, Identifiable, Sendable {
    public let id: String
    public let name: String
    public let mimeType: String
    public let byteSize: Int
    public let sha256: String
    public init(_ file: StagedFile) {
        id = file.id; name = file.name; mimeType = file.mimeType
        byteSize = file.data.count; sha256 = ConversationFile.digest(file.data)
    }
}

/// A preview note in a New Chat draft has no provider conversation yet. The
/// immutable source binding and stable upload ID survive creation retries.
public struct PendingNewChatAnnotation: Codable, Hashable, Identifiable, Sendable {
    public let id: String
    public let draftID: String
    public let folderID: String
    public let rootsRevision: Int
    public var annotation: ArtifactAnnotation
    public var sourceChanged: Bool

    public init(annotation: ArtifactAnnotation, draftID: String, folderID: String,
                rootsRevision: Int) throws {
        try annotation.validate()
        guard !draftID.isEmpty, !folderID.isEmpty, rootsRevision > 0,
              annotation.conversationId == "new-chat:" + draftID else { throw FileFailure.integrity }
        id = UUID().uuidString
        self.draftID = draftID; self.folderID = folderID; self.rootsRevision = rootsRevision
        self.annotation = annotation; sourceChanged = false
    }

    public func file(conversationID: String, projectID: String, folderID: String,
                     draftID: String, rootsRevision: Int) throws -> StagedFile {
        guard UUID(uuidString: id) != nil, !sourceChanged,
              self.draftID == draftID, self.folderID == folderID,
              self.rootsRevision == rootsRevision,
              annotation.projectId == projectID,
              annotation.conversationId == "new-chat:" + draftID,
              !conversationID.hasPrefix("new-chat:") else { throw FileFailure.integrity }
        let bound = try annotation.bound(to: conversationID)
        let file = try bound.stagedFile()
        return try StagedFile(id: id, name: file.name, mimeType: file.mimeType, data: file.data)
    }
}

// MARK: - Sidebar presentation

/// Why a Mac's rows cannot be trusted right now.
public enum SidebarHostNotice: Hashable, Sendable {
    /// The pairing ended; saved projects stay readable.
    case pairAgain
    /// Cupola stopped on the computer (or isn't responding) while its recovery
    /// answers: Restart brings it back.
    case rescue(RescueNotice)
    /// This device could not keep the Mac's chats; Try again saves them again.
    case storageFailed(message: String)
    /// The Mac answered but its Projects could not be read; cached ones stay.
    case projectsFailed(message: String, isRetrying: Bool)
}

public enum SidebarRow: Hashable, Identifiable, Sendable {
    case pinnedHeader
    /// Closes the Pinned threads, so the Projects after them do not read as pinned.
    case projectsHeader
    /// A pinned thread from any Mac; `hostName` is set only when several Macs are paired.
    case pinned(hostID: String, projectID: String, projectName: String, hostName: String?, thread: ProjectThreadSummary, isSelected: Bool)
    case hostNotice(hostID: String, name: String, kind: SidebarHostNotice)
    case project(hostID: String, project: ProjectSummary, isExpanded: Bool, isSelected: Bool)
    case thread(hostID: String, projectID: String, thread: ProjectThreadSummary, isSelected: Bool)
    case threadsLoading(hostID: String, projectID: String)
    case threadsNotice(hostID: String, projectID: String, message: String, canRetry: Bool, isRetrying: Bool = false)
    case moreThreads(hostID: String, projectID: String, isLoading: Bool)
    /// Scheduled tasks of every computer: one row, after the notices.
    case automations
    /// All chats: one thread of any Project on this Mac, newest first, named
    /// with its Project.
    case chat(hostID: String, projectID: String, projectName: String, thread: ProjectThreadSummary, isSelected: Bool)
    /// All chats: the Mac's first pages are still arriving.
    case chatsLoading(hostID: String)
    /// All chats: one Project's threads could not be read; the rest still show.
    case chatsNotice(hostID: String, projectID: String, message: String, canRetry: Bool, isRetrying: Bool = false)
    /// All chats: older threads exist in at least one Project on this Mac.
    case moreChats(hostID: String, isLoading: Bool)

    /// A native session keeps its identity across pinning, layout, attachment,
    /// paging and streaming. Its Mac scopes otherwise identical provider IDs.
    public var id: String {
        switch self {
        case .pinnedHeader: return "pinned-header"
        case .projectsHeader: return "projects-header"
        case .pinned(let host, _, _, _, let thread, _),
             .thread(let host, _, let thread, _),
             .chat(let host, _, _, let thread, _): return "thread:\(host):\(thread.reference)"
        case .hostNotice(let host, _, _): return "host-notice:\(host)"
        case .project(let host, let project, _, _): return "project:\(host):\(project.id)"
        case .threadsLoading(let host, let project): return "loading:\(host):\(project)"
        case .threadsNotice(let host, let project, _, _, _): return "notice:\(host):\(project)"
        case .moreThreads(let host, let project, _): return "more:\(host):\(project)"
        case .automations: return "automations"
        case .chatsLoading(let host): return "chats-loading:\(host)"
        case .chatsNotice(let host, let project, _, _, _): return "chats-notice:\(host):\(project)"
        case .moreChats(let host, _): return "more-chats:\(host)"
        }
    }

    /// A project or thread the owner can open, as opposed to headers and notices.
    public var isResult: Bool {
        switch self {
        case .pinned, .project, .thread, .chat: return true
        default: return false
        }
    }
}

/// Loaded thread pages for one expanded project, in server order.
public struct ProjectThreadsState: Hashable, Sendable {
    public var threads: [ProjectThreadSummary] = []
    public var nextCursor: String?
    /// Any read of this Project's threads, including a background refresh.
    public var isLoading = false
    /// Only an explicit Show more: the row spins for the page it asked for,
    /// never for a refresh of rows already shown.
    public var isLoadingMore = false
    public var failure: String?
    public var partial: [ProjectPartialFailure] = []
    public var hasLoaded = false
    /// The Mac's last answer was still reading a provider (`ProjectThreadsPage.loading`).
    public var isStillReading = false
    public init() {}
    /// Rows from the offline cache, shown until the Mac answers.
    public init(saved rows: [ProjectThreadSummary]) { threads = rows.map(\.saved) }

    /// New folders restart paging: the rows already shown stay (as saved
    /// copies) until the first page replaces them, and so does the bound a
    /// further page set, so All Chats neither reveals rows it held back nor
    /// hides them again when the page arrives.
    public func restartingPaging() -> ProjectThreadsState {
        var next = ProjectThreadsState(saved: threads)
        next.nextCursor = nextCursor
        return next
    }

    /// The Mac has answered but sent no rows yet because a provider is still
    /// reading. The list waits for rows instead of offering Show more or
    /// saying there are none.
    public var awaitsFirstRows: Bool {
        hasLoaded && isStillReading && threads.isEmpty && failure == nil
    }

    /// The most threads the Mac returns in one page.
    public static let maxPage = 30
    /// A refresh re-reads every row already shown (up to one full page), so
    /// pages added with Show more stay put instead of collapsing to the first.
    /// Pinned rows lead the first page outside the Mac's limit, so they don't count.
    public var refreshLimit: Int {
        min(max(SidebarProjection.initialThreads, threads.count { !$0.isPinned }), Self.maxPage)
    }

    /// Paging appends without reordering earlier rows or duplicating a thread
    /// that moved between pages.
    public mutating func apply(_ page: ProjectThreadsPage, replacing: Bool) {
        // A provider that did not fully answer keeps its saved rows the page
        // lacks, by recency. The page's own copies are newer evidence. A page
        // the Mac sent while still reading may lack any provider's rows, so it
        // keeps every row it lacks: a refresh never blanks the list.
        var kept: [ProjectThreadSummary] = []
        if replacing {
            let unavailable = Set(page.partial.map(\.family))
            kept = threads.filter { page.loading || unavailable.contains($0.family) }.map(\.saved)
            threads = []
        }
        var seen = Set(threads.map(\.reference))
        var conversations = Set(threads.compactMap(\.conversationId))
        for thread in page.threads where seen.insert(thread.reference).inserted {
            if let id = thread.conversationId, !conversations.insert(id).inserted { continue }
            threads.append(thread)
        }
        for thread in kept where seen.insert(thread.reference).inserted {
            if let id = thread.conversationId, !conversations.insert(id).inserted { continue }
            let index = threads.firstIndex { !$0.isPinned && $0.updatedAt < thread.updatedAt } ?? threads.endIndex
            threads.insert(thread, at: index)
        }
        nextCursor = page.nextCursor
        partial = page.partial
        failure = nil
        hasLoaded = true
        isStillReading = page.loading
    }
}

public struct SidebarHostProjects: Sendable {
    /// Library activity refresh failures; these do not describe missing Project pages.
    public let partial: [ProjectPartialFailure]
    public let hostID: String
    public let name: String
    public let isOnline: Bool
    public let supportsProjects: Bool?
    public let projects: [ProjectSummary]
    public let threads: [String: ProjectThreadsState]
    /// Pinned threads the Mac reported; empty for older Macs, whose loaded pages still show pins.
    public let pinned: [PinnedProjectThread]
    public let notice: SidebarHostNotice?
    /// Providers the Mac reports it cannot run, so retrying their threads cannot help.
    public let unavailableFamilies: Set<AgentFamily>
    public init(hostID: String, name: String, isOnline: Bool, supportsProjects: Bool?,
                projects: [ProjectSummary], threads: [String: ProjectThreadsState],
                pinned: [PinnedProjectThread] = [], notice: SidebarHostNotice? = nil,
                unavailableFamilies: Set<AgentFamily> = [], partial: [ProjectPartialFailure] = []) {
        self.partial = partial
        self.hostID = hostID; self.name = name; self.isOnline = isOnline
        self.supportsProjects = supportsProjects; self.projects = projects; self.threads = threads
        self.pinned = pinned; self.notice = notice; self.unavailableFamilies = unavailableFamilies
    }
}

/// A Chats row's second line in reading order: Project, branch or worktree,
/// then what the thread is doing now.
public struct ThreadRowDetail: Equatable, Sendable {
    public let project: String?
    public let location: String?
    public let status: String?
    public init(project: String?, location: String?, status: String?) {
        self.project = project; self.location = location; self.status = status
    }
}

extension ProjectThreadSummary {
    /// A detail refresh changes saved presentation fields, preserving the
    /// catalog's provider evidence, work state and stable identity.
    public func updatingDetails(title: String, isPinned: Bool, hasUnread: Bool) -> ProjectThreadSummary {
        ProjectThreadSummary(reference: reference, conversationId: conversationId, title: title, family: family,
                             updatedAt: updatedAt, isPinned: isPinned, hasUnread: hasUnread, isWorking: isWorking, currentActivity: currentActivity,
                             needsInputOnMac: needsInputOnMac, waitingOnMacPermission: waitingOnMacPermission, isComputerSession: isComputerSession, worktreeLabel: worktreeLabel,
                             backgroundWork: backgroundWork, branch: branch, automationRun: automationRun, resumesAt: resumesAt, isSubagent: isSubagent)
    }
    /// A copy the Mac did not just report: its work state is unknown, so it
    /// shows no spinner or step. Finished workers otherwise spin for hours.
    public var saved: ProjectThreadSummary {
        ProjectThreadSummary(reference: reference, conversationId: conversationId, title: title, family: family,
                             updatedAt: updatedAt, isPinned: isPinned, hasUnread: hasUnread, isWorking: false, currentActivity: nil,
                             needsInputOnMac: needsInputOnMac, waitingOnMacPermission: waitingOnMacPermission, isComputerSession: isComputerSession, worktreeLabel: worktreeLabel, branch: branch,
                             automationRun: automationRun, resumesAt: resumesAt, isSubagent: isSubagent)
    }
    /// Line 2 of its Chats row, after the provider icon. `project` is nil inside
    /// the thread's own Project group, where it is redundant; it is never
    /// replaced by the status. A chat-specific Mac permission wait is written
    /// out; other input waits use a dot. The current step shows only while the
    /// Mac is connected.
    public func rowDetail(project: String?, waitingLabel: String?, macConnected: Bool, now: Date = Date()) -> ThreadRowDetail {
        let status = waitingLabel == nil ? (macConnected ? currentStep : nil) : waitsOnMacPermission ? waitingLabel : nil
        // Snoozed until the usage limit resets: the row says when it continues.
        return ThreadRowDetail(project: project, location: locationLabel,
                               status: status ?? (waitingLabel == nil && !isRunning ? resumeLabel(now: now) : nil)
                                ?? (isSubagent == true ? Self.subagentLabel : nil))
    }
    /// "Resumes at 3:40 PM" while a resume is scheduled in the future.
    public func resumeLabel(now: Date = Date()) -> String? {
        guard let at = UsageResume.date(resumesAt), at > now else { return nil }
        return UsageResume.label(at, now: now)
    }
    /// The same thread with a different pin state.
    public func settingPinned(_ value: Bool) -> ProjectThreadSummary {
        updatingDetails(title: title, isPinned: value, hasUnread: hasUnread)
    }
    /// The same thread with a confirmed read status.
    public func settingUnread(_ value: Bool) -> ProjectThreadSummary {
        updatingDetails(title: title, isPinned: isPinned, hasUnread: value)
    }
}

/// The most recently opened project conversations, newest first. Their saved
/// history stays on the phone after the conversation list prunes the rest.
public struct RecentlyOpenedConversations: Codable, Equatable, Sendable {
    public static let limit = 30
    public private(set) var ids: [String]
    public init(ids: [String] = []) { self.ids = Array(ids.prefix(Self.limit)) }
    public mutating func note(_ id: String) {
        guard ids.first != id else { return }
        ids.removeAll { $0 == id }
        ids.insert(id, at: 0)
        if ids.count > Self.limit { ids.removeLast(ids.count - Self.limit) }
    }
}

/// How the sidebar arranges each Mac's threads.
public enum SidebarLayout: String, CaseIterable, Identifiable, Sendable {
    /// Threads under their Project folders.
    case byProject = "projects"
    /// Every Project's threads in one list per Mac, newest first.
    case allChats = "chats"
    public var id: String { rawValue }
    public var title: String {
        switch self {
        case .byProject: "By Project"
        case .allChats: "All Chats"
        }
    }
}

/// Phone-local identity, independent of list grouping and connection lifetime.
public struct SidebarOrderID: Codable, Hashable, Sendable {
    public let host: String
    public let item: String
    public init(host: String, item: String) { self.host = host; self.item = item }
}

/// One small order document. Missing rows keep their slots: a filtered, paged
/// or disconnected catalog is not evidence of deletion.
public struct SidebarOrder: Codable, Equatable, Sendable {
    public enum Section: String, Codable, CaseIterable, Sendable { case pinned, projects, chats }
    private var sections: [String: [SidebarOrderID]] = [:]
    private var threadAliases: [SidebarOrderID: SidebarOrderID] = [:]
    private var manualSections: Set<Section> = []
    private var projectThreads: [SidebarOrderID: Set<SidebarOrderID>] = [:]
    private enum CodingKeys: String, CodingKey { case sections, threadAliases, manualSections, projectThreads }
    public init() {}
    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        manualSections = try values.decodeIfPresent(Set<Section>.self, forKey: .manualSections) ?? []
        projectThreads = try values.decodeIfPresent([SidebarOrderID: Set<SidebarOrderID>].self, forKey: .projectThreads) ?? [:]
        sections = try values.decodeIfPresent([String: [SidebarOrderID]].self, forKey: .sections) ?? [:]
        threadAliases = try values.decodeIfPresent([SidebarOrderID: SidebarOrderID].self, forKey: .threadAliases) ?? [:]
        threadAliases = threadAliases.filter { $0.key.host == $0.value.host }
        // Pre-launch documents recorded recency without any manual-intent marker.
        // They are layout preferences only; do not silently freeze activity with them.
        sections = sections.filter { key, _ in Section(rawValue: key).map { manualSections.contains($0) } ?? false }
        if manualSections.isEmpty { threadAliases = [:]; projectThreads = [:] }
        for section in Section.allCases where sections[section.rawValue] != nil {
            var seen = Set<SidebarOrderID>()
            sections[section.rawValue] = ids(in: section).map { normalized($0, in: section) }.filter { seen.insert($0).inserted }
        }
    }
    public func ids(in section: Section) -> [SidebarOrderID] { sections[section.rawValue] ?? [] }

    public func isManual(_ section: Section) -> Bool { manualSections.contains(section) }

    /// Only a complete successful Project listing proves that a saved row is gone.
    /// Partial/paged responses accumulate membership across refreshes and launches.
    public mutating func reconcileThreads(host: String, project: String, threads: [ProjectThreadSummary], complete: Bool) {
        let key = SidebarOrderID(host: host, item: project)
        for thread in threads { recordThread(host: host, thread: thread) }
        let live = Set(threads.map { normalized(SidebarOrderID(host: host, item: $0.reference), in: .chats) })
        let previous = Set((projectThreads[key] ?? []).map { normalized($0, in: .chats) })
        projectThreads[key] = complete ? live : previous.union(live)
        if complete {
            let elsewhere = Set(projectThreads.filter { $0.key != key }.values.flatMap { $0 }.map { normalized($0, in: .chats) })
            let removed = previous.subtracting(live).subtracting(elsewhere)
            remove(removed, from: .chats)
            remove(removed, from: .pinned)
            threadAliases = threadAliases.filter { !removed.contains($0.value) }
        }
    }

    private func normalized(_ id: SidebarOrderID, in section: Section) -> SidebarOrderID {
        section == .projects ? id : threadAliases[id] ?? id
    }

    /// A provider reference can arrive before attachment, or replace a prepared
    /// `wonder:` reference later. Keep both on the stable conversation's slot.
    @discardableResult public mutating func recordThread(host: String, thread: ProjectThreadSummary) -> Bool {
        guard let conversation = thread.conversationId else { return false }
        let reference = SidebarOrderID(host: host, item: thread.reference)
        let canonical = SidebarOrderID(host: host, item: "conversation:" + conversation)
        guard threadAliases[reference] != canonical else { return false }
        let previous = normalized(reference, in: .chats)
        threadAliases[reference] = canonical
        for section in [Section.pinned, .chats] where sections[section.rawValue] != nil {
            var seen = Set<SidebarOrderID>()
            sections[section.rawValue] = ids(in: section).map { $0 == previous ? canonical : $0 }.filter { seen.insert($0).inserted }
        }
        return true
    }

    /// New default-leading rows go first; older rows discovered by paging go
    /// last. Compare each computer independently so another computer's clock
    /// cannot hide a new leading row. Activity never undoes a manual arrangement.
    @discardableResult public mutating func observe(_ ids: [SidebarOrderID], in section: Section) -> Bool {
        let previous = self.ids(in: section)
        let saved = Set(previous)
        var seen = saved
        var leading: [SidebarOrderID] = [], older: [SidebarOrderID] = []
        var passedKnownHosts = Set<String>()
        for rawID in ids {
            let id = normalized(rawID, in: section)
            if saved.contains(id) { passedKnownHosts.insert(id.host) }
            guard seen.insert(id).inserted else { continue }
            if passedKnownHosts.contains(id.host) { older.append(id) } else { leading.append(id) }
        }
        guard !leading.isEmpty || !older.isEmpty else { return false }
        sections[section.rawValue] = leading + previous + older
        return true
    }

    public func sorted<Value>(_ values: [Value], in section: Section, id: (Value) -> SidebarOrderID) -> [Value] {
        let saved = ids(in: section)
        guard isManual(section), values.count > 1, !saved.isEmpty else { return values }
        let ranks = Dictionary(saved.enumerated().map { ($0.element, $0.offset) }, uniquingKeysWith: min)
        return values.enumerated().sorted { left, right in
            let a = ranks[normalized(id(left.element), in: section)], b = ranks[normalized(id(right.element), in: section)]
            if let a, let b { return a == b ? left.offset < right.offset : a < b }
            if a != nil || b != nil { return a == nil }
            return left.offset < right.offset
        }.map(\.element)
    }

    /// Replaces only the visible slots, preserving a hidden computer's position.
    /// Unknown/missing IDs and cross-section requests are harmless no-ops.
    @discardableResult public mutating func move(_ source: SidebarOrderID, relativeTo target: SidebarOrderID,
                                                after: Bool, visible: [SidebarOrderID], in section: Section) -> Bool {
        let sourceID = normalized(source, in: section), targetID = normalized(target, in: section)
        let visibleIDs = visible.map { normalized($0, in: section) }
        guard sourceID != targetID, Set(visibleIDs).count == visibleIDs.count,
              visibleIDs.contains(sourceID), visibleIDs.contains(targetID) else { return false }
        let previousVisible = sorted(visibleIDs, in: section, id: { $0 })
        var ordered = previousVisible
        ordered.removeAll { $0 == sourceID }
        guard let index = ordered.firstIndex(of: targetID) else { return false }
        ordered.insert(sourceID, at: index + (after ? 1 : 0))
        guard ordered != previousVisible else { return false }
        var next = self
        next.manualSections.insert(section)
        next.observe(visibleIDs, in: section)
        let visibleSet = Set(visibleIDs)
        var cursor = 0
        next.sections[section.rawValue] = next.ids(in: section).map { item in
            guard visibleSet.contains(item) else { return item }
            defer { cursor += 1 }
            return ordered[cursor]
        }
        guard next != self else { return false }
        self = next
        return true
    }

    public mutating func reconcilePinned(host: String, threads: [ProjectThreadSummary]) {
        for thread in threads { recordThread(host: host, thread: thread) }
        let live = Set(threads.map { normalized(SidebarOrderID(host: host, item: $0.reference), in: .pinned) })
        remove(Set(ids(in: .pinned).filter { $0.host == host && !live.contains($0) }), from: .pinned)
    }

    public mutating func removeProjects(host: String, ids: Set<String>) {
        for project in ids {
            reconcileThreads(host: host, project: project, threads: [], complete: true)
            projectThreads.removeValue(forKey: SidebarOrderID(host: host, item: project))
        }
        remove(Set(ids.map { SidebarOrderID(host: host, item: $0) }), from: .projects)
    }

    /// Call only for confirmed deletion, never for an absent page or offline Mac.
    @discardableResult public mutating func remove(_ ids: Set<SidebarOrderID>, from section: Section) -> Bool {
        let previous = self.ids(in: section)
        let normalizedIDs = Set(ids.map { normalized($0, in: section) })
        let next = previous.filter { !normalizedIDs.contains($0) }
        guard next != previous else { return false }
        sections[section.rawValue] = next
        return true
    }
}

extension SidebarRow {
    public var orderItem: SidebarOrderID? {
        switch self {
        case .pinned(let host, _, _, _, let thread, _), .thread(let host, _, let thread, _),
             .chat(let host, _, _, let thread, _): SidebarOrderID(host: host, item: thread.reference)
        case .project(let host, let project, _, _): SidebarOrderID(host: host, item: project.id)
        default: nil
        }
    }
    public var orderSection: SidebarOrder.Section? {
        switch self {
        case .pinned: .pinned
        case .project: .projects
        case .thread, .chat: .chats
        default: nil
        }
    }
    /// Current visible boundaries only. The saved order has no grouping keys,
    /// so merged lists can reuse it without a migration.
    public var orderGroup: String? {
        switch self {
        case .pinned: "pinned"
        case .project: "projects"
        case .thread(let host, let project, _, _): "threads:" + host + ":" + project
        case .chat: "chats"
        default: nil
        }
    }
}

public enum SidebarProjection {
    public static let initialThreads = 5

    /// Translate a native List insertion offset into a move inside the source's
    /// displayed section. A Project's children travel with its header on rebuild.
    public static func moveTarget(rows: [SidebarRow], source: Int, destination: Int)
        -> (source: SidebarOrderID, target: SidebarRow, after: Bool)? {
        guard rows.indices.contains(source), (0...rows.count).contains(destination),
              let id = rows[source].orderItem, let group = rows[source].orderGroup else { return nil }
        let all = rows.indices.filter { rows[$0].orderGroup == group }
        guard let first = all.first, let last = all.last else { return nil }
        var end = last + 1
        // The final Project can be expanded. Its complete subtree is a legal
        // insertion boundary; crossing its computer header is not.
        if case .project(let host, let project, _, _) = rows[last] {
            while end < rows.count {
                let child: Bool
                switch rows[end] {
                case .thread(let h, let p, _, _), .threadsLoading(let h, let p),
                     .threadsNotice(let h, let p, _, _, _), .moreThreads(let h, let p, _):
                    child = h == host && p == project.id
                default: child = false
                }
                if !child { break }
                end += 1
            }
        }
        guard destination >= first, destination <= end else { return nil }
        let peers = all.filter { $0 != source }
        if let next = peers.first(where: { $0 >= destination }) {
            return (id, rows[next], false)
        }
        if let previous = peers.last(where: { $0 < destination }) {
            return (id, rows[previous], true)
        }
        return nil
    }

    /// The whole sidebar as one flat array: Pinned first, then every Mac's
    /// projects merged into one list. Views render rows; they never nest a lazy list per project.
    public static func rows(hosts: [SidebarHostProjects], expanded: Set<String>,
                            selectedHost: String? = nil, selectedConversation: String?, search: String = "",
                            filter: SidebarSessionFilter = .all, needsInput: Set<String> = [],
                            layout: SidebarLayout = .byProject, order: SidebarOrder = SidebarOrder()) -> [SidebarRow] {
        let pinned = pinnedRows(hosts: hosts, selectedHost: selectedHost, selectedConversation: selectedConversation, search: search, filter: filter, needsInput: needsInput, order: order)
        let rest = layout == .allChats
            ? chatRows(hosts: hosts, selectedHost: selectedHost,
                       selectedConversation: selectedConversation, search: search, filter: filter, needsInput: needsInput, order: order)
            : projectRows(hosts: hosts, expanded: expanded, selectedHost: selectedHost,
                          selectedConversation: selectedConversation, selectedProject: nil, search: search, filter: filter, needsInput: needsInput, order: order)
        // Computer notices and Automations lead, above the Pinned heading, so they never read as pinned.
        let leading = rest.prefix { if case .hostNotice = $0 { return true } else { return $0 == .automations } }
        let remainder = rest.dropFirst(leading.count)
        let closesPinned = !pinned.isEmpty && layout == .byProject && remainder.contains { if case .project = $0 { return true } else { return false } }
        return Array(leading) + pinned + (closesPinned ? [.projectsHeader] : []) + remainder
    }

    /// Pinned threads of one Mac's included projects: what the Mac reported,
    /// plus pinned threads already loaded (older Macs, or a pin not yet
    /// reported), most recent activity first.
    public static func pinnedThreads(_ host: SidebarHostProjects) -> [PinnedProjectThread] {
        let included = Set(host.projects.filter(\.isIncluded).map(\.id))
        var references = Set<String>(), conversations = Set<String>()
        var entries: [PinnedProjectThread] = []
        func add(_ entry: PinnedProjectThread) {
            guard included.contains(entry.projectId), references.insert(entry.thread.reference).inserted else { return }
            if let id = entry.thread.conversationId, !conversations.insert(id).inserted { return }
            entries.append(entry)
        }
        host.pinned.forEach(add)
        for project in host.projects where included.contains(project.id) {
            for thread in host.threads[project.id]?.threads ?? [] where thread.isPinned {
                add(PinnedProjectThread(projectId: project.id, thread: thread))
            }
        }
        return entries.enumerated().sorted { lhs, rhs in
            if lhs.element.thread.updatedAt != rhs.element.thread.updatedAt { return lhs.element.thread.updatedAt > rhs.element.thread.updatedAt }
            return lhs.offset < rhs.offset
        }.map(\.element)
    }

    /// The Pinned section across every visible Mac. Empty when nothing is pinned.
    public static func pinnedRows(hosts: [SidebarHostProjects], selectedHost: String? = nil, selectedConversation: String?,
                                  search: String = "", filter: SidebarSessionFilter = .all, needsInput: Set<String> = [], order: SidebarOrder = SidebarOrder()) -> [SidebarRow] {
        let query = search.trimmingCharacters(in: .whitespacesAndNewlines)
        var entries: [(host: SidebarHostProjects, entry: PinnedProjectThread, projectName: String)] = []
        for host in hosts {
            for entry in pinnedThreads(host) {
                guard filter.includes(entry.thread, needsInput: needsInput.contains(SidebarSessionFilter.attentionKey(host: host.hostID, thread: entry.thread.reference))) else { continue }
                guard let project = host.projects.first(where: { $0.id == entry.projectId }) else { continue }
                if !query.isEmpty && !matches(entry.thread.title, query) && !matches(project.name, query) { continue }
                entries.append((host, entry, project.name))
            }
        }
        guard !entries.isEmpty else { return [] }
        let ordered = entries.enumerated().sorted { lhs, rhs in
            let left = lhs.element.entry.thread.updatedAt, right = rhs.element.entry.thread.updatedAt
            return left != right ? left > right : lhs.offset < rhs.offset
        }.map(\.element)
        return [.pinnedHeader] + order.sorted(ordered, in: .pinned, id: { SidebarOrderID(host: $0.host.hostID, item: $0.entry.thread.reference) }).map { item in
            .pinned(hostID: item.host.hostID, projectID: item.entry.projectId, projectName: item.projectName,
                    hostName: hosts.count > 1 ? item.host.name : nil, thread: item.entry.thread,
                    isSelected: isSelected(item.entry.thread, host: item.host.hostID, selectedHost: selectedHost, conversation: selectedConversation))
        }
    }

    private static func isSelected(_ thread: ProjectThreadSummary, host: String, selectedHost: String?, conversation: String?) -> Bool {
        guard let id = thread.conversationId, id == conversation else { return false }
        return selectedHost == nil || selectedHost == host
    }

    /// Every computer's Projects as one list: a computer's own order is kept,
    /// and the computers interleave by recent use (pinned Projects first). One
    /// line per computer that needs attention leads, then each computer's
    /// Automations; a search shows every match.
    public static func projectRows(hosts: [SidebarHostProjects], expanded: Set<String>,
                                   selectedHost: String? = nil, selectedConversation: String?,
                                   selectedProject: String?, search: String = "",
                                   filter: SidebarSessionFilter = .all, needsInput: Set<String> = [], order: SidebarOrder = SidebarOrder()) -> [SidebarRow] {
        let query = search.trimmingCharacters(in: .whitespacesAndNewlines)
        var rows = leadingRows(hosts: hosts, filter: filter, query: query)
        let visible = order.sorted(mergedProjects(hosts), in: .projects, id: { SidebarOrderID(host: $0.host.hostID, item: $0.project.id) })
        // Pinned threads live in the Pinned section, not again under their project.
        let pinnedReferences = Dictionary(hosts.map { ($0.hostID, Set(pinnedThreads($0).map(\.thread.reference))) }, uniquingKeysWith: { $1 })
        var shownReferences = Set<String>()
        for (host, project) in visible {
            let state = host.threads[project.id] ?? ProjectThreadsState()
            let key = expansionKey(host: host.hostID, project: project.id)
            let nameMatches = query.isEmpty || matches(project.name, query)
            let unpinned = state.threads.filter {
                !(pinnedReferences[host.hostID] ?? []).contains($0.reference) && filter.includes($0, needsInput: needsInput.contains(
                    SidebarSessionFilter.attentionKey(host: host.hostID, thread: $0.reference)))
            }
            let matchingThreads = nameMatches ? unpinned : unpinned.filter { matches($0.title, query) }
            if filter == .all && !query.isEmpty && !nameMatches && matchingThreads.isEmpty { continue }
            // Keep unresolved pages reachable: an empty first page does not mean no matches.
            if filter != .all && matchingThreads.isEmpty && state.hasLoaded && !state.isLoading
                && state.nextCursor == nil && state.failure == nil && state.partial.isEmpty { continue }
            // Search temporarily expands matches without changing saved expansion.
            let isExpanded = filter != .all || expanded.contains(key) || (!nameMatches && !matchingThreads.isEmpty)
            rows.append(.project(hostID: host.hostID, project: project, isExpanded: isExpanded,
                                 isSelected: selectedProject == project.id && selectedConversation == nil))
            guard isExpanded else { continue }
            for thread in order.sorted(matchingThreads, in: .chats, id: { SidebarOrderID(host: host.hostID, item: $0.reference) })
            where shownReferences.insert(host.hostID + "\n" + thread.reference).inserted {
                rows.append(.thread(hostID: host.hostID, projectID: project.id, thread: thread,
                                    isSelected: isSelected(thread, host: host.hostID, selectedHost: selectedHost, conversation: selectedConversation)))
            }
            if !host.isOnline && !state.hasLoaded {
                // A computer that is off says so with its red dot alone: no line, no spinner, no retry.
            } else if (state.isLoading && state.threads.isEmpty && !state.hasLoaded) || state.awaitsFirstRows
                        || (filter != .all && !state.hasLoaded && state.failure == nil) {
                rows.append(.threadsLoading(hostID: host.hostID, projectID: project.id))
            } else if let failure = state.failure, host.isOnline {
                rows.append(.threadsNotice(hostID: host.hostID, projectID: project.id, message: failure, canRetry: true, isRetrying: state.isLoading))
            } else if host.isOnline, let partial = state.partial.first(where: { !host.unavailableFamilies.contains($0.family) }) {
                // A provider the Mac cannot run has no recovery from here, so
                // only a provider that failed to answer offers Retry.
                rows.append(.threadsNotice(hostID: host.hostID, projectID: project.id,
                                           message: "\(partial.family.title) didn’t respond on \(host.name), so some threads may be missing.",
                                           canRetry: true, isRetrying: state.isLoading))
            } else if filter != .all && matchingThreads.isEmpty && state.nextCursor != nil {
                rows.append(.threadsNotice(hostID: host.hostID, projectID: project.id, message: "No matches in loaded chats", canRetry: false))
            } else if state.hasLoaded && state.threads.isEmpty && nameMatches {
                rows.append(.threadsNotice(hostID: host.hostID, projectID: project.id, message: "No threads yet", canRetry: false))
            }
            if (nameMatches || filter != .all), state.nextCursor != nil, !state.awaitsFirstRows {
                rows.append(.moreThreads(hostID: host.hostID, projectID: project.id, isLoading: state.isLoadingMore))
            }
        }
        return rows
    }

    /// The lines above the merged list: what needs attention on each computer
    /// (offline, pair again), then the one Automations row.
    private static func leadingRows(hosts: [SidebarHostProjects], filter: SidebarSessionFilter, query: String) -> [SidebarRow] {
        var rows: [SidebarRow] = hosts.compactMap { host in
            host.notice.map { .hostNotice(hostID: host.hostID, name: host.name, kind: $0) }
        }
        if filter == .all, query.isEmpty {
            if hosts.contains(where: { $0.supportsProjects == true || !$0.projects.isEmpty }) { rows.append(.automations) }
        }
        return rows
    }

    /// Included Projects of every computer: each computer keeps its own order
    /// (pinned first) and the lists interleave, pinned then most recently used.
    static func mergedProjects(_ hosts: [SidebarHostProjects]) -> [(host: SidebarHostProjects, project: ProjectSummary)] {
        var lists = hosts.map { host in sorted(host.projects.filter(\.isIncluded)).map { (host: host, project: $0) } }
        func precedes(_ a: ProjectSummary, _ b: ProjectSummary) -> Bool {
            a.isPinned != b.isPinned ? a.isPinned : (a.lastUsedAt ?? a.createdAt) > (b.lastUsedAt ?? b.createdAt)
        }
        var merged: [(host: SidebarHostProjects, project: ProjectSummary)] = []
        while true {
            var best: Int?
            for index in lists.indices where !lists[index].isEmpty {
                if let current = best, !precedes(lists[index][0].project, lists[current][0].project) { continue }
                best = index
            }
            guard let best else { return merged }
            merged.append(lists[best].removeFirst())
        }
    }

    /// All chats across computers: the threads of every included Project
    /// merged newest first. Each Project's unread pages can only hold threads
    /// older than its last loaded one, so a row shows only once no unread page
    /// could sort above it; Show more reads the page that blocks the next rows.
    /// A Project still waiting for its first rows holds nothing back.
    public static func chatRows(hosts: [SidebarHostProjects],
                                selectedHost: String? = nil, selectedConversation: String?, search: String = "",
                                filter: SidebarSessionFilter = .all, needsInput: Set<String> = [], order: SidebarOrder = SidebarOrder()) -> [SidebarRow] {
        let query = search.trimmingCharacters(in: .whitespacesAndNewlines)
        var rows = leadingRows(hosts: hosts, filter: filter, query: query)
        var ready: [(host: SidebarHostProjects, project: ProjectSummary, thread: ProjectThreadSummary)] = []
        var notices: [SidebarRow] = []
        var waiting = false, loadingMore = false
        var blockingHost: String?, blockingAt: Int?
        // One cutoff for every computer: a row shows only once no unread page
        // on any computer could sort above it.
        var cutoff = Int.min
        for host in hosts {
            let merge = chatMerge(host)
            let pinnedReferences = Set(pinnedThreads(host).map(\.thread.reference))
            if let frontier = merge.frontier { cutoff = max(cutoff, frontier) }
            ready += merge.candidates.filter { !pinnedReferences.contains($0.thread.reference) }.map { (host, $0.project, $0.thread) }
            for notice in merge.notices {
                notices.append(.chatsNotice(hostID: host.hostID, projectID: notice.project.id,
                                            message: "\(notice.project.name): \(notice.message)", canRetry: notice.canRetry, isRetrying: notice.isRetrying))
            }
            if merge.waitingForFirstPages { waiting = true }
            if merge.loadingMore { loadingMore = true }
            if let at = merge.blockingAt, blockingAt.map({ at > $0 }) ?? true { blockingHost = host.hostID; blockingAt = at }
        }
        ready = ready.filter { $0.thread.updatedAt >= cutoff }
        // Computers interleave by recency under the one shared cutoff above.
        let merged = ready.enumerated().sorted { lhs, rhs in
            let left = lhs.element.thread.updatedAt, right = rhs.element.thread.updatedAt
            return left != right ? left > right : lhs.offset < rhs.offset
        }.map(\.element)
        var shown = 0
        for item in order.sorted(merged, in: .chats, id: { SidebarOrderID(host: $0.host.hostID, item: $0.thread.reference) }) {
            guard filter.includes(item.thread, needsInput: needsInput.contains(
                SidebarSessionFilter.attentionKey(host: item.host.hostID, thread: item.thread.reference))) else { continue }
            if !query.isEmpty && !matches(item.thread.title, query) && !matches(item.project.name, query) { continue }
            rows.append(.chat(hostID: item.host.hostID, projectID: item.project.id, projectName: item.project.name, thread: item.thread,
                              isSelected: isSelected(item.thread, host: item.host.hostID, selectedHost: selectedHost, conversation: selectedConversation)))
            shown += 1
        }
        rows += notices
        // A Project still waiting for its first rows adds a spinner after
        // the rows that are ready, or stands alone when there are none.
        if waiting, let first = hosts.first { rows.append(.chatsLoading(hostID: first.hostID)) }
        if let blockingHost {
            rows.append(.moreChats(hostID: blockingHost, isLoading: loadingMore))
        } else if shown == 0 && notices.isEmpty && !waiting && filter == .all && query.isEmpty, let first = hosts.first {
            rows.append(.chatsNotice(hostID: first.hostID, projectID: "", message: "No chats yet", canRetry: false))
        }
        return rows
    }

    /// The Project whose next page Show more reads in All chats: the one whose
    /// oldest loaded thread is newest, since it holds back every older row.
    public static func nextChatsProject(_ host: SidebarHostProjects) -> String? { chatMerge(host).blockingProject }

    /// Included Projects whose first page All chats still needs.
    public static func chatsProjectsToLoad(_ host: SidebarHostProjects) -> [String] {
        host.projects.filter(\.isIncluded).compactMap { project in
            let state = host.threads[project.id]
            if let state, state.hasLoaded || state.isLoading || state.failure != nil { return nil }
            return project.id
        }
    }

    private struct ChatMerge {
        /// Every loaded thread in Project order, before any paging cutoff.
        var candidates: [(project: ProjectSummary, thread: ProjectThreadSummary)] = []
        /// The newest point below which this computer may still hold unread threads.
        var frontier: Int?
        var notices: [(project: ProjectSummary, message: String, canRetry: Bool, isRetrying: Bool)] = []
        var blockingProject: String?
        var blockingAt: Int?
        var waitingForFirstPages = false
        var loadingMore = false
    }

    private static func chatMerge(_ host: SidebarHostProjects) -> ChatMerge {
        var merge = ChatMerge()
        var candidates: [(offset: Int, project: ProjectSummary, thread: ProjectThreadSummary)] = []
        // The newest point below which some Project may still hold unread threads.
        var frontier: Int?
        var blocking: (project: String, at: Int)?
        var seen = Set<String>()
        for project in sorted(host.projects.filter(\.isIncluded)) {
            let state = host.threads[project.id] ?? ProjectThreadsState()
            for thread in state.threads where seen.insert(thread.reference).inserted {
                candidates.append((candidates.count, project, thread))
            }
            if state.isLoadingMore { merge.loadingMore = true }
            if let failure = state.failure {
                // Offline, the red dot is the only notice; saved rows stay.
                if host.isOnline { merge.notices.append((project, failure, true, state.isLoading)) }
                continue
            }
            // Retry reloads this Project's pages, so only their failures belong here.
            if host.isOnline, let partial = state.partial.first(where: { !host.unavailableFamilies.contains($0.family) }) {
                merge.notices.append((project, "\(partial.family.title) didn’t respond, so some chats may be missing.", true, state.isLoading))
            }
            // Saved rows show while offline.
            if !state.hasLoaded && !host.isOnline { continue }
            // A Project with no rows yet, unanswered or still being read, never
            // withholds the others' rows (the list would blank on every slow
            // read); its rows join in place when they arrive.
            if state.threads.isEmpty && (!state.hasLoaded || state.awaitsFirstRows) {
                if host.isOnline { merge.waitingForFirstPages = true }
                continue
            }
            // Only a known further page bounds the list. Saved rows the Mac
            // has not answered for yet show as they are; restarted paging
            // keeps its bound (`restartingPaging`).
            guard state.nextCursor != nil else { continue }
            let oldest = state.threads.map(\.updatedAt).min() ?? Int.max
            frontier = max(frontier ?? oldest, oldest)
            if blocking.map({ oldest > $0.at }) ?? true { blocking = (project.id, oldest) }
        }
        merge.blockingProject = blocking?.project
        merge.blockingAt = blocking?.at
        merge.frontier = frontier
        merge.candidates = candidates.map { ($0.project, $0.thread) }
        return merge
    }

    public static func expansionKey(host: String, project: String) -> String { host + "/" + project }

    /// Pinned projects in pin order first, then recently used.
    public static func sorted(_ projects: [ProjectSummary]) -> [ProjectSummary] {
        projects.enumerated().sorted { lhs, rhs in
            if lhs.element.isPinned != rhs.element.isPinned { return lhs.element.isPinned }
            return lhs.offset < rhs.offset
        }.map(\.element)
    }

    private static func matches(_ text: String, _ query: String) -> Bool {
        text.range(of: query, options: [.caseInsensitive, .diacriticInsensitive]) != nil
    }
}
