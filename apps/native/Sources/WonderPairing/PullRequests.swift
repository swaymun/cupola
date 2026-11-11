import Foundation

/// Pull requests a Project thread worked with, as the Mac reads them through
/// its own GitHub CLI sign-in. No GitHub credential reaches the phone.
public struct ThreadPullRequests: Decodable, Equatable, Sendable {
    public static let supportedVersion = 1

    public struct PullRequest: Decodable, Equatable, Identifiable, Sendable {
        public enum State: String, Decodable, Sendable {
            case open, draft, merged, closed, unknown
            public init(from decoder: Decoder) throws {
                self = State(rawValue: try decoder.singleValueContainer().decode(String.self)) ?? .unknown
            }
            public var title: String {
                switch self {
                case .open: "Open"
                case .draft: "Draft"
                case .merged: "Merged"
                case .closed: "Closed"
                case .unknown: "Unknown"
                }
            }
        }

        public struct Checks: Decodable, Equatable, Sendable {
            public enum State: String, Decodable, Sendable {
                case passing, failing, pending, none, unknown
                public init(from decoder: Decoder) throws {
                    self = State(rawValue: try decoder.singleValueContainer().decode(String.self)) ?? .unknown
                }
            }
            public var state: State
            public var passed: Int
            /// Finished neutral or skipped: neither passed nor failed. GitHub lets
            /// them satisfy a required check, so `state` stays `passing`, but they
            /// are never called passed. An older Mac counts them in `passed`.
            public var neutral: Int
            public var failed: Int
            public var pending: Int
            public var unknown: Int
            public var limited: Bool
            public init(state: State, passed: Int = 0, neutral: Int = 0, failed: Int = 0, pending: Int = 0, unknown: Int = 0,
                        limited: Bool = false) {
                self.state = state; self.passed = passed; self.neutral = neutral; self.failed = failed; self.pending = pending
                self.unknown = unknown; self.limited = limited
            }

            private enum CodingKeys: String, CodingKey { case state, passed, neutral, failed, pending, unknown, limited }
            public init(from decoder: Decoder) throws {
                let c = try decoder.container(keyedBy: CodingKeys.self)
                state = try c.decode(State.self, forKey: .state)
                passed = try c.decode(Int.self, forKey: .passed)
                neutral = try c.decodeIfPresent(Int.self, forKey: .neutral) ?? 0
                failed = try c.decode(Int.self, forKey: .failed)
                pending = try c.decode(Int.self, forKey: .pending)
                unknown = try c.decodeIfPresent(Int.self, forKey: .unknown) ?? 0
                limited = try c.decodeIfPresent(Bool.self, forKey: .limited) ?? false
            }

            /// Every check finished and none passed outright: only neutral or
            /// skipped ones, such as an automated review's run. Not a green check mark.
            public var isNeutralOnly: Bool { state == .passing && passed == 0 && neutral > 0 }

            /// "2 of 5 checks failed", or nil when the pull request has no checks.
            public var summary: String? {
                let total = passed + neutral + failed + pending + unknown
                guard total > 0 || state == .unknown else { return nil }
                let noun = total == 1 ? "check" : "checks"
                switch state {
                case .failing: return limited ? "\(failed) reported \(failed == 1 ? "check" : "checks") failed" : "\(failed) of \(total) \(noun) failed"
                case .pending: return limited ? "\(pending) reported \(pending == 1 ? "check" : "checks") running" : "\(pending) of \(total) \(noun) running"
                case .passing:
                    if isNeutralOnly { return neutral == 1 ? "1 neutral check" : "\(neutral) neutral checks" }
                    if neutral > 0 { return "\(passed) passed, \(neutral) neutral" }
                    return limited ? "Reported checks passed" : (total == 1 ? "Check passed" : "All \(total) checks passed")
                case .none: return nil
                case .unknown: return "Some check results are unavailable"
                }
            }
        }

        public var number: Int
        public var repository: String
        public var title: String
        public var state: State
        public var checks: Checks
        public var url: URL
        public var details: PullRequestDetails?
        /// GitHub's last update (UTC RFC 3339); an older Mac omits it.
        public var updatedAt: String?
        /// The branch it merges from and into, its head commit, its line
        /// counts and its author's login; an older Mac omits them.
        public var headRef: String?
        public var baseRef: String?
        public var headOid: String?
        public var additions: Int?
        public var deletions: Int?
        public var author: String?
        public var id: String { url.absoluteString }

        public init(number: Int, repository: String, title: String, state: State, checks: Checks, url: URL,
                    details: PullRequestDetails? = nil, updatedAt: String? = nil, headRef: String? = nil,
                    baseRef: String? = nil, headOid: String? = nil, additions: Int? = nil, deletions: Int? = nil,
                    author: String? = nil) {
            self.number = number; self.repository = repository; self.title = title
            self.state = state; self.checks = checks; self.url = url; self.details = details; self.updatedAt = updatedAt
            self.headRef = headRef; self.baseRef = baseRef; self.headOid = headOid
            self.additions = additions; self.deletions = deletions; self.author = author
        }

        /// Open or draft: still waiting to merge.
        public var isActive: Bool { state == .open || state == .draft }

        /// What wants the person's attention on an open pull request: failing
        /// checks, a reviewer's requested changes or merge conflicts.
        public var needsAttention: Bool {
            guard isActive else { return false }
            return checks.state == .failing || details?.reviewDecision == "CHANGES_REQUESTED"
                || details?.mergeable == "CONFLICTING" || details?.mergeStateStatus == "DIRTY"
        }
    }

    /// The listed pull requests by state, as the Mac counted them.
    public struct Counts: Decodable, Equatable, Sendable {
        public var open: Int
        public var draft: Int
        public var merged: Int
        public var closed: Int
        public init(open: Int = 0, draft: Int = 0, merged: Int = 0, closed: Int = 0) {
            self.open = open; self.draft = draft; self.merged = merged; self.closed = closed
        }
        /// Counted from the items, for a Mac that sends no counts.
        public init(_ pullRequests: [PullRequest]) {
            self.init()
            for pr in pullRequests {
                switch pr.state {
                case .open: open += 1
                case .draft: draft += 1
                case .merged: merged += 1
                case .closed, .unknown: closed += 1
                }
            }
        }
    }

    /// The feature branch the thread's folder is on. The Mac sends none on a
    /// default branch, a detached HEAD or outside Git.
    public struct Branch: Decodable, Equatable, Sendable {
        public struct Opened: Decodable, Equatable, Sendable {
            public var number: Int
            public var url: URL
            public init(number: Int, url: URL) { self.number = number; self.url = url }
        }
        public var name: String
        /// It tracks a branch on the remote, so a pull request can be opened from it.
        public var pushed: Bool
        /// The listed open or draft pull request from this branch.
        public var pullRequest: Opened?
        public init(name: String, pushed: Bool, pullRequest: Opened? = nil) {
            self.name = name; self.pushed = pushed; self.pullRequest = pullRequest
        }
    }

    public var version: Int
    public var available: Bool
    public var detail: String?
    /// The newest of the thread's pull requests.
    public var pullRequests: [PullRequest]
    /// Every pull request the thread opened; more than `pullRequests` holds
    /// for a thread that opened many. An older Mac sends only the list.
    public var total: Int
    /// The listed pull requests by state; counted from the list for an older Mac.
    public var counts: Counts
    /// Every pull request the thread opened was read, so `counts` covers all
    /// of them. An older Mac doesn't say; then a list shorter than `total` is not.
    public var complete: Bool
    public var branch: Branch?

    public init(version: Int = supportedVersion, available: Bool, detail: String? = nil, pullRequests: [PullRequest], total: Int? = nil,
                counts: Counts? = nil, complete: Bool? = nil, branch: Branch? = nil) {
        self.version = version; self.available = available; self.detail = detail; self.pullRequests = pullRequests
        self.total = max(total ?? 0, pullRequests.count)
        self.counts = counts ?? Counts(pullRequests)
        self.complete = complete ?? (self.total <= pullRequests.count)
        self.branch = branch
    }

    private enum CodingKeys: String, CodingKey { case version, available, detail, pullRequests, total, counts, complete, branch }
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        version = try container.decode(Int.self, forKey: .version)
        available = try container.decode(Bool.self, forKey: .available)
        detail = try container.decodeIfPresent(String.self, forKey: .detail)
        // Only github.com pull request pages open from the sheet.
        pullRequests = try container.decode([PullRequest].self, forKey: .pullRequests).filter {
            $0.url.scheme == "https" && $0.url.host == "github.com" && $0.url.path.contains("/pull/")
        }
        total = max(try container.decodeIfPresent(Int.self, forKey: .total) ?? 0, pullRequests.count)
        counts = try container.decodeIfPresent(Counts.self, forKey: .counts) ?? Counts(pullRequests)
        complete = try container.decodeIfPresent(Bool.self, forKey: .complete) ?? (total <= pullRequests.count)
        branch = try? container.decodeIfPresent(Branch.self, forKey: .branch)
    }

    /// Whether the pill appears: GitHub is signed in on the Mac, the response is
    /// a version this app understands, and the thread has pull requests.
    public var showsPill: Bool {
        version == Self.supportedVersion && available && !pullRequests.isEmpty
    }

    public static func path(conversationId: String, refresh: Bool = false) throws -> String {
        guard !conversationId.isEmpty,
              let escaped = conversationId.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else {
            throw PairingFailure.invalidLink
        }
        return "/api/v1/project-conversations/\(escaped)/pull-requests" + (refresh ? "?refresh=true" : "")
    }
}
