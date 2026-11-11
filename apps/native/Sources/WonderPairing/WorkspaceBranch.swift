import Foundation

/// The branch a Project thread's folder is on, as the Mac reads it with Git
/// (read only). `repository` is false for a folder outside Git.
public struct WorkspaceGitSummary: Decodable, Equatable, Sendable {
    public struct Totals: Decodable, Equatable, Sendable {
        public var files: Int
        public var additions: Int
        public var deletions: Int
        public init(files: Int, additions: Int, deletions: Int) {
            self.files = files; self.additions = additions; self.deletions = deletions
        }
    }

    public var available: Bool
    public var detail: String?
    public var repository: Bool
    public var branch: String?
    public var detached: Bool
    public var head: String?
    public var worktree: Bool
    public var baseRef: String?
    public var mergeBase: String?
    public var ahead: Int?
    public var behind: Int?
    public var uncommitted: Totals?
    public var branchChanges: Totals?

    public init(available: Bool = true, detail: String? = nil, repository: Bool, branch: String?, detached: Bool = false,
                head: String? = nil, worktree: Bool = false, baseRef: String? = nil, mergeBase: String? = nil,
                ahead: Int? = nil, behind: Int? = nil, uncommitted: Totals? = nil, branchChanges: Totals? = nil) {
        self.available = available; self.detail = detail; self.repository = repository; self.branch = branch
        self.detached = detached; self.head = head; self.worktree = worktree; self.baseRef = baseRef
        self.mergeBase = mergeBase; self.ahead = ahead; self.behind = behind
        self.uncommitted = uncommitted; self.branchChanges = branchChanges
    }

    /// The chip shows only for a folder in a repository Git could read.
    public var showsChip: Bool { available && repository && (branch != nil || detached) }

    /// A summary for a folder the Mac would not read, carrying why.
    public static func unreadable(_ reason: String) -> Self {
        WorkspaceGitSummary(available: false, detail: reason, repository: false, branch: nil)
    }

    /// Why there is no branch to show, in the Mac's words when it gave some;
    /// nil when the chip shows.
    public var noGitInfoReason: String? {
        guard !showsChip else { return nil }
        if !available {
            let detail = detail?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            return detail.isEmpty ? "Your computer couldn't read Git for this chat's folder." : detail
        }
        if !repository { return "This chat's folder isn't in a Git repository." }
        return "Git didn't report a branch for this chat's folder."
    }

    /// Why a refused summary read has no branch, for a Mac that gave no words.
    public static func refusalReason(status: Int) -> String {
        switch status {
        case 403: "This chat's folder isn't one of the Project's folders on your computer."
        case 404, 501: "Your computer can't show Git info for this chat. Updating Cupola on your computer may help."
        default: "Your computer couldn't read Git for this chat's folder."
        }
    }

    /// The pill's "+12 −3": uncommitted work when there is some.
    public var pillTotals: Totals? {
        uncommitted.flatMap { $0.files > 0 ? $0 : nil }
    }

    /// Commits the branch has that its base does not, when there are any.
    public var pillAhead: Int? { ahead.flatMap { $0 > 0 ? $0 : nil } }

    public var title: String { branch ?? "Detached" }

    /// The pill's name: the last part of a slash-separated branch name
    /// ("agent/branch-chip" → "branch-chip"); the sheet and VoiceOver say it all.
    public var shortTitle: String {
        guard let branch else { return "Detached" }
        let name = branch.split(separator: "/").last.map(String.init) ?? branch
        // Shortened here rather than by layout, so the pill hugs its text.
        guard name.count > 20 else { return name }
        return String(name.prefix(11)) + "…" + String(name.suffix(8))
    }

    /// "feature → main", or the branch alone when there is no base.
    public var comparisonTitle: String {
        guard let baseRef else { return title }
        return "\(title) → \(baseRef)"
    }

    /// "3 commits ahead, 1 behind" (nil without a base).
    public var aheadBehindText: String? {
        guard let ahead, let behind else { return nil }
        if ahead == 0 && behind == 0 { return "Up to date with \(baseRef ?? "its base")" }
        var parts: [String] = []
        if ahead > 0 { parts.append("\(ahead) \(ahead == 1 ? "commit" : "commits") ahead") }
        if behind > 0 { parts.append("\(behind) behind") }
        return parts.joined(separator: ", ")
    }

    /// What VoiceOver reads for the chip.
    public var accessibilityText: String { accessibilityText(uncommittedWork: true) }

    /// The chip's words, without the checkout's uncommitted counts when the
    /// pill shows this thread's own counts instead.
    public func accessibilityText(uncommittedWork: Bool) -> String {
        var parts = [detached ? "Detached, not on a branch" : "Branch \(title)"]
        if worktree { parts.append("worktree") }
        guard uncommittedWork else { return parts.joined(separator: ", ") }
        if let totals = uncommitted, totals.files > 0 {
            parts.append("\(totals.files) uncommitted \(totals.files == 1 ? "file" : "files")")
            parts.append("\(totals.additions) added lines, \(totals.deletions) removed lines")
        } else if uncommitted != nil {
            parts.append("no uncommitted changes")
        }
        return parts.joined(separator: ", ")
    }
}

public enum WorkspaceChangeScope: String, Codable, CaseIterable, Sendable {
    case uncommitted, branch
}

/// The files this thread itself changed ("This thread", `scope` "thread"), or
/// everything in the checkout since the last commit or the branch's base
/// (Uncommitted, Branch).
public struct WorkspaceGitChanges: Decodable, Equatable, Sendable {
    public struct File: Decodable, Equatable, Identifiable, Sendable {
        public var path: String
        public var originalPath: String?
        public var status: String
        public var binary: Bool
        public var additions: Int?
        public var deletions: Int?
        /// This thread's hunks for the file, turn by turn ("This thread" only);
        /// nil for a binary file or one too large to send.
        public var diff: String?
        public var id: String { path }

        public init(path: String, originalPath: String? = nil, status: String, binary: Bool = false,
                    additions: Int? = nil, deletions: Int? = nil, diff: String? = nil) {
            self.path = path; self.originalPath = originalPath; self.status = status; self.binary = binary
            self.additions = additions; self.deletions = deletions; self.diff = diff
        }

        public var statusTitle: String {
            switch status {
            case "added": "Added"
            case "deleted": "Deleted"
            case "renamed": "Renamed"
            case "untracked": "New"
            default: "Modified"
            }
        }
        public var symbol: String {
            switch status {
            case "added", "untracked": "plus.circle"
            case "deleted": "minus.circle"
            case "renamed": "arrow.right.circle"
            default: "pencil"
            }
        }
        public var name: String { (path as NSString).lastPathComponent }
        public var folder: String? {
            let folder = (path as NSString).deletingLastPathComponent
            return folder.isEmpty ? nil : folder
        }
        public var accessibilityText: String {
            var parts = [path, statusTitle]
            if let originalPath { parts.append("from \(originalPath)") }
            if binary { parts.append("binary file") }
            if let additions { parts.append("\(additions) added lines") }
            if let deletions, status != "untracked" { parts.append("\(deletions) removed lines") }
            return parts.joined(separator: ", ")
        }
    }

    public var scope: String
    public var available: Bool
    public var detail: String?
    public var branch: String?
    public var baseRef: String?
    public var files: [File]
    public var totals: WorkspaceGitSummary.Totals
    public var truncated: Bool

    public init(scope: String, available: Bool = true, detail: String? = nil, branch: String? = nil, baseRef: String? = nil,
                files: [File], totals: WorkspaceGitSummary.Totals, truncated: Bool = false) {
        self.scope = scope; self.available = available; self.detail = detail; self.branch = branch
        self.baseRef = baseRef; self.files = files; self.totals = totals; self.truncated = truncated
    }
}

/// What the pill keeps of "This thread" between reads: whether the Mac could
/// read the thread's own changes, and how many there are.
public struct ThreadChangeCount: Equatable, Sendable {
    public var available: Bool
    public var totals: WorkspaceGitSummary.Totals

    public init(available: Bool = true, totals: WorkspaceGitSummary.Totals) {
        self.available = available; self.totals = totals
    }

    public init(_ changes: WorkspaceGitChanges) {
        self.init(available: changes.available, totals: changes.totals)
    }
}
