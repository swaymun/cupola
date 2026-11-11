import Foundation

/// Everything the composer's Git pill knows about a conversation's code: the
/// files the thread itself changed, the latest reply's edits, the Project
/// folder's branch, and its pull requests. One value decides whether the pill
/// shows, what it says (branch, the thread's "+12 −3" or, from an older Mac,
/// the checkout's uncommitted counts, commits ahead, worktree, pull request
/// state), whether it carries a dot, which tab Code changes opens on and what
/// VoiceOver reads.
public struct CodeChanges: Equatable, Sendable {
    public enum Tab: String, CaseIterable, Identifiable, Sendable {
        case thread, pullRequests, lastReply, uncommitted, branch
        public var id: Self { self }
        public var title: String {
            switch self {
            case .thread: "This chat"
            case .pullRequests: "PRs"
            case .lastReply: "Last reply"
            case .uncommitted: "Uncommitted"
            case .branch: "Branch"
            }
        }
        public var scope: WorkspaceChangeScope? {
            switch self {
            case .thread, .pullRequests, .lastReply: nil
            case .uncommitted: .uncommitted
            case .branch: .branch
            }
        }
    }

    /// The latest reply that edited files, counted for the pill and sheet.
    public struct ReplyEdits: Equatable, Sendable {
        public var files: Int
        public var additions: Int?
        public var deletions: Int?
        public init(files: Int, additions: Int?, deletions: Int?) {
            self.files = files; self.additions = additions; self.deletions = deletions
        }
        public init(_ edits: ResponseEditedFiles) {
            files = edits.files.count
            additions = edits.files.allSatisfy { $0.additions != nil } ? edits.files.reduce(0) { $0 + ($1.additions ?? 0) } : nil
            deletions = edits.files.allSatisfy { $0.deletions != nil } ? edits.files.reduce(0) { $0 + ($1.deletions ?? 0) } : nil
        }
    }

    /// Present only for a folder in a Git repository the Mac could read.
    public let git: WorkspaceGitSummary?
    /// Why the Mac gave no branch for the thread's folder (outside Git, a folder
    /// it would not read); nil when there is a branch or nothing was read yet.
    public let noGitInfoReason: String?
    public let pullRequests: [ThreadPullRequests.PullRequest]
    /// Every pull request the thread opened; `pullRequests` lists the newest.
    public let pullRequestTotal: Int
    public let replyEdits: ReplyEdits?
    /// The conversation's last finished reply changed files.
    public let lastReplyChanged: Bool
    /// Every repository the thread worked with, when there is more than one
    /// and the Mac can say (older Macs cannot; their single layout stays).
    public let repositories: [ThreadRepositories.Repository]
    /// `repositories` with the worktrees folded under them, in the Mac's order.
    public let checkouts: [ThreadRepositories.Repository]
    /// The thread's only repository, when the index names exactly one; its
    /// Git actions sit under the branch row.
    public let primaryRepository: ThreadRepositories.Repository?
    /// The Mac's GitHub CLI is signed in, so pull request actions can run.
    public let githubAvailable: Bool
    /// The files this thread itself changed in its one repository, as the Mac
    /// last counted them; nil from a Mac that can't tell, outside Git, or when
    /// the thread worked in several repositories (each offers its own).
    public let thread: ThreadChangeCount?
    /// The Mac lists a thread's own changes, so each repository of a thread
    /// that worked in several offers This thread too.
    public let threadSupported: Bool

    public init(git: WorkspaceGitSummary?, pullRequests: ThreadPullRequests?, replyEdits: ReplyEdits?, lastReplyChanged: Bool,
                repositories: ThreadRepositories? = nil, thread: ThreadChangeCount? = nil) {
        self.repositories = repositories.flatMap { $0.showsSections ? $0.repositories : nil } ?? []
        self.checkouts = self.repositories.flatMap(\.checkouts)
        self.primaryRepository = repositories.flatMap {
            $0.version == ThreadRepositories.supportedVersion && $0.repositories.count == 1 && $0.repositories[0].worktrees.isEmpty
                ? $0.repositories.first : nil
        }
        self.githubAvailable = repositories?.githubAvailable ?? false
        self.git = git.flatMap { $0.showsChip ? $0 : nil }
        self.noGitInfoReason = self.repositories.isEmpty ? git?.noGitInfoReason : nil
        self.pullRequests = pullRequests.flatMap { $0.showsPill ? $0.pullRequests : nil } ?? []
        self.pullRequestTotal = self.pullRequests.isEmpty ? 0 : max(pullRequests?.total ?? 0, self.pullRequests.count)
        self.replyEdits = replyEdits.flatMap { $0.files > 0 ? $0 : nil }
        self.lastReplyChanged = self.replyEdits != nil && lastReplyChanged
        self.thread = self.git != nil && self.repositories.isEmpty ? thread : nil
        self.threadSupported = thread != nil
    }

    /// Shown when there is a branch, edits, pull requests or repositories to
    /// show. A folder without Git info and nothing else hides the pill; when
    /// the pill shows for edits, Code changes still says why there is no branch.
    public var isVisible: Bool {
        git != nil || replyEdits != nil || !pullRequests.isEmpty || !repositories.isEmpty
    }

    /// The pill's name: the branch's last part, the repository count, or nil
    /// (the pill then shows only its counts).
    public var pillTitle: String? {
        if repositories.count > 1 { return "\(repositories.count) repos" }
        // One repository with worktrees: the branch, else its name.
        if let repository = repositories.first { return git?.shortTitle ?? repository.name }
        return git?.shortTitle
    }

    /// The pill's added and removed lines: the thread's own changes when the
    /// Mac counts them (never the checkout's, which other threads and apps
    /// share); from an older Mac, uncommitted work, else the last reply's
    /// edits right after it made them.
    public var pillTotals: (additions: Int, deletions: Int)? {
        if let thread {
            return thread.available && thread.totals.files > 0 ? (thread.totals.additions, thread.totals.deletions) : nil
        }
        if repositories.isEmpty, let totals = git?.pillTotals { return (totals.additions, totals.deletions) }
        if lastReplyChanged, let edits = replyEdits, let additions = edits.additions, let deletions = edits.deletions,
           additions + deletions > 0 {
            return (additions, deletions)
        }
        return nil
    }

    /// The pill's "↑3" (the branch's commits its base lacks).
    public var pillAhead: Int? { repositories.isEmpty ? git?.pillAhead : nil }

    /// The pill marks a thread working in a linked worktree.
    public var pillWorktree: Bool { repositories.isEmpty && git?.worktree == true }

    /// The state the pill's colour follows, across the pull requests the thread
    /// opened or reviews in any of its repositories: an open one (a draft
    /// counts) wins, else the most recently updated merged or closed one.
    /// GitHub's UTC timestamps order as text; a missing one counts as oldest,
    /// and ties keep the Mac's newest-first order.
    public var pillPullRequestState: ThreadPullRequests.PullRequest.State? {
        let candidates = pullRequests.map { ($0.state, $0.updatedAt) }
            + (checkouts + [primaryRepository].compactMap { $0 }).flatMap(\.pullRequests)
                .compactMap { pr in pr.state.map { ($0, pr.updatedAt) } }
        func newest(_ states: Set<ThreadPullRequests.PullRequest.State>) -> ThreadPullRequests.PullRequest.State? {
            candidates.filter { states.contains($0.0) }
                .reduce(nil) { best, next in best.map { (next.1 ?? "") > ($0.1 ?? "") ? next : $0 } ?? next }?.0
        }
        return newest([.open, .draft]) ?? newest([.merged, .closed])
    }

    /// This thread leads when the Mac counts it, listing the thread's pull
    /// requests above its files; otherwise PRs lead when the thread has pull
    /// requests. Uncommitted and Branch (the whole checkout) need a
    /// repository; Last reply is always offered.
    public var tabs: [Tab] {
        if thread != nil { return [.thread, .lastReply, .uncommitted, .branch] }
        return (pullRequests.isEmpty ? [] : [.pullRequests]) + (git == nil ? [.lastReply] : [.lastReply, .uncommitted, .branch])
    }

    /// This thread whenever the Mac counts it. From an older Mac: Last reply
    /// right after a turn that edited files, else the thread's pull requests,
    /// else Uncommitted when there is uncommitted work, else Branch.
    public var defaultTab: Tab {
        if thread != nil { return .thread }
        if lastReplyChanged { return .lastReply }
        if !pullRequests.isEmpty { return .pullRequests }
        guard let git else { return .lastReply }
        if let uncommitted = git.uncommitted, uncommitted.files > 0 { return .uncommitted }
        return .branch
    }

    /// What VoiceOver reads for the pill.
    public var accessibilityLabel: String {
        var parts = ["Code changes"]
        if repositories.count > 1 { parts.append("\(repositories.count) repositories") }
        if repositories.count == 1 {
            let worktrees = checkouts.count - 1
            parts.append("\(repositories[0].name) with \(worktrees) \(worktrees == 1 ? "worktree" : "worktrees")")
        }
        if let git {
            // The pill shows the thread's counts or the checkout's, never both.
            parts.append(git.accessibilityText(uncommittedWork: thread == nil))
            if let ahead = git.pillAhead { parts.append("\(ahead) \(ahead == 1 ? "commit" : "commits") ahead") }
        }
        if let thread, thread.available {
            let totals = thread.totals
            parts.append(totals.files == 0 ? "this chat changed no files"
                : "this chat changed \(totals.files) \(totals.files == 1 ? "file" : "files"), "
                    + "\(totals.additions) added lines, \(totals.deletions) removed lines")
        }
        if let edits = replyEdits {
            let noun = edits.files == 1 ? "file" : "files"
            var text = (lastReplyChanged ? "last reply changed " : "an earlier reply changed ") + "\(edits.files) \(noun)"
            if let additions = edits.additions, let deletions = edits.deletions {
                text += ", \(additions) added lines, \(deletions) removed lines"
            }
            parts.append(text)
        }
        if pullRequestTotal == 1, let pr = pullRequests.first {
            parts.append("pull request \(pr.number) \(pr.state.title.lowercased())" + Self.checksText(pr.checks.state))
        } else if pullRequestTotal > 1 {
            parts.append("\(pullRequestTotal) pull requests" + Self.checksText(openChecks))
        }
        return parts.joined(separator: ", ")
    }

    /// Said under the list when it keeps only the newest of the thread's pull
    /// requests, or nil when it shows them all.
    public var pullRequestLimitNote: String? {
        guard pullRequestTotal > pullRequests.count else { return nil }
        return "This chat opened \(pullRequestTotal.formatted()) pull requests. The newest \(pullRequests.count.formatted()) are shown here; the rest are on GitHub."
    }

    /// The checks that need attention most, among pull requests still open.
    public var openChecks: ThreadPullRequests.PullRequest.Checks.State {
        let open = pullRequests.filter { $0.state == .open || $0.state == .draft }.map(\.checks.state)
        if open.contains(.failing) { return .failing }
        if open.contains(.pending) { return .pending }
        if open.contains(.unknown) { return .unknown }
        return open.contains(.passing) ? .passing : .none
    }

    private static func checksText(_ state: ThreadPullRequests.PullRequest.Checks.State) -> String {
        switch state {
        case .failing: ", checks failing"
        case .pending: ", checks running"
        case .passing: ", checks passed"
        case .none: ""
        case .unknown: ", check results unavailable"
        }
    }
}
