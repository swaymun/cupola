import Foundation

/// What a chat's pull requests pill and list show, prepared once from the
/// Mac's last answer, the last read's failure and whether the chat is a
/// Project thread. Not knowing yet, couldn't check, GitHub signed out on the
/// Mac and "no pull requests" are separate states: only a known answer with
/// something to show makes the pill appear, so it never flashes or says zero
/// when the count is unknown.
public struct ConversationPullRequests: Equatable, Sendable {
    public enum Status: Equatable, Sendable {
        /// Not read yet, or a Mac without pull request reads: nothing shows.
        case unknown
        /// The first read failed; the message says to try again.
        case couldNotCheck(String)
        /// The Mac answered in a newer form than this app reads.
        case unsupported
        /// The Mac can't ask GitHub: its GitHub CLI is missing or signed out.
        case unavailable(String)
        /// The Mac answered; `items` holds what it found, possibly nothing.
        case ready
    }

    public let status: Status
    /// The listed pull requests, open and draft first as the Mac orders them.
    public let items: [ThreadPullRequests.PullRequest]
    /// The open and draft ones.
    public let active: [ThreadPullRequests.PullRequest]
    /// A refresh failed after an earlier answer; the list is the earlier one.
    public let staleMessage: String?
    /// The folder is on a pushed branch that has no open pull request yet.
    public let canCreate: Bool
    /// The branch a new pull request would come from.
    public let branchName: String?
    /// Open and draft pull requests; nil at zero, so the badge is left out.
    public let badgeCount: Int?
    /// The thread opened more than the Mac read, so there may be more open ones.
    public let badgeIsLowerBound: Bool
    /// An open pull request has failing checks, requested changes or conflicts.
    public let needsAttention: Bool
    /// The thread opened more pull requests than are listed.
    public let limitNote: String?

    public init(isProject: Bool, list: ThreadPullRequests?, error: String?) {
        let status: Status
        if !isProject {
            status = .unknown
        } else if let list {
            if list.version != ThreadPullRequests.supportedVersion {
                status = .unsupported
            } else if !list.available {
                status = .unavailable(list.detail ?? "Sign in to GitHub on your computer to see pull requests.")
            } else {
                status = .ready
            }
        } else if let error {
            status = .couldNotCheck(error)
        } else {
            status = .unknown
        }
        self.status = status
        guard status == .ready, let list else {
            items = []; active = []; staleMessage = nil; canCreate = false; branchName = nil
            badgeCount = nil; badgeIsLowerBound = false; needsAttention = false; limitNote = nil
            return
        }
        items = list.pullRequests
        active = list.pullRequests.filter(\.isActive)
        staleMessage = error
        if let branch = list.branch, branch.pushed, branch.pullRequest == nil,
           !active.contains(where: { $0.headRef == branch.name }) {
            canCreate = true; branchName = branch.name
        } else {
            canCreate = false; branchName = list.branch?.name
        }
        let open = list.counts.open + list.counts.draft
        badgeCount = open > 0 ? open : nil
        badgeIsLowerBound = !list.complete
        needsAttention = active.contains(where: \.needsAttention)
        limitNote = list.total > list.pullRequests.count
            ? "Showing the newest \(list.pullRequests.count.formatted()) of \(list.total.formatted()) pull requests this chat opened. The rest are on GitHub."
            : nil
    }

    /// Whether the Mac answered and the thread has pull requests or one can
    /// be opened from its branch. Not on every Project thread, and never
    /// while the answer is unknown or GitHub is signed out.
    public var isVisible: Bool { status == .ready && (!items.isEmpty || canCreate) }

    /// Whether the chat's title menu offers Pull requests: the Mac answered
    /// something worth reading, such as the list, how to sign in or that the
    /// read failed, so it can be retried from there.
    public var opensFromMenu: Bool { status != .unknown }

    /// Why the list is empty or unavailable, in the person's words.
    public var statusMessage: String? {
        switch status {
        case .unknown: return nil
        case .couldNotCheck(let message), .unavailable(let message): return message
        case .unsupported: return "Update Cupola to see this chat’s pull requests."
        case .ready:
            if let staleMessage { return staleMessage }
            return items.isEmpty && !canCreate ? "This chat hasn’t opened a pull request." : nil
        }
    }

    /// What went wrong reading pull requests, for the list's error row with
    /// Try again: couldn't check, signed out, an answer this app can't read,
    /// or a failed refresh, including one after an empty answer. Nil for a
    /// plain "none yet" or before the computer answered.
    public var problemMessage: String? {
        switch status {
        case .unknown: nil
        case .ready: staleMessage
        case .couldNotCheck, .unavailable, .unsupported: statusMessage
        }
    }

    /// Try again helps: the read failed, or the Mac's GitHub sign-in may have
    /// been fixed since.
    public var offersRetry: Bool {
        switch status {
        case .couldNotCheck, .unavailable: true
        case .ready: staleMessage != nil
        case .unknown, .unsupported: false
        }
    }

    /// "Pull requests, 2 open, needs attention".
    public var accessibilityLabel: String {
        var parts = ["Pull requests"]
        if let badgeCount {
            parts.append((badgeIsLowerBound ? "at least " : "") + "\(badgeCount) open")
        } else if badgeIsLowerBound {
            parts.append("no open ones among the newest")
        } else if !items.isEmpty {
            parts.append("none open")
        } else if canCreate {
            parts.append("none yet")
        }
        if active.contains(where: { $0.checks.state == .failing }) { parts.append("checks failing") }
        if active.contains(where: { $0.details?.reviewDecision == "CHANGES_REQUESTED" }) { parts.append("changes requested") }
        if active.contains(where: { $0.details?.mergeable == "CONFLICTING" || $0.details?.mergeStateStatus == "DIRTY" }) {
            parts.append("merge conflicts")
        }
        return parts.joined(separator: ", ")
    }
}
