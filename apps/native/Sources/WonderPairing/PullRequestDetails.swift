import Foundation

/// Native GitHub facts returned with the existing cached PR read. Missing values
/// stay unavailable; mergeability alone never establishes permission to merge.
public struct PullRequestDetails: Decodable, Equatable, Sendable {
    public struct Check: Decodable, Equatable, Sendable {
        public let name: String
        public let workflow: String?
        public let state: ThreadPullRequests.PullRequest.Checks.State
        public enum Result: String, Decodable, Sendable {
            case success = "SUCCESS", failure = "FAILURE", error = "ERROR"
            case queued = "QUEUED", requested = "REQUESTED", waiting = "WAITING"
            case pending = "PENDING", expected = "EXPECTED", running = "IN_PROGRESS"
            case skipped = "SKIPPED", neutral = "NEUTRAL", cancelled = "CANCELLED"
            case timedOut = "TIMED_OUT", actionRequired = "ACTION_REQUIRED"
            case stale = "STALE", startupFailure = "STARTUP_FAILURE", unknown = "UNKNOWN"

            public init(from decoder: Decoder) throws {
                self = Self(rawValue: try decoder.singleValueContainer().decode(String.self)) ?? .unknown
            }
        }
        public let result: Result
        public var isNeutral: Bool { result == .skipped || result == .neutral }
        public var resultTitle: String {
            switch result {
            case .success: "Passed"
            case .failure, .error: "Failed"
            case .queued, .requested, .waiting, .pending, .expected: "Pending"
            case .running: "Running"
            case .skipped: "Skipped"
            case .neutral: "Neutral"
            case .cancelled: "Cancelled"
            case .timedOut: "Timed out"
            case .actionRequired: "Action required"
            case .stale: "Stale"
            case .startupFailure: "Could not start"
            case .unknown: "Unknown"
            }
        }
    }

    public struct Reviewer: Decodable, Equatable, Sendable {
        public let name: String
        public let kind: String
        public var title: String { kind == "team" ? "\(name) (team)" : name }
    }

    public let checks: [Check]?
    public let reviewDecision: String?
    public let reviewRequests: [Reviewer]?
    public let mergeStateStatus: String?
    public let mergeable: String?
    public let changedFiles: Int?
    // gh's JSON export omits pagination metadata. At its first-page ceiling,
    // tell the person to inspect GitHub instead of claiming a complete list.
    public var reviewersMayBeLimited: Bool { (reviewRequests?.count ?? 0) >= 100 }

    public var reviewTitle: String {
        switch reviewDecision {
        case "APPROVED": "Approved"
        case "CHANGES_REQUESTED": "Changes requested"
        case "REVIEW_REQUIRED": "Review required"
        default: "No review decision available"
        }
    }

    public func mergeSummary(state: ThreadPullRequests.PullRequest.State) -> String {
        switch state {
        case .merged: return "Merged"
        case .closed: return "Closed without merging"
        case .draft: return "Draft — not ready for review"
        default: break
        }
        if mergeable == "CONFLICTING" { return "Merge conflicts must be resolved" }
        switch mergeStateStatus {
        case "DIRTY": return "Merge conflicts must be resolved"
        case "BLOCKED": return "GitHub reports this merge is blocked. Open GitHub for the requirements."
        case "BEHIND": return "The branch is behind its base branch"
        case "UNSTABLE": return "GitHub reports some commit checks are not passing"
        case "HAS_HOOKS": return "Checks passed; merge hooks are configured"
        case "CLEAN": return "GitHub reports a clean merge and passing checks"
        case "DRAFT": return "Draft — not ready for review"
        default: return "Merge status unavailable"
        }
    }
}
