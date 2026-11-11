import SwiftUI
import WonderPairing

/// A read-only view of the same snapshot used by Code changes. Refresh belongs
/// to the connection model, including its connection and request identity fences.
struct PullRequestDetailView: View {
    let pr: ThreadPullRequests.PullRequest
    let error: String?
    let refreshing: Bool
    let refresh: () async -> Void

    var body: some View {
        WonderList {
            Section {
                Text(pr.title).font(.headline).textSelection(.enabled)
                LabeledContent(pr.repository, value: pr.state.title)
                if let error {
                    Text(error).font(.subheadline).foregroundStyle(.secondary)
                        .accessibilityIdentifier("pull-request-detail-error")
                    Button("Try again") { Task { await refresh() } }
                        .disabled(refreshing).accessibilityIdentifier("pull-request-detail-retry")
                }
            }
            if let details = pr.details {
                Section("Checks") {
                    if let checks = details.checks {
                        if checks.isEmpty { Text("No checks reported").foregroundStyle(.secondary) }
                        ForEach(checks.indices, id: \.self) { index in
                            let check = checks[index]
                            HStack(alignment: .firstTextBaseline, spacing: 10) {
                                if check.isNeutral {
                                    Image(systemName: "minus.circle").foregroundStyle(.secondary)
                                } else {
                                    PullRequestChecksGlyph(state: check.state)
                                }
                                VStack(alignment: .leading, spacing: 3) {
                                    Text(check.name)
                                    Text([check.workflow, check.resultTitle].compactMap { $0 }.joined(separator: " · "))
                                        .font(.subheadline).foregroundStyle(.secondary)
                                }
                            }
                            .accessibilityElement(children: .ignore)
                            .accessibilityLabel([check.name, check.workflow, check.resultTitle].compactMap { $0 }.joined(separator: ", "))
                            .accessibilityIdentifier("pull-request-check:\(index)")
                        }
                        if pr.checks.limited {
                            Text("More checks may be available on GitHub.").font(.subheadline).foregroundStyle(.secondary)
                        }
                    } else {
                        Text("Check results unavailable").foregroundStyle(.secondary)
                    }
                }
                Section("Review") {
                    Text(details.reviewTitle).accessibilityIdentifier("pull-request-review")
                    if let reviewers = details.reviewRequests {
                        if reviewers.isEmpty { Text("No requested reviewers reported").foregroundStyle(.secondary) }
                        ForEach(reviewers.indices, id: \.self) { index in
                            Label(reviewers[index].title, systemImage: reviewers[index].kind == "team" ? "person.2" : "person")
                        }
                        if details.reviewersMayBeLimited {
                            Text("More requested reviewers may be available on GitHub.").font(.subheadline).foregroundStyle(.secondary)
                        }
                    } else {
                        Text("Requested reviewers unavailable").foregroundStyle(.secondary)
                    }
                }
                Section("Merge status") {
                    Text(details.mergeSummary(state: pr.state))
                        .accessibilityIdentifier("pull-request-merge-status")
                }
                Section {
                    Link(destination: pr.url.appendingPathComponent("files")) {
                        LabeledContent("Files changed", value: details.changedFiles.map(String.init) ?? "Unavailable")
                    }
                    .accessibilityIdentifier("pull-request-files")
                    .accessibilityHint("Opens the changed files on GitHub")
                }
            } else {
                Section {
                    Text("Details are unavailable from this computer. Open the pull request on GitHub.")
                        .foregroundStyle(.secondary)
                        .accessibilityIdentifier("pull-request-details-unavailable")
                }
            }
            Section {
                Link("Open on GitHub", destination: pr.url)
                    .accessibilityIdentifier("pull-request-github")
            }
        }
        .listSectionSpacing(12)
        .navigationTitle("Pull request #\(pr.number)")
        .navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("pull-request-detail")
        .refreshable { await refresh() }
    }
}
