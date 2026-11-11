#if WONDER_DIAGNOSTICS
import Foundation

/// Synthetic GitHub facts for the real PR navigation flow; never starts a
/// provider turn or sends a GitHub write request.
enum DiagnosticPullRequestDetails {
    static func fields(number: Int, refreshed: Bool) -> [String: Any]? {
        guard number == 128 else { return nil }
        return [
            "checks": [
                ["name": "Unit tests", "workflow": "CI", "state": "passing", "result": "SUCCESS"],
                ["name": "Package tests", "state": "passing", "result": "SUCCESS"],
                ["name": "Documentation", "state": "passing", "result": "SKIPPED"],
                ["name": "iOS build", "state": "pending", "result": "IN_PROGRESS"],
                ["name": "Privacy audit", "state": "pending", "result": "QUEUED"]
            ],
            "reviewDecision": refreshed ? "APPROVED" : "REVIEW_REQUIRED",
            "reviewRequests": [["name": "alex", "kind": "user"], ["name": "example/mobile", "kind": "team"]],
            "mergeStateStatus": "BLOCKED", "mergeable": "MERGEABLE", "changedFiles": 7
        ]
    }
}
#endif
