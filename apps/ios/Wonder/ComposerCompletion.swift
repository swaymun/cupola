import SwiftUI
import WonderPairing

/// The text editor a completion writes into. It refuses while IME composition,
/// dictation or an unsaved commit is in progress, or when the draft no longer
/// holds the trigger that was chosen from.
@MainActor protocol ComposerCompletionEditor: AnyObject {
    func replaceCompletion(_ trigger: ComposerTrigger, with insertion: String) -> Bool
}

/// Inline commands and Project file/thread references. The host supplies
/// provider commands, Cupola draft actions and scoped reference insertions. `@Observable` so only the list re-renders while
/// typing, not the conversation that owns it.
@MainActor @Observable final class ComposerCompletionModel {
    struct Context: Equatable, Sendable {
        /// The conversation, or a new chat's `project-files:` scope.
        let conversationID: String
        /// Invalidates results when a thread switches provider; also selects
        /// the provider of a new chat that has no conversation yet.
        let family: AgentFamily?
        let commands: Bool
        let files: Bool
        var threads: Bool = true
        var inT3Code: Bool = false
    }
    enum Row: Identifiable, Equatable {
        case command(Int, ComposerCommandCatalog.Command)
        case file(WorkspaceSearchResults.Entry)
        case thread(ComposerThreadResults.Entry)
        var id: String {
            switch self {
            case .command(let index, let command): "command:\(index):\(command.name)"
            case .file(let entry): "file:" + entry.id
            case .thread(let entry): "thread:" + entry.id
            }
        }
        var insertion: String {
            switch self {
            case .command(_, let command): command.insertion
            case .file(let entry): entry.mention
            case .thread(let entry): entry.mention
            }
        }
    }
    private(set) var trigger: ComposerTrigger?
    private(set) var rows: [Row] = []
    private(set) var loading = false
    /// Plain words for an empty, unavailable or failed list.
    private(set) var message: String?
    private(set) var highlighted = 0
    /// A hardware keyboard moved the highlight; show it.
    private(set) var showsHighlight = false

    @ObservationIgnored private(set) var context: Context?
    @ObservationIgnored private var loadCommands: (@MainActor @Sendable (String, AgentFamily?, Bool) async throws -> ComposerCommandCatalog)?
    @ObservationIgnored private var searchFiles: (@MainActor @Sendable (String, String, AgentFamily?) async throws -> WorkspaceSearchResults)?
    @ObservationIgnored private var searchThreads: (@MainActor @Sendable (String, String) async throws -> ComposerThreadResults)?
    @ObservationIgnored weak var editor: ComposerCompletionEditor?
    @ObservationIgnored private var catalog: (conversationID: String, value: ComposerCommandCatalog, at: Date)?
    @ObservationIgnored private var commandTask: Task<Void, Never>?
    @ObservationIgnored private var searchTask: Task<Void, Never>?
    @ObservationIgnored private var pending: (text: String, selection: NSRange)?
    @ObservationIgnored private var applyScheduled = false
    /// The trigger the person dismissed with Escape stays closed until it changes.
    @ObservationIgnored private var dismissed: NSRange?

    /// The session can change its list at any time, so it is not kept long.
    private static let catalogLifetime: TimeInterval = 30
    private static let searchDelay: Duration = .milliseconds(150)

    func configure(_ context: Context?,
                   loadCommands: @escaping @MainActor @Sendable (String, AgentFamily?, Bool) async throws -> ComposerCommandCatalog,
                   searchFiles: @escaping @MainActor @Sendable (String, String, AgentFamily?) async throws -> WorkspaceSearchResults,
                   searchThreads: @escaping @MainActor @Sendable (String, String) async throws -> ComposerThreadResults) {
        self.loadCommands = loadCommands
        self.searchFiles = searchFiles
        self.searchThreads = searchThreads
        guard context != self.context else { return }
        self.context = context
        catalog = nil
        reset()
    }

    /// Ends every request; the owner calls this when the composer goes away.
    func reset() {
        commandTask?.cancel(); commandTask = nil
        searchTask?.cancel(); searchTask = nil
        if catalog?.conversationID != context?.conversationID { catalog = nil }
        pending = nil
        dismissed = nil
        close()
    }

    private func close() {
        if trigger != nil { trigger = nil }
        if !rows.isEmpty { rows = [] }
        if loading { loading = false }
        if message != nil { message = nil }
        highlighted = 0; showsHighlight = false
    }

    /// The editor's committed text and caret. Called from UIKit callbacks,
    /// which may run inside a SwiftUI update, so the change applies on the
    /// next main-queue turn, coalesced to the latest input.
    func editorChanged(text: String, selection: NSRange) {
        pending = (text, selection)
        guard !applyScheduled else { return }
        applyScheduled = true
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.applyScheduled = false
            guard let input = self.pending else { return }
            self.pending = nil
            self.apply(text: input.text, selection: input.selection)
        }
    }

    func editorEnded() {
        pending = nil
        searchTask?.cancel(); searchTask = nil
        close()
    }

    private func apply(text: String, selection: NSRange) {
        guard let context else { return close() }
        guard let next = ComposerTrigger.detect(in: text, selection: selection,
                                                commands: context.commands, files: context.files || context.threads) else {
            dismissed = nil
            searchTask?.cancel(); searchTask = nil
            return close()
        }
        if let dismissed, dismissed.location == next.range.location { return close() }
        dismissed = nil
        guard next != trigger else { return }
        let changedKind = trigger?.isCommand != next.isCommand || trigger?.range.location != next.range.location
        trigger = next
        if changedKind { highlighted = 0; showsHighlight = false; rows = [] }
        if next.isCommand { showCommands(context: context) } else { search(next.query, context: context) }
    }

    private func showCommands(context: Context) {
        searchTask?.cancel(); searchTask = nil
        // A ready list is reused briefly; any other answer is asked again only
        // after a few seconds, not on every keystroke.
        if let catalog, catalog.conversationID == context.conversationID,
           Date().timeIntervalSince(catalog.at) < ([.ready, .cached].contains(catalog.value.availability) ? Self.catalogLifetime : 5) {
            return present(catalog.value)
        }
        loading = true; message = nil
        guard commandTask == nil, let loadCommands else { return }
        let conversationID = context.conversationID
        commandTask = Task { [weak self] in
            let result: Result<ComposerCommandCatalog, Error>
            do { result = .success(try await loadCommands(conversationID, context.family, context.inT3Code)) } catch { result = .failure(error) }
            guard let self, !Task.isCancelled, self.context == context else { return }
            self.commandTask = nil
            switch result {
            case .success(let value):
                self.catalog = (conversationID, value, Date())
                if self.trigger?.isCommand == true { self.present(value) }
            case .failure:
                self.catalog = nil
                guard self.trigger?.isCommand == true else { return }
                self.loading = false; self.rows = []
                self.message = "Commands can't be loaded right now. Your computer may be offline."
            }
        }
    }

    private func present(_ catalog: ComposerCommandCatalog) {
        loading = false
        guard let trigger, trigger.isCommand else { return }
        rows = catalog.matching(trigger.query).enumerated().map { .command($0.offset, $0.element) }
        switch catalog.availability {
        case .ready, .cached:
            message = rows.isEmpty ? "No matching commands." : nil
        case .unknown:
            message = "Provider commands are still loading."
        case .unavailable:
            message = "More provider commands appear after this chat starts."
        }
        highlighted = min(highlighted, max(0, rows.count - 1))
    }

    private func search(_ query: String, context: Context) {
        commandTask?.cancel(); commandTask = nil
        searchTask?.cancel()
        loading = true
        if rows.isEmpty { message = nil }
        // Clear the previous query immediately: a slow reply must never leave
        // a tappable file from a different search under the new text.
        rows = []
        guard let searchFiles, let searchThreads else { return }
        searchTask = Task { [weak self] in
            try? await Task.sleep(for: Self.searchDelay)
            guard !Task.isCancelled else { return }
            async let files = context.files ? try? searchFiles(context.conversationID, query, context.family) : nil
            async let threads = context.threads ? try? searchThreads(context.conversationID, query) : nil
            let (fileResults, threadResults) = await (files, threads)
            guard let self, !Task.isCancelled, self.context == context,
                  case .file(let current, _)? = self.trigger, current == query else { return }
            self.loading = false
            // Leave room for both kinds even in a large Project. Each source
            // is ranked by the host. A failed source doesn't hide the other.
            let fileRows = Array((fileResults?.entries ?? []).prefix(10)).map(Row.file)
            let threadRows = Array((threadResults?.entries ?? []).prefix(10)).map(Row.thread)
            self.rows = (0..<max(fileRows.count, threadRows.count)).flatMap { index in
                (fileRows.indices.contains(index) ? [fileRows[index]] : [])
                    + (threadRows.indices.contains(index) ? [threadRows[index]] : [])
            }
            let missingFiles = context.files && fileResults == nil
            let missingThreads = context.threads && threadResults == nil
            if missingFiles || missingThreads {
                self.message = missingFiles && missingThreads ? "References can't be loaded. Check your computer."
                    : missingFiles ? "Files are unavailable." : "Threads are unavailable."
            } else {
                self.message = self.rows.isEmpty ? "No matching files or threads." : nil
            }
            self.highlighted = min(self.highlighted, max(0, self.rows.count - 1))
        }
    }

    @discardableResult func choose(_ row: Row) -> Bool {
        guard let trigger, let editor, editor.replaceCompletion(trigger, with: row.insertion) else { return false }
        searchTask?.cancel(); searchTask = nil
        close()
        return true
    }

    var isCachedCatalog: Bool { catalog?.value.availability == .cached }

    // Hardware keyboard: arrows move, Return or Tab chooses, Escape closes.
    var acceptsKeys: Bool { trigger != nil }
    func moveHighlight(_ delta: Int) {
        guard !rows.isEmpty else { return }
        highlighted = (highlighted + delta + rows.count) % rows.count
        showsHighlight = true
    }
    func chooseHighlighted() -> Bool {
        guard rows.indices.contains(highlighted) else { return false }
        return choose(rows[highlighted])
    }
    func dismiss() {
        dismissed = trigger?.range
        searchTask?.cancel(); searchTask = nil
        close()
    }
}

/// One compact list directly above the message field: the provider's commands
/// after `/`, Project files after `@`.
struct ComposerSuggestionList: View {
    let completion: ComposerCompletionModel
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        if let trigger = completion.trigger {
            let secondary = theme.isDefault ? Color.secondary : theme.secondaryText
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 6) {
                    Text(trigger.isCommand ? "Commands" : "Files and threads")
                        .font(.caption.weight(.semibold)).foregroundStyle(secondary)
                        .accessibilityAddTraits(.isHeader)
                    if completion.loading && !completion.rows.isEmpty { ProgressView().controlSize(.mini) }
                    Spacer(minLength: 0)
                    if trigger.isCommand, completion.isCachedCatalog {
                        Text("Last seen").font(.caption).foregroundStyle(secondary)
                    }
                }
                .padding(.horizontal, 12).padding(.top, 8).padding(.bottom, 4)
                if completion.rows.isEmpty {
                    Group {
                        if completion.loading {
                            Label(trigger.isCommand ? "Loading commands…" : "Searching…", systemImage: "hourglass")
                        } else if let message = completion.message {
                            Text(message)
                        }
                    }
                    .font(.footnote).foregroundStyle(secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12).padding(.bottom, 8)
                    .accessibilityIdentifier("composer-suggestions-status")
                } else {
                    ScrollView {
                        VStack(spacing: 0) {
                            ForEach(Array(completion.rows.enumerated()), id: \.element.id) { index, row in
                                Button { completion.choose(row) } label: {
                                    ComposerSuggestionRow(row: row, secondary: secondary)
                                }
                                .buttonStyle(.plain)
                                .background(completion.showsHighlight && index == completion.highlighted
                                            ? theme.chip : Color.clear)
                                .accessibilityIdentifier(Self.identifier(row))
                            }
                        }
                    }
                    .scrollBounceBehavior(.basedOnSize)
                    .frame(maxHeight: typeSize.isAccessibilitySize ? 180 : 220)
                    .fixedSize(horizontal: false, vertical: true)
                    if let message = completion.message {
                        Text(message).font(.caption).foregroundStyle(secondary)
                            .padding(.horizontal, 12).padding(.bottom, 6)
                    }
                }
            }
            .accessibilityElement(children: .contain)
            .accessibilityLabel(trigger.isCommand ? "Commands" : "Files and threads")
            .accessibilityIdentifier("composer-suggestions")
        }
    }

    static func identifier(_ row: ComposerCompletionModel.Row) -> String {
        switch row {
        case .command(_, let command): "composer-suggestion:command:" + command.name
        case .file(let entry): "composer-suggestion:file:" + entry.rootId + ":" + entry.path
        case .thread(let entry): "composer-suggestion:thread:" + entry.id
        }
    }
}

private struct ComposerSuggestionRow: View {
    let row: ComposerCompletionModel.Row
    let secondary: Color
    @Environment(\.wonderTheme) private var theme

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            switch row {
            case .command(_, let command):
                VStack(alignment: .leading, spacing: 1) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text("/" + command.name).font(.body.weight(.medium)).foregroundStyle(theme.text)
                        if let hint = command.argumentHint {
                            Text(hint).font(.footnote).foregroundStyle(secondary).lineLimit(1)
                        }
                    }
                    if !command.description.isEmpty {
                        Text(command.description).font(.footnote).foregroundStyle(secondary).lineLimit(1)
                    }
                }
            case .thread(let entry):
                Image(systemName: "bubble.left.and.bubble.right")
                    .font(.footnote).foregroundStyle(secondary).frame(width: 18)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 1) {
                    Text(entry.title).font(.body).foregroundStyle(theme.text).lineLimit(1)
                    Text(entry.family.title + " · Thread").font(.footnote).foregroundStyle(secondary)
                }
            case .file(let entry):
                Image(systemName: entry.isDirectory ? "folder" : "doc")
                    .font(.footnote).foregroundStyle(secondary).frame(width: 18)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 1) {
                    Text(entry.path).font(.body).foregroundStyle(theme.text).lineLimit(1).truncationMode(.middle)
                    let location = entry.rootId == "workspace" ? "" : entry.rootLabel
                    if !location.isEmpty {
                        Text(location).font(.footnote).foregroundStyle(secondary).lineLimit(1).truncationMode(.head)
                    }
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12).padding(.vertical, 6)
        .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
        .contentShape(Rectangle())
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(label)
        .accessibilityHint("Inserts it into your message.")
        .accessibilityAddTraits(.isButton)
    }

    private var label: String {
        switch row {
        case .command(_, let command):
            return ["Command " + command.name, command.argumentHint, command.description.isEmpty ? nil : command.description]
                .compactMap { $0 }.joined(separator: ", ")
        case .thread(let entry):
            return "Thread " + entry.title + ", " + entry.family.title
        case .file(let entry):
            let kind = entry.isDirectory ? "Folder" : "File"
            let place = entry.folder.isEmpty ? (entry.rootId == "workspace" ? nil : entry.rootLabel) : "in " + entry.folder
            return [kind + " " + entry.name, place].compactMap { $0 }.joined(separator: ", ")
        }
    }
}

extension ConnectionModel {
    func loadComposerCommands(_ conversationID: String, family: AgentFamily?, inT3Code: Bool) async throws -> ComposerCommandCatalog {
        guard var components = URLComponents(string: "/api/v1/conversations/\(Self.escape(conversationID))/commands") else {
            throw PairingFailure.invalidLink
        }
        components.queryItems = (family.map { [URLQueryItem(name: "family", value: $0.rawValue)] } ?? [])
            + (inT3Code ? [URLQueryItem(name: "inT3Code", value: "true")] : [])
        guard let path = components.string else { throw PairingFailure.invalidLink }
        return try await manage(path)
    }

    func searchComposerThreads(_ conversationID: String, query: String) async throws -> ComposerThreadResults {
        let path = "/api/v1/conversations/\(Self.escape(conversationID))/mentions/threads"
        var components = URLComponents(string: path)!
        components.queryItems = [URLQueryItem(name: "q", value: query), URLQueryItem(name: "limit", value: "10")]
        return try await manage(components.string!)
    }

    /// `family` names a new chat's provider; a conversation uses its own.
    func searchComposerFiles(_ conversationID: String, query: String, family: AgentFamily?) async throws -> WorkspaceSearchResults {
        guard var components = URLComponents(string: Self.workspaceEndpoint(conversationID: conversationID, operation: "search") ?? "") else {
            throw PairingFailure.invalidLink
        }
        components.queryItems = [URLQueryItem(name: "q", value: query), URLQueryItem(name: "limit", value: "20")]
            + (family.map { [URLQueryItem(name: "family", value: $0.rawValue)] } ?? [])
        guard let path = components.string else { throw PairingFailure.invalidLink }
        return try await manage(path)
    }

    /// What the composer of `chat` completes, from the Mac's capability record.
    func composerCompletionContext(_ chat: ChatSummary) -> ComposerCompletionModel.Context? {
        guard isProject(chat), let capabilities = capabilities(chat),
              capabilities.slashCommands || capabilities.fileMentions || capabilities.threadTools else { return nil }
        return .init(conversationID: chat.id, family: agentFamily(chat),
                     commands: true, files: capabilities.fileMentions, threads: capabilities.threadTools,
                     inT3Code: nativeHolders[chat.id]?.t3Code != nil)
    }
}
