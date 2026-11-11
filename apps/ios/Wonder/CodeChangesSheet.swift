import SwiftUI
import UIKit
import WonderPairing

/// The Git logo (Jason Long, CC BY 3.0) as a template vector, so it tints with
/// the theme like an SF Symbol. Credited in Settings > Acknowledgements.
struct GitMark: View {
    var body: some View {
        Image("GitLogo").renderingMode(.template).resizable().scaledToFit()
            .accessibilityHidden(true)
    }
}

extension ComposerPill {
    /// The pill's icon in the chat menu and Settings > Pills. Pull requests
    /// uses the open pull request symbol its rows use, not Fork's branch.
    @MainActor var menuIcon: Image { Image(systemName: systemImage) }
}

/// Code changes: the branch and what it is compared with, then This thread (the
/// files the thread itself changed, under its pull requests), the last reply's
/// edits, or the whole checkout's uncommitted work or branch, each file opening
/// its diff. A thread that worked with several repositories gets
/// one section per repository instead (roles, branch, sync, uncommitted counts
/// and pull requests), each inside a Project folder opening its own changes.
/// Read from the Mac with Git and the GitHub CLI. A repository wholly inside a
/// Project folder also offers Git actions (commit, push, pull, pull requests,
/// merge and auto-merge) that the Mac runs with its own sign-in; each control
/// keeps its progress and result while the sheet is open.
struct CodeChangesSheet: View {
    typealias Location = ThreadRepositories.Location
    struct DiffTarget: Hashable {
        let file: WorkspaceGitChanges.File
        let scope: WorkspaceChangeScope
        var location: Location? = nil
        static func == (a: Self, b: Self) -> Bool { a.file.id == b.file.id && a.scope == b.scope && a.location == b.location }
        func hash(into hasher: inout Hasher) { hasher.combine(file.id); hasher.combine(scope); hasher.combine(location) }
    }
    struct PullRequestTarget: Hashable { let id: String }
    struct RepositoryTarget: Hashable { let id: String }

    struct ReplyTarget: Hashable {
        let path: String
    }
    /// A file This thread lists, carrying the thread's own hunks for it.
    struct ThreadDiffTarget: Hashable {
        let file: WorkspaceGitChanges.File
        var location: Location? = nil
        static func == (a: Self, b: Self) -> Bool { a.file.id == b.file.id && a.location == b.location }
        func hash(into hasher: inout Hasher) { hasher.combine(file.id); hasher.combine(location) }
    }
    /// A form opened from a Git action.
    enum ActionForm: Identifiable {
        case commit(String)
        case createPullRequest(String)
        case merge(repository: String, url: URL, number: Int, title: String)
        var id: String {
            switch self {
            case .commit(let id): "commit:" + id
            case .createPullRequest(let id): "pr:" + id
            case .merge(_, let url, _, _): "merge:" + url.absoluteString
            }
        }
    }

    let changes: CodeChanges
    let latestEdits: ResponseEditedFiles?
    let pullRequestError: String?
    let refreshingPullRequests: Bool
    /// Why there are no pull requests to list: couldn't check, GitHub signed
    /// out on the computer, or none yet.
    let pullRequestStatus: ConversationPullRequests?
    let loadChanges: (WorkspaceChangeScope, Location?) async throws -> WorkspaceGitChanges
    let loadThreadChanges: (Location?) async throws -> WorkspaceGitChanges
    let loadDiff: (WorkspaceGitChanges.File, WorkspaceChangeScope, Location?) async throws -> WorkspaceDiffResponse
    let commentContext: DiffCommentContext?
    let stageComment: (ArtifactAnnotation, String) throws -> Void
    let refreshSummary: () async -> Void
    let refreshPullRequests: () async -> Void
    let refreshRepositories: () async -> Void
    let canRunActions: Bool
    let loadMergeState: (ThreadRepositories.Repository, URL) async throws -> PullRequestMergeState
    let stagePullRequestTask: (String) -> Bool

    private enum PullRequestFilter: String, CaseIterable, Identifiable {
        case active = "Active", all = "All"
        var id: Self { self }
    }
    @State private var filter: PullRequestFilter
    @State private var pickedFilter = false
    @StateObject private var actions: GitActionsModel
    @State private var actionForm: ActionForm?
    @State private var tab: CodeChanges.Tab
    /// The person chose a tab, so This thread arriving later doesn't move them.
    @State private var pickedTab = false
    @State private var thread = ThreadListState()
    @State private var lists: [WorkspaceChangeScope: WorkspaceGitChanges] = [:]
    @State private var failures: [WorkspaceChangeScope: String] = [:]
    @State private var loading: Set<WorkspaceChangeScope> = []
    @State private var copied = false
    private let dismiss = ToolDismiss()
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.companionPane) private var pane
    @Environment(\.toolHasGestureDismissal) private var gestureDismissal
    @State private var verticalBar = false
    @State private var path = NavigationPath()
    private var inlineHeader: Bool { CompanionPaneChrome.usesInlineHeader(pane, verticalBar: verticalBar) }

    init(changes: CodeChanges, latestEdits: ResponseEditedFiles?, pullRequestError: String?, refreshingPullRequests: Bool,
         pullRequestStatus: ConversationPullRequests? = nil,
         commentContext: DiffCommentContext? = nil,
         stageComment: @escaping (ArtifactAnnotation, String) throws -> Void = { _, _ in throw FileFailure.unsupported },
         loadChanges: @escaping (WorkspaceChangeScope, Location?) async throws -> WorkspaceGitChanges,
         loadThreadChanges: @escaping (Location?) async throws -> WorkspaceGitChanges = { _ in throw FileFailure.unsupported },
         loadDiff: @escaping (WorkspaceGitChanges.File, WorkspaceChangeScope, Location?) async throws -> WorkspaceDiffResponse,
         refreshSummary: @escaping () async -> Void, refreshPullRequests: @escaping () async -> Void,
         refreshRepositories: @escaping () async -> Void = {},
         runGitAction: ((GitActionRequest) async throws -> GitActionOutcome)? = nil,
         loadMergeState: @escaping (ThreadRepositories.Repository, URL) async throws -> PullRequestMergeState = { _, _ in throw FileFailure.unsupported },
         stagePullRequestTask: @escaping (String) -> Bool = { _ in false }) {
        self.changes = changes; self.latestEdits = latestEdits
        self.pullRequestError = pullRequestError; self.refreshingPullRequests = refreshingPullRequests
        self.pullRequestStatus = pullRequestStatus
        self.loadChanges = loadChanges; self.loadThreadChanges = loadThreadChanges; self.loadDiff = loadDiff
        self.commentContext = commentContext; self.stageComment = stageComment
        self.refreshSummary = refreshSummary; self.refreshPullRequests = refreshPullRequests
        self.refreshRepositories = refreshRepositories
        self.canRunActions = runGitAction != nil
        self.loadMergeState = loadMergeState; self.stagePullRequestTask = stagePullRequestTask
        _actions = StateObject(wrappedValue: GitActionsModel(
            perform: { request in
                guard let runGitAction else { throw FileFailure.unsupported }
                return try await runGitAction(request)
            },
            describe: { CodeChangesSheet.message($0, fallback: "Your computer couldn't run this. Try again.") }))
        _tab = State(initialValue: changes.defaultTab)
        _filter = State(initialValue: pullRequestStatus?.active.isEmpty == false ? .active : .all)
    }

    /// Changes when the Mac reports new work (a finished turn, a refresh), so
    /// an open list reloads instead of polling.
    private var summaryRevision: String {
        guard let summary = changes.git else { return "-" }
        return [summary.head ?? "-", summary.branch ?? "-",
         summary.uncommitted.map { "\($0.files)/\($0.additions)/\($0.deletions)" } ?? "-",
         summary.branchChanges.map { "\($0.files)/\($0.additions)/\($0.deletions)" } ?? "-"].joined(separator: "|")
    }

    private var multiRepository: Bool { !changes.repositories.isEmpty }

    var body: some View {
        NavigationStack(path: $path) {
            WonderList {
                pullRequestStatusSection
                if let status = pullRequestStatus, !status.items.isEmpty {
                    Section {
                        if typeSize.isAccessibilitySize {
                            pullRequestFilter.pickerStyle(.menu).labelsHidden()
                        } else {
                            pullRequestFilter.pickerStyle(.segmented)
                        }
                    }
                }
                pullRequestSection
                if let status = pullRequestStatus, status.items.isEmpty,
                   status.problemMessage == nil, let message = status.statusMessage {
                    Section { Text(message).foregroundStyle(.secondary) }
                }
                if let status = pullRequestStatus, status.canCreate {
                    if let repo = changes.primaryRepository, offersActions(repo) {
                        Section {
                            Button("Create pull request", systemImage: "arrow.triangle.pull") {
                                actionForm = .createPullRequest(repo.id)
                            }.accessibilityIdentifier("pull-requests-create")
                            if let result = actions.status(repo.id) {
                                ActionStatusRow(status: result, identifier: "repo-action-status:\(repo.id)")
                            }
                        }
                    } else {
                        Section {
                            Text("This branch has no pull request yet.").foregroundStyle(.secondary)
                            Button("Ask the agent to open a pull request") {
                                let branch = status.branchName.map { " from branch `\($0)`" } ?? ""
                                if stagePullRequestTask("Open a pull request\(branch). Check the changes and prepare it for review.") { dismiss() }
                            }.accessibilityIdentifier("pull-requests-create")
                        }
                    }
                }
            }
            .listSectionSpacing(12)
            .refreshable {
                await refreshPullRequests()
                await refreshRepositories()
            }
            .accessibilityIdentifier("pull-requests")
            .navigationTitle("Pull requests")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                if !inlineHeader && (pane != nil || !gestureDismissal) {
                    ToolbarItem(placement: .confirmationAction) {
                        HStack {
                            doneButton
                            CompanionExpandButton()
                        }
                    }
                }
            }
            .modifier(paneHeader("Pull requests"))
            .navigationDestination(for: PullRequestTarget.self) { target in
                if let pr = pullRequestStatus?.items.first(where: { $0.id == target.id }) {
                    PullRequestDetailView(pr: pr, error: pullRequestError, refreshing: refreshingPullRequests,
                                          refresh: refreshPullRequests)
                        .modifier(paneHeader("Pull request #\(pr.number)", showsBack: true))
                } else {
                    // A refresh can remove the selected PR; keep a useful way out.
                    WonderList {
                        Text("This pull request is no longer listed.")
                        if let url = URL(string: target.id) { Link("Open on GitHub", destination: url) }
                    }
                    .navigationTitle("Pull request")
                    .navigationBarTitleDisplayMode(.inline)
                    .accessibilityIdentifier("pull-request-no-longer-listed")
                    .modifier(paneHeader("Pull request", showsBack: true))
                }
            }
            .navigationDestination(for: RepositoryTarget.self) { target in
                if let repo = changes.checkouts.first(where: { $0.id == target.id }), let location = repo.location {
                    RepositoryChangesView(repository: repo, location: location, threadSupported: changes.threadSupported,
                                          loadChanges: loadChanges, loadThreadChanges: loadThreadChanges)
                        .modifier(paneHeader(repo.checkoutName, showsBack: true))
                } else {
                    WonderList { Text("This repository is no longer listed.") }
                        .navigationTitle("Changes")
                        .navigationBarTitleDisplayMode(.inline)
                        .modifier(paneHeader("Changes", showsBack: true))
                }
            }
            .navigationDestination(for: DiffTarget.self) { target in
                FileDiffView(name: target.file.name, path: target.file.path, additions: target.file.additions,
                             deletions: target.file.deletions, binary: target.file.binary,
                             // Line comments pin the thread's own folder; other repositories read only.
                             commentContext: target.location == nil ? commentContext : nil, comparison: target.scope,
                             originalPath: target.file.originalPath, stageComment: stageComment) {
                    let response = try await loadDiff(target.file, target.scope, target.location)
                    return (response.diff.isEmpty ? [] : [response.diff], nil, response.annotationVersion == 1)
                }
                .modifier(paneHeader(target.file.name, showsBack: true))
            }
            .navigationDestination(for: ThreadDiffTarget.self) { target in
                ThreadFileDiff(file: target.file)
                    .modifier(paneHeader(target.file.name, showsBack: true))
            }
            .navigationDestination(for: ReplyTarget.self) { target in
                if let file = latestEdits?.files.first(where: { $0.path == target.path }) {
                    FileDiffView(name: file.name, path: file.path, additions: file.additions, deletions: file.deletions, binary: false) {
                        if file.patches.isEmpty { throw FileDiffView.Unavailable(message: DiffDocument.filenameOnlyNote) }
                        return (file.patches, file.partial ? "Only the first part of this diff was saved. Review the full change on your computer." : nil, false)
                    }
                    .modifier(paneHeader(file.name, showsBack: true))
                }
            }
        }
        .readsVerticalBar($verticalBar)
        .sheet(item: $actionForm) { form in actionFormView(form) }
        .confirmationDialog(actions.confirmation.map { "Push to \(repository($0.key)?.defaultBranch ?? "the default branch")?" } ?? "",
                            isPresented: Binding(get: { actions.confirmation != nil }, set: { if !$0 { actions.confirmation = nil } }),
                            titleVisibility: .visible) {
            Button("Push", role: .destructive) { actions.confirm() }
                .accessibilityIdentifier("git-push-confirm")
        } message: {
            Text(actions.confirmation?.message ?? "")
        }
        // Results nobody will see are not waited for.
        .onDisappear { actions.cancelAll() }
        .onChange(of: pullRequestStatus?.active.isEmpty) { _, empty in
            if !pickedFilter, let empty { filter = empty ? .all : .active }
        }
        // Opening asks for current pull request states and repositories; the
        // Mac answers from its cache when it read them moments ago.
        .task {
            await refreshPullRequests()
            await refreshRepositories()
        }
    }

    private var pullRequestFilter: some View {
        Picker("Pull requests", selection: Binding(get: { filter }, set: { filter = $0; pickedFilter = true })) {
            ForEach(PullRequestFilter.allCases) { Text($0.rawValue).tag($0) }
        }
        .accessibilityIdentifier("pull-requests-filter")
    }

    private var doneButton: some View {
        ToolCloseControl {
            Button { dismiss() } label: {
                Text("Close").frame(minWidth: inlineHeader ? 44 : nil, minHeight: inlineHeader ? 44 : nil)
                    .contentShape(Rectangle())
            }.accessibilityIdentifier("pull-requests-done")
        }
    }

    private func paneHeader(_ title: String, showsBack: Bool = false) -> some ViewModifier {
        CompanionPaneNavigationHeader(enabled: inlineHeader, title: title, showsBack: showsBack,
                                      back: goBack) { doneButton }
    }

    private func goBack() {
        if !path.isEmpty { path.removeLast() }
    }

    // MARK: Single repository (today's layout)

    @ViewBuilder private var singleRepository: some View {
        if let git = changes.git { branchSection(git) }
        if let reason = changes.noGitInfoReason {
            Section {
                Text(reason)
                    .font(.subheadline).foregroundStyle(.secondary)
                    .accessibilityIdentifier("changes-no-git-info")
            }
        }
        if let repo = changes.primaryRepository, offersActions(repo) {
            Section { repositoryActions(repo) }
        }
        if changes.tabs.count > 1 {
            Section {
                Picker("Changes", selection: Binding(get: { tab }, set: { tab = $0; pickedTab = true })) {
                    ForEach(changes.tabs) { Text($0.title).tag($0) }
                }
                .pickerStyle(.segmented)
                .accessibilityIdentifier("changes-scope")
                .listRowBackground(Color.clear)
                .listRowInsets(EdgeInsets(top: 4, leading: 0, bottom: 4, trailing: 0))
            }
        }
        switch tab {
        case .thread:
            pullRequestSection
            ThreadChangeList(state: thread, location: nil) { Task { await loadThread() } }
        case .pullRequests: pullRequestSection
        case .lastReply: lastReply
        case .uncommitted: changeList(.uncommitted)
        case .branch: changeList(.branch)
        }
    }

    // MARK: Pull requests

    /// With no pull requests to list, says why when it isn't simply "none":
    /// the computer couldn't check, GitHub is signed out there, this app
    /// can't read its answer, or a refresh of an empty list failed. Try again
    /// when that may have changed.
    @ViewBuilder private var pullRequestStatusSection: some View {
        if let status = pullRequestStatus, status.items.isEmpty, let message = status.problemMessage {
            Section {
                Label(message, systemImage: "exclamationmark.triangle")
                    .font(.subheadline).foregroundStyle(.secondary)
                    .accessibilityIdentifier("pull-requests-unavailable")
                if status.offersRetry {
                    Button("Try again") { Task { await refreshPullRequests() } }
                        .disabled(refreshingPullRequests)
                        .accessibilityIdentifier("pull-requests-retry")
                }
            }
        }
    }

    @ViewBuilder private var pullRequestSection: some View {
        if let status = pullRequestStatus, !status.items.isEmpty {
            let items = filter == .active ? status.active : status.items
            Section {
                if items.isEmpty {
                    Text("No active pull requests.").foregroundStyle(.secondary)
                        .accessibilityIdentifier("pull-requests-empty")
                }
                if let pullRequestError {
                    Label(pullRequestError, systemImage: "exclamationmark.triangle")
                        .font(.subheadline).foregroundStyle(.secondary)
                        .accessibilityIdentifier("pull-requests-error")
                    Button("Try again") { Task { await refreshPullRequests() } }
                        .disabled(refreshingPullRequests)
                        .accessibilityIdentifier("pull-requests-retry")
                }
                ForEach(items) { pr in
                    let layout = typeSize.isAccessibilitySize
                        ? AnyLayout(VStackLayout(alignment: .leading, spacing: 8))
                        : AnyLayout(HStackLayout(spacing: 8))
                    layout {
                        NavigationLink(value: PullRequestTarget(id: pr.id)) { PullRequestRow(pr: pr) }
                            .buttonStyle(.plain)
                            .accessibilityIdentifier("pull-request-row:\(pr.number)")
                            .accessibilityHint("Shows checks, reviews and merge status")
                        pullRequestMenu(number: pr.number, url: pr.url, title: pr.title, state: pr.state, checks: pr.checks.state,
                                        conflicting: pr.details?.mergeable == "CONFLICTING",
                                        repository: ([changes.primaryRepository].compactMap { $0 } + changes.checkouts).first { $0.remote == pr.repository && offersActions($0) },
                                        branch: pr.headRef)
                    }
                    if let status = actions.status(pr.id) { ActionStatusRow(status: status, identifier: "pull-request-action-status:\(pr.number)") }
                }
            } footer: {
                pullRequestLimitNote
            }
        }
    }

    /// Says the list keeps only the newest of a thread's many pull requests.
    @ViewBuilder var pullRequestLimitNote: some View {
        if let note = pullRequestStatus?.limitNote {
            Text(note).accessibilityIdentifier("pull-requests-limit")
        }
    }

    // MARK: Branch

    private func branchSection(_ git: WorkspaceGitSummary) -> some View {
        Section {
            VStack(alignment: .leading, spacing: 6) {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Image(systemName: git.worktree ? "tree" : "arrow.triangle.branch").foregroundStyle(.secondary)
                    // "agent/xyz → main": the base in the secondary colour, kept with its
                    // arrow so a long name wraps before "→", not after it.
                    Text("\(Text(git.title).fontWeight(.semibold))\(Text(git.baseRef.map { " →\u{00A0}" + $0 } ?? "").foregroundStyle(.secondary))")
                        .lineLimit(typeSize.isAccessibilitySize ? 5 : 2).truncationMode(.middle)
                        .accessibilityLabel(git.comparisonTitle)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    if git.branch != nil {
                        Button {
                            UIPasteboard.general.string = git.title
                            copied = true
                            Task { try? await Task.sleep(for: .seconds(1.5)); copied = false }
                        } label: {
                            Image(systemName: copied ? "checkmark" : "doc.on.doc")
                                .frame(width: 32, height: 32)
                                .contentShape(Rectangle())
                        }
                        .buttonStyle(.borderless)
                        .accessibilityLabel(copied ? "Branch name copied" : "Copy branch name")
                        .accessibilityIdentifier("changes-copy-branch")
                    }
                }
                if let detail = [git.worktree ? "Worktree" : nil, git.aheadBehindText].compactMap({ $0 }).joined(separator: " · ").nilIfEmpty {
                    Text(detail).font(.caption).foregroundStyle(.secondary)
                }
            }
            .padding(.vertical, 2)
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("changes-branch")
        }
    }

    // MARK: Last reply

    @ViewBuilder private var lastReply: some View {
        if let latestEdits, !latestEdits.files.isEmpty, let edits = changes.replyEdits {
            Section {
                ForEach(latestEdits.files) { file in
                    NavigationLink(value: ReplyTarget(path: file.path)) {
                        EditedFileRow(name: file.name, folder: FileChangeSummary.relativePath(file.path).flatMap {
                            let folder = ($0 as NSString).deletingLastPathComponent
                            return folder.isEmpty ? nil : folder
                        }, additions: file.additions, deletions: file.deletions)
                    }
                    .accessibilityIdentifier("reply-file:" + file.path)
                }
            } header: {
                Text((changes.lastReplyChanged ? "Last reply" : "An earlier reply") + " · " + totalsText(files: edits.files, additions: edits.additions, deletions: edits.deletions))
                    .accessibilityIdentifier("changes-last-reply-header")
            }
        } else {
            Section {
                Text("No edits in the last reply")
                    .font(.subheadline).foregroundStyle(.secondary)
                    .accessibilityIdentifier("changes-last-reply-empty")
            }
        }
    }

    // MARK: Uncommitted and Branch

    private func changeList(_ scope: WorkspaceChangeScope) -> some View {
        ScopedChangeList(scope: scope, list: lists[scope], failure: failures[scope], loading: loading.contains(scope),
                         baseRef: changes.git?.baseRef, location: nil) { Task { await load(scope) } }
    }

    private func totalsText(files: Int, additions: Int?, deletions: Int?) -> String {
        Self.totalsText(files: files, additions: additions, deletions: deletions)
    }

    static func totalsText(files: Int, additions: Int?, deletions: Int?) -> String {
        var text = "\(files.formatted()) \(files == 1 ? "file" : "files")"
        if let additions { text += " · +\(additions.formatted())" }
        if let deletions { text += " −\(deletions.formatted())" }
        return text
    }

    private func loadThread() async {
        thread.loading = true
        defer { thread.loading = false }
        let result = await ThreadListState.read { try await loadThreadChanges(nil) }
        guard !Task.isCancelled else { return }
        thread.apply(result)
    }

    private func load(_ scope: WorkspaceChangeScope) async {
        loading.insert(scope)
        defer { loading.remove(scope) }
        do {
            let changes = try await loadChanges(scope, nil)
            guard !Task.isCancelled else { return }
            lists[scope] = changes
            failures[scope] = nil
        } catch is CancellationError {
        } catch {
            guard !Task.isCancelled else { return }
            failures[scope] = Self.message(error, fallback: "Changes could not be loaded from your computer.")
        }
    }

    static func message(_ error: Error, fallback: String) -> String {
        if case PairingFailure.hostMessage(_, let detail) = error { return detail }
        if case PairingFailure.response(let code) = error {
            switch code {
            case 401: return "Your computer connection expired. Reconnect, then try again."
            case 403: return "Access to this folder changed. Review the Project's folders on your computer."
            case 413: return "Too large to show here — open it on your computer."
            case 504: return "Git took too long on your computer."
            default: break
            }
        }
        return fallback
    }
}

// MARK: Several repositories

extension CodeChangesSheet {
    /// One repository: its name and roles, branch and base, how it stands with
    /// what it pushes to, uncommitted counts, its pull requests, and for a
    /// repository inside a Project folder a way into its changes.
    /// Worktrees of the repository the thread worked in follow, each with its
    /// own branch, counts, actions, pull requests and changes.
    @ViewBuilder func repositorySection(_ repo: ThreadRepositories.Repository) -> some View {
        Section {
            RepositoryHeader(repository: repo)
            checkoutContents(repo)
            ForEach(repo.worktrees) { worktree in
                RepositoryHeader(repository: worktree, nested: true)
                checkoutContents(worktree)
            }
        }
    }

    /// One checkout's actions, pull requests and way into its changes.
    @ViewBuilder func checkoutContents(_ repo: ThreadRepositories.Repository) -> some View {
        if offersActions(repo) { repositoryActions(repo) }
        ForEach(repo.pullRequests) { pr in
            HStack(spacing: 8) {
                if changes.pullRequests.contains(where: { $0.id == pr.id }) {
                    NavigationLink(value: PullRequestTarget(id: pr.id)) { RepositoryPullRequestRow(pr: pr) }
                        .accessibilityIdentifier("repo-pull-request:\(pr.repository)#\(pr.number)")
                        .accessibilityHint("Shows checks, reviews and merge status")
                } else {
                    Link(destination: pr.url) { RepositoryPullRequestRow(pr: pr) }
                        .buttonStyle(.plain)
                        .accessibilityIdentifier("repo-pull-request:\(pr.repository)#\(pr.number)")
                        .accessibilityHint("Opens on GitHub")
                }
                let known = changes.pullRequests.first { $0.id == pr.id }
                pullRequestMenu(number: pr.number, url: pr.url, title: pr.displayTitle, state: pr.state, checks: pr.checks,
                                conflicting: known?.details?.mergeable == "CONFLICTING",
                                repository: offersActions(repo) ? repo : nil, branch: pr.relation == .opened ? repo.branch : nil)
            }
            if let status = actions.status(pr.id) {
                ActionStatusRow(status: status, identifier: "pull-request-action-status:\(pr.number)")
            }
        }
        if repo.opensChanges {
            NavigationLink(value: RepositoryTarget(id: repo.id)) {
                Label(changes.threadSupported ? "This chat and checkout changes" : "Uncommitted and branch changes",
                      systemImage: "doc.text.magnifyingglass")
            }
            .accessibilityLabel(repo.worktree.map { "Changes in worktree \($0)" } ?? "Changes in \(repo.name)")
            .accessibilityIdentifier(repo.worktree.map { "repo-worktree-changes:\($0)" } ?? "repo-changes:\(repo.name)")
        }
    }
}

// MARK: Git actions

extension CodeChangesSheet {
    func offersActions(_ repo: ThreadRepositories.Repository) -> Bool { canRunActions && repo.actionsAvailable }

    func repository(_ id: String) -> ThreadRepositories.Repository? {
        changes.checkouts.first { $0.id == id } ?? changes.primaryRepository.flatMap { $0.id == id ? $0 : nil }
    }

    /// The repository's one suggested action as a button, every action in a
    /// menu that says why one is unavailable, then the latest result.
    @ViewBuilder func repositoryActions(_ repo: ThreadRepositories.Repository) -> some View {
        let availability = GitActionAvailability(repo, githubAvailable: changes.githubAvailable, busy: actions.isRunning(repo.id))
        HStack(spacing: 10) {
            if let quick = availability.quick {
                Button(quick.title) { start(quick.action, repo, availability) }
                    .buttonStyle(.wonderProminent).controlSize(.small)
                    .accessibilityIdentifier("repo-quick-action:\(repo.checkoutName)")
                    .accessibilityLabel("\(quick.title.replacingOccurrences(of: "…", with: "")) in \(repo.checkoutName)")
            }
            Spacer(minLength: 0)
            Menu {
                ForEach([availability.commit, availability.push, availability.pull, availability.createPullRequest], id: \.action) { item in
                    Button { start(item.action, repo, availability) } label: {
                        Text(item.action == .push && availability.pushesToDefaultBranch ? "Push to \(repo.defaultBranch ?? "main")…" : item.title)
                        if let reason = item.disabledReason { Text(reason) }
                    }
                    .disabled(!item.enabled)
                    .accessibilityIdentifier("git-action:\(item.action.rawValue)")
                }
            } label: {
                Label("Git actions", systemImage: "ellipsis.circle").font(.subheadline)
            }
            .buttonStyle(.borderless)
            .accessibilityLabel("Git actions for \(repo.checkoutName)")
            .accessibilityIdentifier("repo-actions:\(repo.checkoutName)")
        }
        if let status = actions.status(repo.id) {
            ActionStatusRow(status: status, identifier: "repo-action-status:\(repo.checkoutName)")
        }
    }

    private func start(_ action: GitAction, _ repo: ThreadRepositories.Repository, _ availability: GitActionAvailability) {
        switch action {
        case .commit: actionForm = .commit(repo.id)
        case .createPullRequest: actionForm = .createPullRequest(repo.id)
        case .push, .pull:
            guard let request = GitActionRequest(repository: repo, action: action) else { return }
            if action == .push && availability.pushesToDefaultBranch {
                // Ask first; the Mac refuses an unconfirmed push there too.
                actions.confirmation = .init(key: repo.id, request: request,
                    message: "This pushes straight to \(repo.defaultBranch ?? "main"), the default branch, without a pull request.")
            } else {
                actions.run(request, key: repo.id)
            }
        case .merge, .autoMerge: break
        }
    }

    /// Merge and agent requests for one pull request. Merging needs a
    /// repository Wonder may write; agent requests only fill the composer.
    @ViewBuilder func pullRequestMenu(number: Int, url: URL, title: String, state: ThreadPullRequests.PullRequest.State?,
                                      checks: ThreadPullRequests.PullRequest.Checks.State?, conflicting: Bool,
                                      repository: ThreadRepositories.Repository?, branch: String? = nil) -> some View {
        let tasks = PullRequestAgentTask.available(state: state, checks: checks, conflicting: conflicting)
        let canMerge = repository != nil && (state == .open || state == .draft)
        if canMerge || !tasks.isEmpty {
            Menu {
                if canMerge {
                    // The form pins Merge and auto-merge to the commit it shows;
                    // GitHub stays available for a Mac that doesn't report one.
                    if let repository {
                        Button("Review and merge…", systemImage: "arrow.triangle.merge") {
                            actionForm = .merge(repository: repository.id, url: url, number: number, title: title)
                        }
                        .accessibilityIdentifier("pull-request-merge-review:\(number)")
                    }
                    Link("Merge on GitHub", destination: url)
                        .accessibilityIdentifier("pull-request-merge:\(number)")
                }
                ForEach(tasks) { task in
                    Button(task.title, systemImage: task.systemImage) {
                        stage(task.prompt(number: number, url: url, branch: branch, base: nil))
                    }
                    .accessibilityIdentifier("pull-request-task:\(task.rawValue)")
                }
            } label: {
                Image(systemName: "ellipsis.circle").font(.system(size: 20)).frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
            }
            .buttonStyle(.borderless)
            .accessibilityLabel("Actions for pull request \(number)")
            .accessibilityIdentifier("pull-request-actions:\(number)")
        }
    }

    /// Puts the request in the composer and closes the sheet so it can be edited and sent.
    private func stage(_ prompt: String) {
        if stagePullRequestTask(prompt) { dismiss() }
    }

    @ViewBuilder func actionFormView(_ form: ActionForm) -> some View {
        switch form {
        case .commit(let id):
            if let repo = repository(id) {
                CommitForm(repository: repo) { message, includeUnstaged in
                    if let request = GitActionRequest.commit(repo, message: message, includeUnstaged: includeUnstaged) {
                        actions.run(request, key: repo.id)
                    }
                }
            }
        case .createPullRequest(let id):
            if let repo = repository(id) {
                CreatePullRequestForm(repository: repo) { title, body, base, draft in
                    if let request = GitActionRequest.createPullRequest(repo, title: title, body: body, base: base, draft: draft) {
                        actions.run(request, key: repo.id)
                    }
                }
            }
        case .merge(let id, let url, let number, let title):
            if let repo = repository(id) {
                PullRequestMergeForm(repository: repo, url: url, number: number, title: title, actions: actions,
                                     load: { try await loadMergeState(repo, url) }) { prompt in
                    if stagePullRequestTask(prompt) { actionForm = nil; dismiss() }
                }
            }
        }
    }
}

/// A Git action's progress, success or failure, kept under its control.
private struct ActionStatusRow: View {
    let status: GitActionStatus
    let identifier: String
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            switch status.phase {
            case .running: ProgressView().controlSize(.small)
            case .succeeded: Image(systemName: "checkmark.circle.fill").foregroundStyle(DiffColors.added(scheme))
            case .failed: Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
            }
            Text(status.message).font(.footnote).foregroundStyle(status.phase == .failed ? .primary : .secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(status.message)
        .accessibilityValue(status.phase == .running ? "In progress" : status.phase == .succeeded ? "Done" : "Failed")
        .accessibilityIdentifier(identifier)
    }
}

/// Commit: a message, and whether unstaged and new files go in too.
private struct CommitForm: View {
    let repository: ThreadRepositories.Repository
    let commit: (String, Bool) -> Void
    @State private var message = ""
    @State private var includeUnstaged: Bool
    @FocusState private var focused: Bool
    @Environment(\.dismiss) private var dismiss

    init(repository: ThreadRepositories.Repository, commit: @escaping (String, Bool) -> Void) {
        self.repository = repository; self.commit = commit
        // With nothing staged, committing staged changes alone would do nothing.
        _includeUnstaged = State(initialValue: (repository.changes?.staged ?? 0) == 0)
    }

    private var trimmed: String { message.trimmingCharacters(in: .whitespacesAndNewlines) }

    var body: some View {
        NavigationStack {
            WonderList {
                Section {
                    TextField("Commit message", text: $message, axis: .vertical)
                        .lineLimit(2...8).focused($focused)
                        .accessibilityIdentifier("commit-message")
                }
                Section {
                    Toggle("Include unstaged changes", isOn: $includeUnstaged)
                        .systemSwitch()
                        .accessibilityIdentifier("commit-include-unstaged")
                } footer: {
                    Text((includeUnstaged
                          ? "Commits every change in \(repository.checkoutName), new files included, except what .gitignore excludes."
                          : "Commits only the changes already staged.")
                         + " Git hooks don't run when committing from Cupola.")
                }
            }
            .navigationTitle(repository.branch.map { "Commit to \($0)" } ?? "Commit")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Commit") { commit(trimmed, includeUnstaged); dismiss() }
                        .disabled(trimmed.isEmpty)
                        .accessibilityIdentifier("commit-confirm")
                }
            }
            .onAppear { focused = true }
        }
        .presentationDetents([.medium, .large]).presentationDragIndicator(.hidden)
    }
}

/// Create pull request: title, description, base branch and draft.
private struct CreatePullRequestForm: View {
    let repository: ThreadRepositories.Repository
    let create: (String, String, String?, Bool) -> Void
    @State private var title = ""
    @State private var detail = ""
    @State private var base: String
    @State private var draft = false
    @Environment(\.dismiss) private var dismiss

    init(repository: ThreadRepositories.Repository, create: @escaping (String, String, String?, Bool) -> Void) {
        self.repository = repository; self.create = create
        _base = State(initialValue: repository.defaultBranch ?? "main")
    }

    private var trimmed: String { title.trimmingCharacters(in: .whitespacesAndNewlines) }

    var body: some View {
        NavigationStack {
            WonderList {
                Section {
                    TextField("Title", text: $title).accessibilityIdentifier("pull-request-title")
                    TextField("Description", text: $detail, axis: .vertical).lineLimit(3...10)
                        .accessibilityIdentifier("pull-request-body")
                }
                Section {
                    LabeledContent("Into") {
                        TextField("Base branch", text: $base).multilineTextAlignment(.trailing)
                            .textInputAutocapitalization(.never).autocorrectionDisabled()
                            .accessibilityIdentifier("pull-request-base")
                    }
                    Toggle("Draft", isOn: $draft).systemSwitch().accessibilityIdentifier("pull-request-draft")
                } footer: {
                    Text("Opens \(repository.branch ?? "this branch") on GitHub with your computer's GitHub CLI sign-in.")
                }
            }
            .navigationTitle("Create pull request")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button(draft ? "Create draft" : "Create") {
                        create(trimmed, detail, base.trimmingCharacters(in: .whitespaces), draft); dismiss()
                    }
                    .disabled(trimmed.isEmpty)
                    .accessibilityIdentifier("pull-request-create")
                }
            }
        }
        .presentationDetents([.medium, .large]).presentationDragIndicator(.hidden)
    }
}

/// Merge controls for one pull request: why it can't merge yet, the merge
/// method the repository allows, Merge, auto-merge, and agent requests.
private struct PullRequestMergeForm: View {
    let repository: ThreadRepositories.Repository
    let url: URL
    let number: Int
    let title: String
    @ObservedObject var actions: GitActionsModel
    let load: () async throws -> PullRequestMergeState
    let stage: (String) -> Void
    @State private var state: PullRequestMergeState?
    @State private var failure: String?
    @State private var method: MergeMethod = .squash
    /// The method and commit the owner is confirming; a later read can't change them.
    @State private var confirming: MergeApproval?
    @State private var newCommitsNote: String?
    @State private var attempt = 0
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase

    private var key: String { url.absoluteString }
    private var running: Bool { actions.isRunning(key) }

    var body: some View {
        NavigationStack {
            WonderList {
                Section {
                    Text(title).font(.headline)
                    if let state {
                        if let blocked = state.blocked {
                            Label(blocked, systemImage: "exclamationmark.circle").foregroundStyle(.secondary)
                                .accessibilityIdentifier("merge-blocked")
                        } else if let note = state.headNote, state.state == .open || state.state == .draft {
                            Label(note, systemImage: "exclamationmark.circle").foregroundStyle(.secondary)
                                .accessibilityIdentifier("merge-head-unknown")
                        } else {
                            Label("Ready to merge", systemImage: "checkmark.circle").accessibilityIdentifier("merge-ready")
                        }
                        if let newCommitsNote {
                            Label(newCommitsNote, systemImage: "arrow.triangle.2.circlepath").foregroundStyle(.secondary)
                                .accessibilityIdentifier("merge-new-commits")
                        }
                    } else if let failure {
                        Label(failure, systemImage: "exclamationmark.triangle").foregroundStyle(.secondary)
                            .accessibilityIdentifier("merge-state-error")
                        Button("Try again") { attempt += 1 }
                    } else {
                        ProgressView().frame(maxWidth: .infinity).accessibilityIdentifier("merge-state-loading")
                    }
                }
                if let state, state.state == .open || state.state == .draft {
                    if state.methods.count > 1 {
                        Section("Merge method") {
                            Picker("Merge method", selection: $method) {
                                ForEach(state.methods) { Text($0.title).tag($0) }
                            }
                            .pickerStyle(.inline).labelsHidden()
                            .accessibilityIdentifier("merge-method")
                        }
                    }
                    Section {
                        let mergeBlocked = !state.canMerge || running
                        // The theme tint hides SwiftUI's default disabled look; dim it explicitly.
                        Button("Merge pull request", systemImage: "arrow.triangle.merge") {
                            if let approval = MergeApproval(method: method, state: state) { newCommitsNote = nil; confirming = approval }
                        }
                            .disabled(mergeBlocked)
                            .foregroundStyle(mergeBlocked ? AnyShapeStyle(.secondary) : AnyShapeStyle(.tint))
                            .accessibilityIdentifier("merge-confirm-open")
                        Button(state.autoMerge ? "Turn off auto-merge" : "Merge when ready", systemImage: "clock.arrow.circlepath") {
                            if let request = GitActionRequest.autoMerge(repository, pullRequest: url, enabled: !state.autoMerge,
                                                                       method: state.autoMerge ? nil : method,
                                                                       reviewedHead: state.reviewedHead) {
                                actions.run(request, key: key)
                            }
                        }
                        .disabled(running || (!state.autoMerge && !state.canAutoMerge))
                        .foregroundStyle(running || (!state.autoMerge && !state.canAutoMerge) ? AnyShapeStyle(.secondary) : AnyShapeStyle(.tint))
                        .accessibilityIdentifier("merge-auto")
                    } footer: {
                        Text(state.autoMerge
                             ? "GitHub merges this when its required checks and reviews pass."
                             : "Merge when ready turns on GitHub's auto-merge: it merges once required checks and reviews pass, which may be right away.")
                    }
                    let tasks = PullRequestAgentTask.available(state: state.state, checks: state.checks, conflicting: state.conflicting)
                    Section {
                        ForEach(tasks) { task in
                            Button(task.title, systemImage: task.systemImage) {
                                stage(task.prompt(number: number, url: url, branch: state.headRef, base: state.baseRef))
                            }
                            .accessibilityIdentifier("merge-task:\(task.rawValue)")
                        }
                    } header: { Text("Ask the agent") } footer: {
                        Text("Adds a request to your message to edit and send.")
                    }
                }
                if let status = actions.status(key) {
                    Section { ActionStatusRow(status: status, identifier: "merge-action-status") }
                }
            }
            .navigationTitle("Pull request #\(number)")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() }.accessibilityIdentifier("merge-done") }
            }
            .confirmationDialog("Merge #\(number)?", isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } }),
                                titleVisibility: .visible, presenting: confirming) { pending in
                Button(pending.method.title) {
                    if let request = pending.request(repository, pullRequest: url) { actions.run(request, key: key) }
                }
                .accessibilityIdentifier("merge-confirm")
            } message: { _ in
                Text("Merges the commits you reviewed into \(state?.baseRef ?? "its base branch") on GitHub.")
            }
            .task(id: attempt) {
                failure = nil
                do {
                    let loaded = try await load()
                    guard !Task.isCancelled else { return }
                    // The repository's default first; a later read keeps the owner's choice.
                    if state == nil || !loaded.methods.contains(method),
                       let preferred = loaded.defaultMethod ?? loaded.methods.first {
                        method = preferred
                    }
                    // New commits void an open confirmation: show them and ask again.
                    if let pending = confirming, !pending.holds(for: loaded) { confirming = nil }
                    if let shown = state, shown.reviewedHead != loaded.reviewedHead {
                        newCommitsNote = "New commits arrived since you opened this pull request. Review them, then merge."
                    }
                    state = loaded
                } catch is CancellationError {
                } catch {
                    guard !Task.isCancelled else { return }
                    failure = CodeChangesSheet.message(error, fallback: "Merge status couldn't be read from your computer.")
                }
            }
            // Coming back to the app reads the pull request again, as the Mac sees it now.
            .onChange(of: scenePhase) { _, phase in if phase == .active { attempt += 1 } }
            // A finished merge or auto-merge change reads the new state.
            .onChange(of: actions.status(key)?.phase) { _, phase in
                // A refusal (new commits, say) reads the state again so the form shows what the Mac sees.
                if phase == .succeeded || phase == .failed { attempt += 1 }
            }
        }
        .presentationDetents([.large])
    }
}

/// A repository's heading: name, role chips and Git facts, read as one element.
/// A worktree listed under its repository (`nested`) is headed by its folder
/// name instead; a repository led by one of its worktrees names it below.
private struct RepositoryHeader: View {
    let repository: ThreadRepositories.Repository
    var nested = false
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        let large = typeSize.isAccessibilitySize
        VStack(alignment: .leading, spacing: 5) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: nested ? "tree" : repository.local ? "folder" : "network").foregroundStyle(.secondary)
                Text(nested ? repository.worktree ?? repository.name : repository.name)
                    .font(nested ? .subheadline.weight(.semibold) : .body.weight(.semibold))
                    .lineLimit(large ? 4 : 1).truncationMode(.middle)
            }
            if !nested, let worktree = repository.worktree {
                Label("Worktree \(worktree)", systemImage: "tree")
                    .font(.subheadline).lineLimit(large ? 4 : 1).truncationMode(.middle)
                    .labelStyle(CompactLabel())
            }
            if !repository.roles.isEmpty {
                RoleChips(roles: repository.roles)
            }
            if let branch = repository.branchTitle {
                Label(branch, systemImage: "arrow.triangle.branch")
                    .font(.subheadline).lineLimit(large ? 5 : 2).truncationMode(.middle)
                    .labelStyle(CompactLabel())
            }
            if let facts = [repository.syncText, repository.changes?.summary, repository.detail].compactMap({ $0 })
                .joined(separator: " · ").nilIfEmpty {
                Text(facts).font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 2)
        .padding(.leading, nested ? 12 : 0)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(nested ? repository.worktreeAccessibilityText : repository.accessibilityText)
        .accessibilityIdentifier(nested ? "repo-worktree:\(repository.worktree ?? repository.name)" : "repo:\(repository.name)")
    }
}

/// An icon beside the subheadline text, closer than the list's default label.
private struct CompactLabel: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            configuration.icon.foregroundStyle(.secondary).font(.caption)
            configuration.title
        }
    }
}

/// Small capsules naming what the thread did with a repository; they wrap
/// onto further lines at large text sizes.
private struct RoleChips: View {
    let roles: [ThreadRepositories.Role]
    @Environment(\.wonderTheme) private var theme

    var body: some View {
        WrappingRow(spacing: 6) {
            ForEach(roles, id: \.self) { role in
                Text(role.title)
                    .font(.caption.weight(.medium))
                    .padding(.horizontal, 7).padding(.vertical, 2)
                    .background(theme.accent.opacity(0.14), in: Capsule())
                    .foregroundStyle(role == .workspace ? Color.secondary : theme.accent)
            }
        }
    }
}

/// Lays children left to right, starting a new line when one does not fit.
private struct WrappingRow: Layout {
    var spacing: CGFloat

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let width = proposal.width ?? .infinity
        var x: CGFloat = 0, y: CGFloat = 0, line: CGFloat = 0, widest: CGFloat = 0
        for view in subviews {
            let size = view.sizeThatFits(.unspecified)
            if x > 0, x + size.width > width { y += line + spacing; x = 0; line = 0 }
            x += size.width + spacing
            line = max(line, size.height)
            widest = max(widest, x - spacing)
        }
        return CGSize(width: min(widest, width), height: y + line)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var x = bounds.minX, y = bounds.minY, line: CGFloat = 0
        for view in subviews {
            let size = view.sizeThatFits(.unspecified)
            if x > bounds.minX, x + size.width > bounds.maxX { y += line + spacing; x = bounds.minX; line = 0 }
            view.place(at: CGPoint(x: x, y: y), proposal: ProposedViewSize(size))
            x += size.width + spacing
            line = max(line, size.height)
        }
    }
}

/// A pull request under its repository: state glyph, title, number, and
/// whether the thread opened or is reviewing it.
private struct RepositoryPullRequestRow: View {
    let pr: ThreadRepositories.PullRequest
    @Environment(\.colorScheme) private var scheme
    @Environment(\.dynamicTypeSize) private var typeSize

    private var state: (symbol: String, color: Color) {
        switch pr.state ?? .unknown {
        case .open: ("arrow.triangle.pull", DiffColors.added(scheme))
        case .draft: ("circle.dashed", .secondary)
        case .merged: ("arrow.triangle.merge", .purple)
        case .closed: ("xmark.circle", DiffColors.removed(scheme))
        case .unknown: ("arrow.triangle.pull", .secondary)
        }
    }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: state.symbol).foregroundStyle(state.color).font(.subheadline)
            VStack(alignment: .leading, spacing: 2) {
                Text(pr.displayTitle).lineLimit(typeSize.isAccessibilitySize ? 4 : 1).truncationMode(.tail)
                Text(["#\(pr.number)", pr.state?.title, pr.relation.title].compactMap { $0 }.joined(separator: " · "))
                    .font(.caption).foregroundStyle(.secondary).monospacedDigit()
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if let checks = pr.checks { PullRequestChecksGlyph(state: checks).font(.subheadline) }
        }
        .contentShape(Rectangle())
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(pr.accessibilityText)
    }
}

/// One repository's Uncommitted and Branch changes, read through the same
/// verified readers as the thread's own folder.
private struct RepositoryChangesView: View {
    let repository: ThreadRepositories.Repository
    let location: ThreadRepositories.Location
    let loadChanges: (WorkspaceChangeScope, ThreadRepositories.Location?) async throws -> WorkspaceGitChanges
    let loadThreadChanges: (ThreadRepositories.Location?) async throws -> WorkspaceGitChanges
    /// This thread's own changes in the repository lead when the Mac can list them.
    @State private var tab: CodeChanges.Tab
    @State private var thread = ThreadListState()
    @State private var lists: [WorkspaceChangeScope: WorkspaceGitChanges] = [:]
    @State private var failures: [WorkspaceChangeScope: String] = [:]
    @State private var loading: Set<WorkspaceChangeScope> = []
    private let tabs: [CodeChanges.Tab]

    init(repository: ThreadRepositories.Repository, location: ThreadRepositories.Location, threadSupported: Bool,
         loadChanges: @escaping (WorkspaceChangeScope, ThreadRepositories.Location?) async throws -> WorkspaceGitChanges,
         loadThreadChanges: @escaping (ThreadRepositories.Location?) async throws -> WorkspaceGitChanges) {
        self.repository = repository; self.location = location
        self.loadChanges = loadChanges; self.loadThreadChanges = loadThreadChanges
        tabs = (threadSupported ? [.thread] : []) + [.uncommitted, .branch]
        _tab = State(initialValue: tabs[0])
    }

    var body: some View {
        WonderList {
            Section {
                Picker("Changes", selection: $tab) {
                    ForEach(tabs) { Text($0.title).tag($0) }
                }
                .pickerStyle(.segmented)
                .accessibilityIdentifier("repo-changes-scope")
                .listRowBackground(Color.clear)
                .listRowInsets(EdgeInsets(top: 4, leading: 0, bottom: 4, trailing: 0))
            }
            if let scope = tab.scope {
                ScopedChangeList(scope: scope, list: lists[scope], failure: failures[scope], loading: loading.contains(scope),
                                 baseRef: repository.baseRef, location: location) { Task { await load(scope) } }
            } else {
                ThreadChangeList(state: thread, location: location) { Task { await loadThread() } }
            }
        }
        .listSectionSpacing(12)
        .refreshable { await load(tab) }
        .navigationTitle(repository.checkoutName)
        .navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("repo-changes-view")
        .task(id: tab) { await load(tab) }
    }

    private func load(_ tab: CodeChanges.Tab) async {
        if let scope = tab.scope { await load(scope) } else { await loadThread() }
    }

    private func loadThread() async {
        thread.loading = true
        defer { thread.loading = false }
        let result = await ThreadListState.read { try await loadThreadChanges(location) }
        guard !Task.isCancelled else { return }
        thread.apply(result)
    }

    private func load(_ scope: WorkspaceChangeScope) async {
        loading.insert(scope)
        defer { loading.remove(scope) }
        do {
            let changes = try await loadChanges(scope, location)
            guard !Task.isCancelled else { return }
            lists[scope] = changes
            failures[scope] = nil
        } catch is CancellationError {
        } catch {
            guard !Task.isCancelled else { return }
            failures[scope] = CodeChangesSheet.message(error, fallback: "Changes could not be loaded from your computer.")
        }
    }
}

/// A scope's changed files (or why there are none), each opening its diff.
private struct ScopedChangeList: View {
    let scope: WorkspaceChangeScope
    let list: WorkspaceGitChanges?
    let failure: String?
    let loading: Bool
    let baseRef: String?
    let location: ThreadRepositories.Location?
    let retry: () -> Void
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        if let failure {
            Section {
                Label(failure, systemImage: "exclamationmark.triangle")
                    .font(.subheadline).foregroundStyle(.secondary)
                    .accessibilityIdentifier("changes-error")
                Button("Try again", action: retry)
                    .disabled(loading)
                    .accessibilityIdentifier("changes-retry")
            }
        }
        if let changes = list {
            if !changes.available {
                Section {
                    Text(changes.detail ?? "These changes are unavailable.")
                        .font(.subheadline).foregroundStyle(.secondary)
                        .accessibilityIdentifier("changes-unavailable")
                }
            } else if changes.files.isEmpty {
                Section {
                    Text(scope == .branch ? "No changes on this branch." : "No uncommitted changes.")
                        .font(.subheadline).foregroundStyle(.secondary)
                        .accessibilityIdentifier("changes-empty")
                } header: { header() } footer: { wholeCheckout }
            } else {
                Section {
                    ForEach(changes.files) { file in
                        NavigationLink(value: CodeChangesSheet.DiffTarget(file: file, scope: scope, location: location)) {
                            ChangedFileRow(file: file)
                        }
                        .accessibilityIdentifier("changed-file:" + file.path)
                    }
                } header: {
                    header(totals: changes.totals)
                } footer: {
                    VStack(alignment: .leading, spacing: 4) {
                        if changes.truncated {
                            Text("Showing the first \(changes.files.count.formatted()) files. Review the rest on your computer.")
                        }
                        wholeCheckout
                    }
                }
            }
        } else if failure == nil {
            Section {
                ProgressView().frame(maxWidth: .infinity).listRowBackground(Color.clear)
                    .accessibilityIdentifier("changes-loading")
            }
        }
    }

    /// Uncommitted and Branch read the folder, which other threads and apps share.
    private var wholeCheckout: some View {
        Text("The whole checkout, including changes made outside this chat.")
            .accessibilityIdentifier("changes-whole-checkout")
    }

    private func header(totals: WorkspaceGitSummary.Totals? = nil) -> some View {
        let title = scope == .branch ? (baseRef.map { "Since \($0)" } ?? "Branch") : "Since the last commit"
        let counts = totals.map { CodeChangesSheet.totalsText(files: $0.files, additions: $0.additions, deletions: $0.deletions) }
        // Accessibility sizes wrap the whole header rather than cutting its middle.
        return Text([title, counts].compactMap { $0 }.joined(separator: " · "))
            .lineLimit(typeSize.isAccessibilitySize ? nil : 2).truncationMode(.middle)
            .accessibilityIdentifier("changes-header")
    }
}

/// This thread's list as the sheet last read it.
@MainActor private struct ThreadListState {
    var list: WorkspaceGitChanges?
    var failure: String?
    var loading = false

    static func read(_ load: () async throws -> WorkspaceGitChanges) async -> Result<WorkspaceGitChanges, Error> {
        do { return .success(try await load()) } catch { return .failure(error) }
    }

    /// Keeps the last list through a failed read; a cancelled read changes nothing.
    mutating func apply(_ result: Result<WorkspaceGitChanges, Error>) {
        switch result {
        case .success(let list): self.list = list; failure = nil
        case .failure(is CancellationError): break
        case .failure(let error):
            failure = CodeChangesSheet.message(error, fallback: "This chat's changes could not be loaded from your computer.")
        }
    }
}

/// The files this thread itself changed, over all its turns (or why there are
/// none), each opening the thread's own hunks for it.
private struct ThreadChangeList: View {
    let state: ThreadListState
    let location: ThreadRepositories.Location?
    let retry: () -> Void
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        if let failure = state.failure {
            Section {
                Label(failure, systemImage: "exclamationmark.triangle")
                    .font(.subheadline).foregroundStyle(.secondary)
                    .accessibilityIdentifier("changes-error")
                Button("Try again", action: retry)
                    .disabled(state.loading)
                    .accessibilityIdentifier("changes-retry")
            }
        }
        if let changes = state.list {
            if !changes.available {
                Section {
                    Text(changes.detail ?? "This chat's changes are unavailable.")
                        .font(.subheadline).foregroundStyle(.secondary)
                        .accessibilityIdentifier("changes-unavailable")
                }
            } else if changes.files.isEmpty {
                Section {
                    Text("This chat hasn't changed any files.")
                        .font(.subheadline).foregroundStyle(.secondary)
                        .accessibilityIdentifier("changes-empty")
                } footer: { note(changes) }
            } else {
                Section {
                    ForEach(changes.files) { file in
                        NavigationLink(value: CodeChangesSheet.ThreadDiffTarget(file: file, location: location)) {
                            ChangedFileRow(file: file)
                        }
                        .accessibilityIdentifier("thread-file:" + file.path)
                    }
                } header: {
                    Text("Changed in this chat · " + CodeChangesSheet.totalsText(files: changes.totals.files,
                         additions: changes.totals.additions, deletions: changes.totals.deletions))
                        .lineLimit(typeSize.isAccessibilitySize ? nil : 2)
                        .accessibilityIdentifier("changes-header")
                } footer: { note(changes) }
            }
        } else if state.failure == nil {
            Section {
                ProgressView().frame(maxWidth: .infinity).listRowBackground(Color.clear)
                    .accessibilityIdentifier("changes-loading")
            }
        }
    }

    /// The Mac's word that the list may be incomplete, and a cut-off list.
    @ViewBuilder private func note(_ changes: WorkspaceGitChanges) -> some View {
        let notes = [changes.detail,
                     changes.truncated ? "Showing the first \(changes.files.count.formatted()) files. Review the rest on your computer." : nil]
            .compactMap { $0 }
        if !notes.isEmpty {
            Text(notes.joined(separator: " ")).accessibilityIdentifier("thread-changes-note")
        }
    }
}

/// One file's hunks from This thread, as the Mac sent them with the list.
private struct ThreadFileDiff: View {
    let file: WorkspaceGitChanges.File
    var body: some View {
        FileDiffView(name: file.name, path: file.path, additions: file.additions, deletions: file.deletions, binary: file.binary) {
            guard let diff = file.diff else {
                throw FileDiffView.Unavailable(message: "Too large to show here — open it on your computer.")
            }
            return (diff.isEmpty ? [] : [diff], nil, false)
        }
    }
}

/// One pull request on one line: title, number, then its state and checks as
/// small glyphs; the whole row opens its native detail view.
private struct PullRequestRow: View {
    let pr: ThreadPullRequests.PullRequest
    @Environment(\.colorScheme) private var scheme

    private var state: (symbol: String, color: Color) {
        switch pr.state {
        case .open: ("arrow.triangle.pull", DiffColors.added(scheme))
        case .draft: ("circle.dashed", .secondary)
        case .merged: ("arrow.triangle.merge", .purple)
        case .closed: ("xmark.circle", DiffColors.removed(scheme))
        case .unknown: ("questionmark.circle", .secondary)
        }
    }

    private var branchSummary: String? {
        guard let head = pr.headRef, let base = pr.baseRef else { return nil }
        return "\(head) → \(base)"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            // The title owns the row width. Status glyphs must never turn a
            // large-text title into a column one character wide.
            Text(pr.title)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Image(systemName: state.symbol).foregroundStyle(state.color)
                Text("#\(pr.number) · \(pr.state.title)")
                PullRequestChecksGlyph(state: pr.checks.state, neutral: pr.checks.isNeutralOnly)
            }.font(.caption)
            Text(([pr.repository] + [branchSummary].compactMap { $0 }).joined(separator: " · "))
                .font(.caption).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let summary = pr.checks.summary {
                Text(summary).font(.caption).foregroundStyle(.secondary)
            }
            if pr.details?.reviewDecision == "CHANGES_REQUESTED" {
                Text("Changes requested").font(.caption).foregroundStyle(.secondary)
            }
            if pr.details?.mergeable == "CONFLICTING" || pr.details?.mergeStateStatus == "DIRTY" {
                Text("Conflicts").font(.caption).foregroundStyle(.secondary)
            }
        }
        .contentShape(Rectangle())
        .accessibilityElement(children: .ignore)
        .accessibilityLabel((["Pull request \(pr.number)", pr.title, pr.state.title, pr.repository] +
                             [branchSummary, pr.checks.summary,
                              pr.details?.reviewDecision == "CHANGES_REQUESTED" ? "Changes requested" : nil,
                              pr.details?.mergeable == "CONFLICTING" || pr.details?.mergeStateStatus == "DIRTY" ? "Conflicts" : nil]
                             .compactMap { $0 }).joined(separator: ", "))
    }
}

struct PullRequestChecksGlyph: View {
    let state: ThreadPullRequests.PullRequest.Checks.State
    /// Every check finished neutral or skipped (`Checks.isNeutralOnly`): no
    /// green check mark when nothing passed outright.
    var neutral = false
    @Environment(\.colorScheme) private var scheme
    var body: some View {
        switch state {
        case .failing: Image(systemName: "xmark.circle.fill").foregroundStyle(DiffColors.removed(scheme))
        case .pending: Image(systemName: "clock.fill").foregroundStyle(.orange)
        case .passing where neutral: Image(systemName: "minus.circle").foregroundStyle(.secondary)
        case .passing: Image(systemName: "checkmark.circle.fill").foregroundStyle(DiffColors.added(scheme))
        case .none: EmptyView()
        case .unknown: Image(systemName: "questionmark.circle").foregroundStyle(.secondary)
        }
    }
}

private struct ChangedFileRow: View {
    let file: WorkspaceGitChanges.File
    @Environment(\.colorScheme) private var scheme
    @Environment(\.dynamicTypeSize) private var typeSize

    private var tint: Color {
        switch file.status {
        case "added", "untracked": DiffColors.added(scheme)
        case "deleted": DiffColors.removed(scheme)
        default: .secondary
        }
    }

    var body: some View {
        let large = typeSize.isAccessibilitySize
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: file.symbol).foregroundStyle(tint)
            VStack(alignment: .leading, spacing: 2) {
                Text(file.name).font(.body).fixedSize(horizontal: false, vertical: true)
                Text([file.folder, file.statusTitle, file.originalPath.map { "from \($0)" }, file.binary ? "Binary" : nil]
                    .compactMap { $0 }.joined(separator: " · "))
                    .font(.caption).foregroundStyle(.secondary).lineLimit(large ? 4 : 2).truncationMode(.middle)
                DiffCounts(additions: file.additions, deletions: file.deletions)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(file.accessibilityText)
        .accessibilityHint("Shows this file's diff")
    }
}

/// A file the last reply edited, from its saved patches.
struct EditedFileRow: View {
    let name: String
    let folder: String?
    let additions: Int?
    let deletions: Int?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: "doc.text").foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(name).font(.body).fixedSize(horizontal: false, vertical: true)
                if let folder {
                    Text(folder).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
                DiffCounts(additions: additions, deletions: deletions)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(([folder.map { $0 + "/" + name } ?? name]
            + [additions.map { "\($0) added lines" }, deletions.map { "\($0) removed lines" }].compactMap { $0 }).joined(separator: ", "))
        .accessibilityHint("Shows this file's diff")
    }
}

private struct DiffCounts: View {
    let additions: Int?
    let deletions: Int?
    @Environment(\.colorScheme) private var scheme
    var body: some View {
        HStack(spacing: 4) {
            if let additions, additions > 0 || deletions == 0 {
                Text("+\(additions)").foregroundStyle(DiffColors.added(scheme))
            }
            if let deletions, deletions > 0 {
                Text("−\(deletions)").foregroundStyle(DiffColors.removed(scheme))
            }
        }
        .font(.caption.monospacedDigit())
    }
}

/// One file's unified (or, on a wide screen, side-by-side) diff in the shared
/// renderer, from Git on the Mac or a reply's saved patches.
private struct FileDiffView: View {
    struct Unavailable: Error { let message: String }
    let name: String
    let path: String
    let additions: Int?
    let deletions: Int?
    let binary: Bool
    var commentContext: DiffCommentContext? = nil
    var comparison: WorkspaceChangeScope? = nil
    var originalPath: String? = nil
    var stageComment: (ArtifactAnnotation, String) throws -> Void = { _, _ in throw FileFailure.unsupported }
    /// Patches, a display note, and whether the host supports pinned diff notes.
    let load: () async throws -> ([String], String?, Bool)
    @State private var commentSource: DiffAnnotationSource?
    @State private var loadedCommentContext: DiffCommentContext?
    @State private var showingComment = false
    @State private var commentAdded = false
    @State private var prepared: PreparedDiff?
    @State private var note: String?
    @State private var message: String?
    @State private var failed = false
    @State private var attempt = 0
    @AppStorage("wonder.diff.layout") private var layout = DiffLayout.automatic
    @Environment(\.horizontalSizeClass) private var sizeClass

    var body: some View {
        Group {
            if let message {
                ContentUnavailableView {
                    Label(name, systemImage: binary ? "doc.richtext" : "doc.text.magnifyingglass")
                } description: {
                    Text(message)
                } actions: {
                    if failed { Button("Try again") { attempt += 1 }.accessibilityIdentifier("changes-diff-retry") }
                }
                .accessibilityIdentifier("changes-diff-message")
            } else if let prepared {
                DiffDocument(sections: [DiffDocument.Section(id: path, title: path,
                    additions: additions, deletions: deletions, expanded: true, diff: prepared,
                    note: note, identifier: "changes-diff-file")], split: layout.split(sizeClass), showsHeaders: false) { _ in }
            } else {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
                    .accessibilityIdentifier("changes-diff-loading")
            }
        }
        .navigationTitle(name)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if let commentSource, let loadedCommentContext, let comparison {
                ToolbarItem(placement: .bottomBar) {
                    Button(commentAdded ? "Add another comment" : "Comment on lines", systemImage: "text.bubble") { showingComment = true }
                        .accessibilityIdentifier("diff-comment-open")
                        .sheet(isPresented: $showingComment) {
                            DiffCommentEditor(source: commentSource, context: loadedCommentContext,
                                path: path, comparison: comparison, originalPath: originalPath) { annotation in
                                try stageComment(annotation, loadedCommentContext.assignmentScope)
                                commentAdded = true
                            }
                        }
                }
            }
            if prepared != nil {
                ToolbarItem(placement: .primaryAction) { DiffLayoutPicker(layout: $layout, split: layout.split(sizeClass)) }
            }
        }
        .safeAreaInset(edge: .bottom) {
            if commentAdded { Text("Comment added to your next message").font(.footnote).foregroundStyle(.secondary)
                .padding(8).accessibilityIdentifier("diff-comment-added") }
        }
        .task(id: attempt) {
            message = nil; failed = false; commentSource = nil
            let context = commentContext
            if binary { message = "Binary file — not shown here."; return }
            do {
                let (patches, note, canComment) = try await load()
                guard !Task.isCancelled else { return }
                if patches.isEmpty { message = "No textual changes in this file."; return }
                let path = path
                self.note = note
                let result = await Task.detached(priority: .userInitiated) {
                    (PreparedDiff(patches: patches, path: path),
                     canComment && patches.count == 1 && note == nil ? try? DiffAnnotationSource(patch: patches[0]) : nil)
                }.value
                guard !Task.isCancelled else { return }
                prepared = result.0; commentSource = result.1; loadedCommentContext = context
            } catch is CancellationError {
            } catch let unavailable as Unavailable {
                message = unavailable.message
            } catch {
                guard !Task.isCancelled else { return }
                if case PairingFailure.response(413) = error {
                    message = "Too large to show here — open it on your computer."
                } else {
                    failed = true
                    message = CodeChangesSheet.message(error, fallback: "This diff could not be loaded.")
                }
            }
        }
    }
}

private extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}
