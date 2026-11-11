import Combine
import Foundation

/// A Git action the Mac runs with its own git and gh sign-in on a repository
/// the thread worked with: commit, push, pull (fast-forward only), open a pull
/// request, merge it or turn auto-merge on or off. Only a repository wholly
/// inside a Project folder is offered; the Mac checks again before running.
public enum GitAction: String, Codable, Sendable, CaseIterable {
    case commit, push, pull, createPullRequest, merge, autoMerge
}

public enum MergeMethod: String, Codable, Sendable, CaseIterable, Identifiable {
    case squash, merge, rebase
    public var id: String { rawValue }
    public var title: String {
        switch self {
        case .squash: "Squash and merge"
        case .merge: "Create a merge commit"
        case .rebase: "Rebase and merge"
        }
    }
}

public struct GitActionRequest: Encodable, Equatable, Sendable {
    public var repositoryId: String
    public var root: String
    public var repo: String
    public var action: GitAction
    public var message: String?
    public var includeUnstaged: Bool?
    public var confirmDefaultBranch: Bool?
    public var title: String?
    public var body: String?
    public var base: String?
    public var draft: Bool?
    public var pullRequestUrl: String?
    public var method: MergeMethod?
    public var enabled: Bool?
    /// Merge or turn auto-merge on only while the pull request's head is still
    /// this commit; the Mac refuses with `headChanged` otherwise.
    public var expectedHeadOid: String?

    /// Nil for a repository Wonder keeps read only.
    public init?(repository: ThreadRepositories.Repository, action: GitAction) {
        guard repository.actionsAvailable, let location = repository.location else { return nil }
        repositoryId = repository.id; root = location.rootId; repo = location.path; self.action = action
    }

    public static func commit(_ repository: ThreadRepositories.Repository, message: String, includeUnstaged: Bool) -> Self? {
        var request = Self(repository: repository, action: .commit)
        request?.message = message; request?.includeUnstaged = includeUnstaged
        return request
    }

    public static func createPullRequest(_ repository: ThreadRepositories.Repository, title: String, body: String,
                                         base: String?, draft: Bool) -> Self? {
        var request = Self(repository: repository, action: .createPullRequest)
        request?.title = title; request?.body = body; request?.base = base; request?.draft = draft
        return request
    }

    /// Merges exactly the commit the owner reviewed; nil without a known one.
    public static func merge(_ repository: ThreadRepositories.Repository, pullRequest: URL, method: MergeMethod,
                             reviewedHead: String?) -> Self? {
        guard let head = PullRequestMergeState.validHead(reviewedHead) else { return nil }
        var request = Self(repository: repository, action: .merge)
        request?.pullRequestUrl = pullRequest.absoluteString; request?.method = method; request?.expectedHeadOid = head
        return request
    }

    /// Turning auto-merge on is pinned to the reviewed commit like Merge, and
    /// nil without a known one. Turning it off needs no commit: the Mac only
    /// disables it.
    public static func autoMerge(_ repository: ThreadRepositories.Repository, pullRequest: URL, enabled: Bool,
                                 method: MergeMethod?, reviewedHead: String? = nil) -> Self? {
        var request = Self(repository: repository, action: .autoMerge)
        request?.pullRequestUrl = pullRequest.absoluteString; request?.enabled = enabled
        if enabled {
            guard let head = PullRequestMergeState.validHead(reviewedHead) else { return nil }
            request?.method = method; request?.expectedHeadOid = head
        }
        return request
    }

    /// What the row shows while the Mac works.
    public var progressTitle: String {
        switch action {
        case .commit: "Committing…"
        case .push: "Pushing…"
        case .pull: "Pulling…"
        case .createPullRequest: draft == true ? "Opening draft pull request…" : "Opening pull request…"
        case .merge: "Merging…"
        case .autoMerge: enabled == false ? "Turning off auto-merge…" : "Turning on auto-merge…"
        }
    }

    public static func path(conversationId: String) throws -> String {
        try ThreadRepositories.path(conversationId: conversationId) + "/actions"
    }
}

/// What the Mac did, or why it refused, in its own plain words.
public struct GitActionOutcome: Decodable, Equatable, Sendable {
    public struct CreatedPullRequest: Decodable, Equatable, Sendable {
        public var number: Int
        public var url: URL
        public init(number: Int, url: URL) { self.number = number; self.url = url }
    }
    public var action: GitAction
    public var ok: Bool
    /// A stable refusal code (`confirmDefaultBranch`, `agentRunning`, `busy`…).
    public var reason: String?
    public var message: String
    public var commit: String?
    public var pullRequest: CreatedPullRequest?

    public init(action: GitAction, ok: Bool, reason: String? = nil, message: String, commit: String? = nil,
                pullRequest: CreatedPullRequest? = nil) {
        self.action = action; self.ok = ok; self.reason = reason; self.message = message
        self.commit = commit; self.pullRequest = pullRequest
    }

    /// The push would go straight to the default branch; ask first.
    public var needsDefaultBranchConfirmation: Bool { !ok && reason == "confirmDefaultBranch" }
}

/// Whether a pull request can merge now and how, read with gh on the Mac.
public struct PullRequestMergeState: Decodable, Equatable, Sendable {
    public var number: Int
    public var url: URL
    public var state: ThreadPullRequests.PullRequest.State
    public var headRef: String?
    public var baseRef: String?
    /// The head commit the Mac read, as it sent it.
    public var headOid: String?
    public var checks: ThreadPullRequests.PullRequest.Checks.State
    public var conflicting: Bool
    public var autoMerge: Bool
    public var methods: [MergeMethod]
    public var defaultMethod: MergeMethod?
    public var blockedReason: String?
    /// Why Merge is unavailable now, in plain words.
    public var blocked: String?

    public init(number: Int, url: URL, state: ThreadPullRequests.PullRequest.State, headRef: String? = nil,
                baseRef: String? = nil, headOid: String? = nil, checks: ThreadPullRequests.PullRequest.Checks.State = .passing,
                conflicting: Bool = false, autoMerge: Bool = false, methods: [MergeMethod] = MergeMethod.allCases,
                defaultMethod: MergeMethod? = .squash, blockedReason: String? = nil, blocked: String? = nil) {
        self.number = number; self.url = url; self.state = state; self.headRef = headRef; self.baseRef = baseRef; self.headOid = headOid
        self.checks = checks; self.conflicting = conflicting; self.autoMerge = autoMerge; self.methods = methods
        self.defaultMethod = defaultMethod; self.blockedReason = blockedReason; self.blocked = blocked
    }

    private enum CodingKeys: String, CodingKey {
        case number, url, state, headRef, baseRef, headOid, checks, conflicting, autoMerge, methods, defaultMethod, blockedReason, blocked
    }
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        number = try c.decode(Int.self, forKey: .number)
        url = try c.decode(URL.self, forKey: .url)
        state = try c.decode(ThreadPullRequests.PullRequest.State.self, forKey: .state)
        headRef = try c.decodeIfPresent(String.self, forKey: .headRef)
        baseRef = try c.decodeIfPresent(String.self, forKey: .baseRef)
        // A head of another type counts as unknown, never as a reason to drop the whole state.
        headOid = (try? c.decodeIfPresent(String.self, forKey: .headOid)) ?? nil
        checks = try c.decode(ThreadPullRequests.PullRequest.Checks.State.self, forKey: .checks)
        conflicting = try c.decode(Bool.self, forKey: .conflicting)
        autoMerge = try c.decode(Bool.self, forKey: .autoMerge)
        // A method a newer Mac adds is skipped.
        methods = try c.decode([String].self, forKey: .methods).compactMap(MergeMethod.init(rawValue:))
        defaultMethod = try c.decodeIfPresent(String.self, forKey: .defaultMethod).flatMap(MergeMethod.init(rawValue:))
        blockedReason = try c.decodeIfPresent(String.self, forKey: .blockedReason)
        blocked = try c.decodeIfPresent(String.self, forKey: .blocked)
    }

    /// A commit id as the Mac sends and checks it: 40 or 64 hex digits, lowercased.
    public static func validHead(_ oid: String?) -> String? {
        guard let oid, oid.count == 40 || oid.count == 64, oid.allSatisfy(\.isHexDigit), oid.allSatisfy(\.isASCII) else { return nil }
        return oid.lowercased()
    }

    /// The commit the owner reviews and a guarded action is pinned to; nil
    /// when the Mac sent none or an unusable one.
    public var reviewedHead: String? { Self.validHead(headOid) }

    /// Merge and auto-merge need to know which commit they are approving.
    public static let unknownHeadNote = "Your computer didn't report this pull request's latest commit, so it can't be merged here. Merge on GitHub instead."

    /// Auto-merge can be turned on: open, not a draft, no conflicts, and the
    /// reviewed commit is known. It can always be turned off.
    public var canAutoMerge: Bool { state == .open && !conflicting && !methods.isEmpty && reviewedHead != nil }

    /// Merge is offered: nothing blocks it on the Mac and the reviewed commit is known.
    public var canMerge: Bool { blocked == nil && reviewedHead != nil }

    /// Why a guarded action is unavailable because the head is unknown.
    public var headNote: String? { reviewedHead == nil ? Self.unknownHeadNote : nil }

    public static func path(conversationId: String, repository: ThreadRepositories.Repository, url: URL) throws -> String {
        guard let location = repository.location else { throw PairingFailure.invalidLink }
        var components = URLComponents()
        components.percentEncodedPath = try ThreadRepositories.path(conversationId: conversationId) + "/pull-request"
        components.queryItems = [URLQueryItem(name: "repositoryId", value: repository.id),
                                 URLQueryItem(name: "root", value: location.rootId),
                                 URLQueryItem(name: "repo", value: location.path),
                                 URLQueryItem(name: "url", value: url.absoluteString)]
        guard let path = components.string else { throw PairingFailure.invalidLink }
        return path
    }
}

/// The merge method and commit the owner approved. A later read of the pull
/// request can't change them: if its head moved, the approval no longer holds
/// and the owner reviews again.
public struct MergeApproval: Equatable, Sendable {
    public let method: MergeMethod
    public let head: String

    /// Nil when the displayed state has no known head to approve.
    public init?(method: MergeMethod, state: PullRequestMergeState) {
        guard let head = state.reviewedHead else { return nil }
        self.method = method; self.head = head
    }

    /// Whether `state` still shows the approved commit.
    public func holds(for state: PullRequestMergeState) -> Bool { state.reviewedHead == head }

    public func request(_ repository: ThreadRepositories.Repository, pullRequest: URL) -> GitActionRequest? {
        .merge(repository, pullRequest: pullRequest, method: method, reviewedHead: head)
    }
}

/// Which actions a repository offers now, and why not when it doesn't. The
/// Mac decides again when one runs; these keep the menu honest.
public struct GitActionAvailability: Equatable, Sendable {
    public struct Item: Equatable, Sendable {
        public var action: GitAction
        public var title: String
        /// Why it is unavailable, or nil when it can run.
        public var disabledReason: String?
        public var enabled: Bool { disabledReason == nil }
    }
    public var commit: Item
    public var push: Item
    public var pull: Item
    public var createPullRequest: Item
    /// The one action the row offers as a button.
    public var quick: Item?
    /// Pushing goes straight to the default branch.
    public var pushesToDefaultBranch: Bool

    public init(_ repo: ThreadRepositories.Repository, githubAvailable: Bool, busy: Bool) {
        let changes = repo.changes.map { $0.staged + $0.unstaged + $0.untracked } ?? 0
        let onBranch = repo.branch != nil && !repo.detached
        let ahead = repo.ahead ?? 0, behind = repo.behind ?? 0
        let defaultBranch = repo.defaultBranch ?? "main"
        let tracking = repo.upstream.map { $0.split(separator: "/", maxSplits: 1).last.map(String.init) ?? $0 }
        let onDefault = repo.branch == defaultBranch
        pushesToDefaultBranch = onDefault || tracking == defaultBranch
        let hasOpenPullRequest = repo.pullRequests.contains { $0.relation == .opened && ($0.state == .open || $0.state == .draft) }
        func item(_ action: GitAction, _ title: String, _ reasons: [(Bool, String)]) -> Item {
            Item(action: action, title: title, disabledReason: busy ? "Another Git action is running." : reasons.first { $0.0 }?.1)
        }
        commit = item(.commit, "Commit…", [
            (!onBranch, "Check out a branch on your computer first."),
            (changes == 0, "No changes to commit."),
        ])
        push = item(.push, "Push", [
            (!onBranch, "Check out a branch on your computer first."),
            (repo.noCommits, "Commit first; this branch has no commits."),
            (repo.upstream != nil && ahead == 0, "No commits to push."),
            (behind > 0, "Pull first; the remote has newer commits."),
        ])
        pull = item(.pull, "Pull", [
            (!onBranch, "Check out a branch on your computer first."),
            (repo.upstream == nil, "This branch doesn't track a remote branch yet."),
        ])
        createPullRequest = item(.createPullRequest, "Create pull request…", [
            (!githubAvailable, "Sign in to the GitHub CLI on your computer (gh auth login)."),
            (!onBranch, "Check out a branch on your computer first."),
            (onDefault, "Create a branch for this work first."),
            (hasOpenPullRequest, "This branch already has a pull request."),
            (repo.upstream == nil, "Push this branch first."),
            (ahead > 0, "Push the new commits first."),
        ])
        quick = [commit, push, createPullRequest].first(where: \.enabled)
            ?? (behind > 0 && pull.enabled ? pull : nil)
    }
}

/// Canned requests that put a pull request task into the composer for the
/// owner to edit and send. Nothing runs until they send it.
public enum PullRequestAgentTask: String, CaseIterable, Sendable, Identifiable {
    case fixChecks, addressComments, resolveConflicts
    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .fixChecks: "Fix failing checks"
        case .addressComments: "Address comments"
        case .resolveConflicts: "Resolve conflicts"
        }
    }

    public var systemImage: String {
        switch self {
        case .fixChecks: "wrench.and.screwdriver"
        case .addressComments: "text.bubble"
        case .resolveConflicts: "arrow.triangle.branch"
        }
    }

    /// Offered for an open pull request; Resolve conflicts only when it conflicts.
    public static func available(state: ThreadPullRequests.PullRequest.State?, checks: ThreadPullRequests.PullRequest.Checks.State?,
                                 conflicting: Bool) -> [Self] {
        guard state == .open || state == .draft else { return [] }
        var tasks: [Self] = []
        if checks == .failing { tasks.append(.fixChecks) }
        tasks.append(.addressComments)
        if conflicting { tasks.append(.resolveConflicts) }
        return tasks
    }

    /// The fixed request, naming only the pull request, its address and branches.
    public func prompt(number: Int, url: URL, branch: String?, base: String?) -> String {
        let pr = "pull request #\(number) (\(url.absoluteString))"
        let on = branch.map { " on branch `\($0)`" } ?? ""
        switch self {
        case .fixChecks:
            return "Fix the failing checks on \(pr)\(on). Read the failing check logs, fix the cause, run the checks locally where you can, then commit and push the fix."
        case .addressComments:
            return "Address the review comments on \(pr)\(on). Read the open review threads, verify each comment before changing code, skip resolved ones, then commit and push the fixes."
        case .resolveConflicts:
            let into = base.map { " with `\($0)`" } ?? " with its base branch"
            return "Resolve the merge conflicts in \(pr)\(on)\(into). Keep the intent of both sides, run the checks, then commit and push."
        }
    }

    /// Adds the request after anything already written, never replacing it.
    public static func append(_ prompt: String, to draft: String) -> String {
        let existing = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        if existing.isEmpty { return prompt }
        if existing.hasSuffix(prompt) { return draft }
        return existing + "\n\n" + prompt
    }
}

/// One control's latest result, kept while Code changes stays open.
public struct GitActionStatus: Equatable, Sendable {
    public enum Phase: Equatable, Sendable { case running, succeeded, failed }
    public var phase: Phase
    public var message: String
    public init(phase: Phase, message: String) { self.phase = phase; self.message = message }
}

/// Runs Git actions for Code changes, one per repository or pull request at a
/// time, and keeps each one's progress and result for the sheet. Its tasks end
/// with the sheet.
@MainActor
public final class GitActionsModel: ObservableObject {
    public struct Confirmation: Identifiable, Equatable {
        public let key: String
        public let request: GitActionRequest
        public let message: String
        public var id: String { key }
        public init(key: String, request: GitActionRequest, message: String) {
            self.key = key; self.request = request; self.message = message
        }
    }

    @Published public private(set) var statuses: [String: GitActionStatus] = [:]
    @Published public var confirmation: Confirmation?
    private var tasks: [String: Task<Void, Never>] = [:]
    private let perform: @MainActor (GitActionRequest) async throws -> GitActionOutcome
    private let describe: @MainActor (Error) -> String
    private let succeeded: @MainActor (GitActionOutcome) async -> Void

    public init(perform: @escaping @MainActor (GitActionRequest) async throws -> GitActionOutcome,
                describe: @escaping @MainActor (Error) -> String = { _ in "Your computer couldn't run this. Try again." },
                succeeded: @escaping @MainActor (GitActionOutcome) async -> Void = { _ in }) {
        self.perform = perform; self.describe = describe; self.succeeded = succeeded
    }

    public func status(_ key: String) -> GitActionStatus? { statuses[key] }
    public func isRunning(_ key: String) -> Bool { statuses[key]?.phase == .running }

    /// Starts `request` for `key` unless one is already running there.
    @discardableResult
    public func run(_ request: GitActionRequest, key: String) -> Task<Void, Never>? {
        guard !isRunning(key) else { return nil }
        statuses[key] = GitActionStatus(phase: .running, message: request.progressTitle)
        let task = Task { [weak self] in
            guard let self else { return }
            let result: GitActionStatus?
            do {
                let outcome = try await self.perform(request)
                if Task.isCancelled { return }
                if outcome.needsDefaultBranchConfirmation {
                    self.confirmation = Confirmation(key: key, request: request, message: outcome.message)
                    result = nil
                } else {
                    result = GitActionStatus(phase: outcome.ok ? .succeeded : .failed, message: outcome.message)
                    if outcome.ok { await self.succeeded(outcome) }
                }
            } catch is CancellationError {
                return
            } catch {
                if Task.isCancelled { return }
                result = GitActionStatus(phase: .failed, message: self.describe(error))
            }
            self.statuses[key] = result
            self.tasks[key] = nil
        }
        tasks[key] = task
        return task
    }

    /// The owner confirmed pushing to the default branch.
    @discardableResult
    public func confirm() -> Task<Void, Never>? {
        guard let confirmation else { return nil }
        self.confirmation = nil
        var request = confirmation.request
        request.confirmDefaultBranch = true
        return run(request, key: confirmation.key)
    }

    public func clear(_ key: String) { if !isRunning(key) { statuses[key] = nil } }

    /// The sheet closed: stop waiting for results nobody will see.
    public func cancelAll() {
        tasks.values.forEach { $0.cancel() }
        tasks = [:]
        statuses = statuses.filter { $0.value.phase != .running }
        confirmation = nil
    }
}
