import Foundation

/// The repositories a Project thread worked with, as the Mac derives them from
/// the thread's own records: its Project folder's repository, ones it edited,
/// created or cloned (anywhere on the Mac) and pull requests it opened or
/// reviewed. A repository outside the Project's folders arrives without a
/// location, so its files and diffs are never offered here.
public struct ThreadRepositories: Decodable, Equatable, Sendable {
    public static let supportedVersion = 1

    public enum Role: String, Decodable, Sendable, CaseIterable {
        case workspace, edited, created, cloned, reviewed

        /// Plain words for the role chips and VoiceOver.
        public var title: String {
            switch self {
            case .workspace: "Project folder"
            case .edited: "Edited"
            case .created: "Created in this chat"
            case .cloned: "Cloned"
            case .reviewed: "Reviewing PR"
            }
        }
    }

    /// Where the Mac's Code changes readers find a repository inside a Project folder.
    public struct Location: Decodable, Equatable, Hashable, Sendable {
        public var rootId: String
        public var path: String
        public init(rootId: String, path: String) { self.rootId = rootId; self.path = path }
    }

    public struct Changes: Decodable, Equatable, Sendable {
        public var staged: Int
        public var unstaged: Int
        public var untracked: Int
        public var limited: Bool
        public init(staged: Int, unstaged: Int, untracked: Int, limited: Bool = false) {
            self.staged = staged; self.unstaged = unstaged; self.untracked = untracked; self.limited = limited
        }

        /// "1 staged · 2 unstaged · 1 untracked", or that there is nothing to commit.
        public var summary: String {
            let parts = [(staged, "staged"), (unstaged, "unstaged"), (untracked, "untracked")]
                .filter { $0.0 > 0 }.map { "\($0.0.formatted())\(limited ? "+" : "") \($0.1)" }
            return parts.isEmpty ? "No uncommitted changes" : parts.joined(separator: " · ")
        }
        public var accessibilityText: String { summary.replacingOccurrences(of: " · ", with: ", ") }
    }

    public struct PullRequest: Decodable, Equatable, Identifiable, Sendable {
        public enum Relation: String, Decodable, Sendable {
            case opened, reviewing
            public var title: String { self == .opened ? "Opened here" : "Reviewing" }
        }
        public var number: Int
        public var repository: String
        public var url: URL
        public var relation: Relation
        public var title: String?
        public var state: ThreadPullRequests.PullRequest.State?
        public var checks: ThreadPullRequests.PullRequest.Checks.State?
        /// GitHub's last update (UTC RFC 3339); nil when GitHub was not read.
        public var updatedAt: String?
        public var id: String { url.absoluteString }

        public init(number: Int, repository: String, url: URL, relation: Relation, title: String? = nil,
                    state: ThreadPullRequests.PullRequest.State? = nil, checks: ThreadPullRequests.PullRequest.Checks.State? = nil,
                    updatedAt: String? = nil) {
            self.number = number; self.repository = repository; self.url = url; self.relation = relation
            self.title = title; self.state = state; self.checks = checks; self.updatedAt = updatedAt
        }

        public var displayTitle: String { title ?? "Pull request #\(number)" }

        public var accessibilityText: String {
            ["Pull request \(number)", title, state?.title, relation.title, repository].compactMap { $0 }.joined(separator: ", ")
        }
    }

    public struct Repository: Decodable, Equatable, Identifiable, Sendable {
        public var id: String
        public var name: String
        public var roles: [Role]
        public var local: Bool
        public var location: Location?
        /// The Mac offers Git actions here: the whole repository is inside a
        /// Project folder. False from a Mac without Git actions.
        public var writable: Bool
        /// The branch pull requests target; pushing to it asks first.
        public var defaultBranch: String?
        public var remote: String?
        public var branch: String?
        public var detached: Bool
        public var noCommits: Bool
        public var baseRef: String?
        public var upstream: String?
        public var ahead: Int?
        public var behind: Int?
        public var changes: Changes?
        public var pullRequests: [PullRequest]
        public var detail: String?
        /// A linked worktree's folder name; `name` is then its repository's.
        public var worktree: String?
        /// The listed entry this worktree is shown under.
        public var worktreeOf: String?
        /// The repository's other checkouts the thread worked in, folded under
        /// this entry when the index is built; each keeps its own changes and
        /// Git actions.
        public internal(set) var worktrees: [Repository] = []

        public init(id: String, name: String, roles: [Role], local: Bool = true, location: Location? = nil,
                    writable: Bool = false, defaultBranch: String? = nil, remote: String? = nil, branch: String? = nil, detached: Bool = false, noCommits: Bool = false,
                    baseRef: String? = nil, upstream: String? = nil, ahead: Int? = nil, behind: Int? = nil,
                    changes: Changes? = nil, pullRequests: [PullRequest] = [], detail: String? = nil,
                    worktree: String? = nil, worktreeOf: String? = nil) {
            self.id = id; self.name = name; self.roles = roles; self.local = local; self.location = location
            self.writable = writable; self.defaultBranch = defaultBranch
            self.remote = remote; self.branch = branch; self.detached = detached; self.noCommits = noCommits
            self.baseRef = baseRef; self.upstream = upstream; self.ahead = ahead; self.behind = behind
            self.changes = changes; self.pullRequests = pullRequests; self.detail = detail
            self.worktree = worktree; self.worktreeOf = worktreeOf
        }

        private enum CodingKeys: String, CodingKey {
            case id, name, roles, local, location, writable, defaultBranch, remote, branch, detached, noCommits, baseRef, upstream, ahead, behind, changes, pullRequests, detail, worktree, worktreeOf
        }
        public init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            id = try c.decode(String.self, forKey: .id)
            name = try c.decode(String.self, forKey: .name)
            // A role a newer Mac adds is skipped rather than failing the list.
            roles = try c.decode([String].self, forKey: .roles).compactMap(Role.init(rawValue:))
            local = try c.decode(Bool.self, forKey: .local)
            location = try c.decodeIfPresent(Location.self, forKey: .location)
            writable = try c.decodeIfPresent(Bool.self, forKey: .writable) ?? false
            defaultBranch = try c.decodeIfPresent(String.self, forKey: .defaultBranch)
            remote = try c.decodeIfPresent(String.self, forKey: .remote)
            branch = try c.decodeIfPresent(String.self, forKey: .branch)
            detached = try c.decode(Bool.self, forKey: .detached)
            noCommits = try c.decode(Bool.self, forKey: .noCommits)
            baseRef = try c.decodeIfPresent(String.self, forKey: .baseRef)
            upstream = try c.decodeIfPresent(String.self, forKey: .upstream)
            ahead = try c.decodeIfPresent(Int.self, forKey: .ahead)
            behind = try c.decodeIfPresent(Int.self, forKey: .behind)
            changes = try c.decodeIfPresent(Changes.self, forKey: .changes)
            // Only github.com pull request pages open from the sheet.
            pullRequests = try c.decode([PullRequest].self, forKey: .pullRequests).filter {
                $0.url.scheme == "https" && $0.url.host == "github.com" && $0.url.path.contains("/pull/")
            }
            detail = try c.decodeIfPresent(String.self, forKey: .detail)
            worktree = try c.decodeIfPresent(String.self, forKey: .worktree).flatMap { $0.isEmpty ? nil : $0 }
            worktreeOf = try c.decodeIfPresent(String.self, forKey: .worktreeOf)
        }

        /// The checkout's own name: a worktree's folder, else the repository's.
        public var checkoutName: String { worktree ?? name }

        /// This entry and the worktrees folded under it, in the Mac's order.
        public var checkouts: [Repository] { [self] + worktrees }

        /// Its files and diffs can be read: a repository inside a Project folder.
        public var opensChanges: Bool { local && location != nil }

        /// Commit, push, pull and pull request actions are offered.
        public var actionsAvailable: Bool { local && writable && location != nil }

        /// "agent/x → origin/main", "main", "Detached", or nil for a pull
        /// request's repository that is not on the Mac.
        public var branchTitle: String? {
            guard local else { return nil }
            let name = branch ?? "Detached"
            guard let baseRef else { return name }
            return "\(name) → \(baseRef)"
        }

        /// Where the branch stands with what it pushes to.
        public var syncText: String? {
            guard local else { return nil }
            if noCommits { return "No commits yet" }
            if detached { return nil }
            guard let upstream else { return "Not pushed" }
            guard let ahead, let behind else { return "Tracks \(upstream)" }
            if ahead == 0 && behind == 0 { return "Up to date with \(upstream)" }
            var parts: [String] = []
            if ahead > 0 { parts.append("\(ahead) \(ahead == 1 ? "commit" : "commits") to push") }
            if behind > 0 { parts.append("\(behind) behind") }
            return parts.joined(separator: ", ")
        }

        /// What VoiceOver reads for the repository's heading.
        public var accessibilityText: String {
            var parts = ["Repository \(name)"]
            if let worktree { parts.append("worktree \(worktree)") }
            parts += roles.map(\.title)
            if local { parts.append(branch.map { "branch \($0)" } ?? "detached") }
            if let syncText { parts.append(syncText) }
            if let changes { parts.append(changes.accessibilityText) }
            if !local { parts.append("not on your computer") }
            return parts.joined(separator: ", ")
        }

        /// What VoiceOver reads for a worktree listed under its repository.
        public var worktreeAccessibilityText: String {
            var parts = ["Worktree \(worktree ?? name)"]
            parts += roles.map(\.title)
            parts.append(branch.map { "branch \($0)" } ?? "detached")
            if let syncText { parts.append(syncText) }
            if let changes { parts.append(changes.accessibilityText) }
            if let detail { parts.append(detail) }
            return parts.joined(separator: ", ")
        }
    }

    public var version: Int
    public var githubAvailable: Bool
    public var truncated: Bool
    public var detail: String?
    /// One entry per repository: a linked worktree whose lead the Mac listed
    /// is folded into that lead's `worktrees` instead.
    public var repositories: [Repository]

    public init(version: Int = supportedVersion, githubAvailable: Bool = true, truncated: Bool = false,
                detail: String? = nil, repositories: [Repository]) {
        self.version = version; self.githubAvailable = githubAvailable; self.truncated = truncated
        self.detail = detail; self.repositories = Self.fold(repositories)
    }

    private enum CodingKeys: String, CodingKey { case version, githubAvailable, truncated, detail, repositories }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init(version: try c.decode(Int.self, forKey: .version),
                  githubAvailable: try c.decode(Bool.self, forKey: .githubAvailable),
                  truncated: try c.decode(Bool.self, forKey: .truncated),
                  detail: try c.decodeIfPresent(String.self, forKey: .detail),
                  repositories: try c.decode([Repository].self, forKey: .repositories))
    }

    /// Folds each worktree under the listed entry it names, keeping the Mac's
    /// order; one whose lead is missing (or is itself folded) stays its own
    /// repository.
    static func fold(_ entries: [Repository]) -> [Repository] {
        let leads = Set(entries.filter { $0.worktreeOf == nil }.map(\.id))
        var folded: [Repository] = []
        var position: [String: Int] = [:]
        for entry in entries {
            if let lead = entry.worktreeOf, leads.contains(lead), let index = position[lead] {
                var member = entry
                member.worktrees = []
                folded[index].worktrees.append(member)
            } else {
                position[entry.id] = folded.count
                folded.append(entry)
            }
        }
        return folded
    }

    /// Every checkout listed, worktrees included, in the Mac's order.
    public var checkouts: [Repository] { repositories.flatMap(\.checkouts) }

    /// One section per repository when the thread worked with more than one,
    /// or with worktrees of one; a single checkout keeps the familiar branch
    /// layout.
    public var showsSections: Bool {
        version == Self.supportedVersion && (repositories.count > 1 || repositories.contains { !$0.worktrees.isEmpty })
    }

    public static func path(conversationId: String, refresh: Bool = false) throws -> String {
        guard !conversationId.isEmpty,
              let escaped = conversationId.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else {
            throw PairingFailure.invalidLink
        }
        return "/api/v1/conversations/\(escaped)/repos" + (refresh ? "?refresh=true" : "")
    }
}
