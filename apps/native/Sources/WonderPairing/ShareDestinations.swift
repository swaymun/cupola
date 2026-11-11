import Foundation

/// A Project thread the share sheet can send to, including a native provider
/// thread that needs attaching before its first Wonder message.
public struct ShareTarget: Identifiable, Hashable, Sendable {
    public let id: String
    public let projectID: String
    public let reference: String
    public let title: String
    public let conversationID: String?
    public let family: AgentFamily
    public let projectName: String
    public let updatedAt: Date
    public let isWorking: Bool
    public let isPinned: Bool

    public init(project: ProjectSummary, thread: ProjectThreadSummary) {
        id = "thread:" + thread.reference
        projectID = project.id; reference = thread.reference
        title = thread.title; conversationID = thread.conversationId; family = thread.family
        projectName = project.name; updatedAt = Date(timeIntervalSince1970: TimeInterval(thread.updatedAt))
        isWorking = thread.isWorking; isPinned = thread.isPinned
    }

    /// Mirrors the app's attach rule: a catalog thread without a conversation
    /// needs one, and a native session whose provider syncs desktop projects is
    /// reattached so a missing desktop Project assignment is repaired.
    /// Attaching starts no model work.
    public func needsAttach(_ capabilities: ProviderCapabilities) -> Bool {
        conversationID == nil || (capabilities.desktopProjectSync && !reference.hasPrefix("wonder:"))
    }

    public func matches(_ query: String) -> Bool {
        let query = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !query.isEmpty else { return true }
        return [title, projectName].contains {
            $0.range(of: query, options: [.caseInsensitive, .diacriticInsensitive]) != nil
        }
    }
}

public struct ShareTargetSection: Identifiable, Hashable, Sendable {
    public let id: String
    public let title: String
    public let targets: [ShareTarget]
}

public enum ShareCatalog {
    public static let recentLimit = 5

    /// Project threads from pinned rows and each Project's first page, one row
    /// per provider thread; a later copy of the same thread replaces an earlier.
    public static func threads(projects: [ProjectSummary], pinned: [PinnedProjectThread],
                               pages: [String: [ProjectThreadSummary]]) -> [ShareTarget] {
        let byID = Dictionary(projects.filter(\.isIncluded).map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        var result: [String: ShareTarget] = [:]
        for row in pinned { if let project = byID[row.projectId] { insert(ShareTarget(project: project, thread: row.thread), into: &result) } }
        for (projectID, rows) in pages {
            guard let project = byID[projectID] else { continue }
            for thread in rows { insert(ShareTarget(project: project, thread: thread), into: &result) }
        }
        return Array(result.values)
    }

    private static func insert(_ target: ShareTarget, into result: inout [String: ShareTarget]) {
        result[target.id] = target
    }

    /// Recent first, then each Project in the app's order. While
    /// searching there is no Recent section, so every match shows once under
    /// its Project.
    public static func sections(threads: [ShareTarget], projects: [ProjectSummary],
                                query: String = "", recentLimit: Int = recentLimit) -> [ShareTargetSection] {
        let newestFirst: (ShareTarget, ShareTarget) -> Bool = {
            let first = $0.updatedAt, second = $1.updatedAt
            return first != second ? first > second : $0.title.localizedStandardCompare($1.title) == .orderedAscending
        }
        let searching = !query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let visibleThreads = threads.filter { $0.matches(query) }.sorted(by: newestFirst)
        var sections: [ShareTargetSection] = []
        var shown = Set<String>()
        if !searching {
            let recent = Array(visibleThreads.prefix(recentLimit))
            if !recent.isEmpty { sections.append(ShareTargetSection(id: "recent", title: "Recent", targets: recent)) }
            shown = Set(recent.map(\.id))
        }
        for project in projects where project.isIncluded {
            let rows = visibleThreads.filter { $0.projectID == project.id && !shown.contains($0.id) }
            if !rows.isEmpty { sections.append(ShareTargetSection(id: "project:" + project.id, title: project.name, targets: rows)) }
        }
        return sections
    }
}

/// What the share sheet sends: the user's note above what was shared.
public enum ShareComposition {
    public static let maximumBodyBytes = 65536
    public static let maximumFiles = 4
    /// The Keychain item through which the app tells the share extension its
    /// theme; the extension cannot read the app's preferences.
    public static let themeKeychainAccount = "share-theme-v1"

    public static func body(note: String, shared: [String]) -> String {
        ([note.trimmingCharacters(in: .whitespacesAndNewlines)] + shared.map { $0.trimmingCharacters(in: .whitespacesAndNewlines) })
            .filter { !$0.isEmpty }.joined(separator: "\n\n")
    }

    /// The request that creates a Project thread for the first message, as New
    /// Chat sends it: prepare only, in the Project's primary folder. The same
    /// `requestID` retries the same creation instead of starting another.
    public static func newThreadBody(project: ProjectSummary, family: AgentFamily, capabilities: ProviderCapabilities,
                                     model: AgentOptions.Model, supportsModes: Bool, deviceID: String, requestID: String,
                                     body: String) throws -> Data {
        var fields: [String: Any] = ["deviceId": deviceID, "clientMessageId": requestID, "family": family.rawValue,
                                     "model": model.id, "accessMode": ProjectAccessMode.workspace.rawValue, "body": body,
                                     "prepareOnly": true, "rootsRevision": project.rootsRevision]
        if let effort = ModelDefaults.effort(for: model) { fields["effort"] = effort }
        if supportsModes, capabilities.allowsApprovalChoice { fields["claudeApproval"] = ClaudeApproval.standard.rawValue }
        if let folder = project.primaryFolder?.id { fields["folderId"] = folder }
        return try JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys])
    }

    /// The models a new thread can use for `family`, default first.
    public static func models(_ options: [AgentOptions.Model], family: AgentFamily) -> [AgentOptions.Model] {
        let visible = options.filter { $0.family == family && !$0.hidden }
        guard let preferred = ModelDefaults.defaultModel(in: visible) else { return visible }
        return [preferred] + visible.filter { $0.id != preferred.id }
    }
}
