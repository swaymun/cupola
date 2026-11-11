import SwiftUI
import AVKit
import ImageIO
import PhotosUI
import WonderPairing
import UniformTypeIdentifiers
import PDFKit
import WebKit
import VisionKit
import Photos

struct ContextCompactionMarker: View {
    let row: ReadRow
    let isProjectConversation: Bool

    private var presentation: ContextCompactionPresentation {
        // A provider switch is a different kind of boundary with its own text.
        if let text = row.providerSwitchText {
            return ContextCompactionPresentation(label: text, symbol: "arrow.left.arrow.right", isRunning: false)
        }
        return ContextCompactionPresentation.forState(row.item?.state ?? "unknown",
                                               isProjectConversation: isProjectConversation)
    }

    /// When the compaction happened, once it is over: the provider (or T3
    /// Code, before its prompt cache expires) compacts without being asked,
    /// so the owner cannot otherwise tell when the context was shortened.
    private var label: String {
        // Before 2000 is a placeholder, not a time anything happened.
        guard !row.isProviderSwitch, !presentation.isRunning, row.time > 946_684_800,
              row.item?.state.lowercased() != "unknown" else { return presentation.label }
        let date = Date(timeIntervalSince1970: row.time)
        let when = Calendar.current.isDateInToday(date)
            ? date.formatted(date: .omitted, time: .shortened)
            : date.formatted(date: .abbreviated, time: .shortened)
        return presentation.label + " · " + when
    }

    var body: some View {
        HStack(spacing: 10) {
            Rectangle()
                .fill(.quaternary)
                .frame(height: 1)
            HStack(spacing: 6) {
                Image(systemName: presentation.symbol)
                    .font(.caption.weight(.semibold))
                Text(label)
                    .font(.caption)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .layoutPriority(1)
            .foregroundStyle(.secondary)
            Rectangle()
                .fill(.quaternary)
                .frame(height: 1)
        }
        .frame(minHeight: 36)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(label)
        .accessibilityIdentifier((row.isProviderSwitch ? "provider-switch:" : "context-compaction:") + row.id)
    }
}

/// A background task's report that woke the agent: one quiet line like a tool
/// row (`Command "Build" finished`). The run's tools fold into their own
/// "Worked" row after it, then the reply.
struct TaskNotificationRow: View {
    let row: ReadRow

    var body: some View {
        let label = row.taskNotificationText ?? "Background task finished"
        HStack(spacing: 6) {
            Image(systemName: row.taskNotificationSymbol)
                .font(.caption.weight(.semibold))
                .frame(width: 18)
            Text(label)
                .font(.subheadline)
                .lineLimit(1)
                .truncationMode(.middle)
        }
        .foregroundStyle(.secondary)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 4)
        // One line, however long the command; holding the row copies all of it.
        .contentShape(Rectangle())
        .contextMenu {
            Button("Copy", systemImage: "doc.on.doc") { UIPasteboard.general.string = label }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(label)
        .accessibilityAction(named: Text("Copy")) { UIPasteboard.general.string = label }
        .accessibilityIdentifier("task-notification:" + row.id)
    }
}

private struct CommandDisclosureLabel: View {
    let summary: CommandSummary
    let symbol: String
    let durationIdentifier: String
    @ScaledMetric(relativeTo: .subheadline) private var minimumCommandWidth: CGFloat = 76
    @ScaledMetric(relativeTo: .subheadline) private var iconWidth: CGFloat = 18

    var body: some View {
        ViewThatFits(in: .horizontal) {
            // Duration is secondary. Only show it when the command itself can
            // remain completely visible; otherwise give the row to the
            // tail-truncated command.
            line(includeDuration: true, minimumWidth: minimumCommandWidth, requireFullCommand: true)
            line(includeDuration: false, minimumWidth: minimumCommandWidth)
            // At accessibility sizes in a narrow column, the command is
            // more useful than a decorative terminal icon.
            line(includeDuration: false, minimumWidth: 0, includeIcon: false)
        }
        .font(.subheadline.weight(.medium))
        .frame(minHeight: 24, alignment: .leading)
    }

    @ViewBuilder private func line(includeDuration: Bool, minimumWidth: CGFloat, includeIcon: Bool = true,
                                   requireFullCommand: Bool = false) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            if includeIcon { Image(systemName: symbol).frame(width: iconWidth) }
            Text(summary.prefix).fixedSize(horizontal: true, vertical: false)
            if requireFullCommand {
                Text(summary.displayCommand)
                    .font(.system(.subheadline, design: .monospaced).weight(.medium))
                    .lineLimit(1)
                    .fixedSize(horizontal: true, vertical: false)
            } else {
                Text(summary.displayCommand)
                    .font(.system(.subheadline, design: .monospaced).weight(.medium))
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .frame(minWidth: minimumWidth, idealWidth: minimumWidth, maxWidth: .infinity, alignment: .leading)
            }
            if includeDuration, let duration = summary.duration {
                Text("for " + duration)
                    .fixedSize(horizontal: true, vertical: false)
                    .accessibilityIdentifier(durationIdentifier)
            }
        }
        .lineLimit(1)
        .fixedSize(horizontal: false, vertical: true)
    }
}

private struct FileChangeDisclosureLabel: View {
    @Environment(\.wonderTheme) private var theme
    let summary: FileChangeSummary
    let failed: Bool
    let openFile: ((String) -> Void)?
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    var body: some View {
        Group {
            if dynamicTypeSize.isAccessibilitySize {
                VStack(alignment: .leading, spacing: 4) {
                    title.lineLimit(3).truncationMode(.middle)
                    counts
                }
            } else {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    title.lineLimit(1).truncationMode(.middle)
                        .frame(minWidth: 0, maxWidth: .infinity, alignment: .leading)
                    counts.layoutPriority(1)
                }
            }
        }
        .font(.subheadline.weight(.medium))
        .foregroundStyle(failed ? Color.red : Color.primary)
        .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
        .accessibilityElement(children: .contain)
    }

    @ViewBuilder private var title: some View {
        if let path = summary.path, let openFile {
            if dynamicTypeSize.isAccessibilitySize {
                VStack(alignment: .leading, spacing: 4) {
                    Text(summary.action)
                    fileLink(path, open: openFile)
                }
            } else {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(summary.action).fixedSize(horizontal: true, vertical: false)
                    fileLink(path, open: openFile)
                }
            }
        } else { Text(summary.title) }
    }

    private func fileLink(_ path: String, open: @escaping (String) -> Void) -> some View {
        Button { open(path) } label: {
            Text(summary.target).underline().multilineTextAlignment(.leading)
                .frame(minHeight: 44, alignment: .leading).contentShape(Rectangle())
        }
        .buttonStyle(.plain).foregroundStyle(theme.accent)
        .accessibilityIdentifier("file-change-open:" + path)
        .accessibilityHint("Open this file in Files")
        .accessibilityAddTraits(.isLink)
    }

    private var counts: some View {
        HStack(spacing: 6) {
            if let added = summary.additions {
                Text("+\(added)").accessibilityIdentifier("file-change-additions").foregroundStyle(colorScheme == .dark ? Color.green : Color(red: 0.1, green: 0.42, blue: 0.2))
            }
            if let removed = summary.deletions {
                Text("−\(removed)").accessibilityIdentifier("file-change-deletions").foregroundStyle(colorScheme == .dark ? Color.red : Color(red: 0.75, green: 0.12, blue: 0.16))
            }
        }
        .monospacedDigit()
        .fixedSize(horizontal: true, vertical: false)
    }
}

private struct SubagentActivityLabel: View {
    let title: String
    let statusLabel: String
    /// The agent glyph, or a Claude task's status glyph.
    var symbol = SubagentActivityLabel.agentSymbol
    /// False when the glyph already shows the status (a Claude task).
    var showsStatus = true
    static let agentSymbol = "sparkles.rectangle.stack"
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            if dynamicTypeSize.isAccessibilitySize {
                VStack(alignment: .leading, spacing: 4) {
                    Text(title).font(.subheadline.weight(.medium))
                        .fixedSize(horizontal: false, vertical: true)
                    Text(statusLabel).font(.caption).foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                Label(title, systemImage: symbol)
                    .font(.subheadline.weight(.medium)).lineLimit(1)
                Spacer()
                // A Claude task's glyph already shows its status.
                if showsStatus {
                    Text(statusLabel).font(.caption).foregroundStyle(.secondary)
                }
            }
            Image(systemName: "chevron.right").font(.caption2).foregroundStyle(.secondary)
        }
        .frame(minHeight: 44)
        .contentShape(Rectangle())
    }
}

struct ActivityItemView: View {
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    let row: ReadRow
    let expanded: Bool
    let conversationID: String
    let outputScope: String
    let loadOutput: (@Sendable (ToolOutputIdentity) async throws -> ToolOutput)?
    let projectSubagents: [ProjectSubagentSummary]
    let projectSubagentsAvailable: Bool
    let projectSubagentFreshIDs: Set<String>
    let openProjectSubagent: ((ProjectSubagentSummary) -> Void)?
    let openFile: ((String) -> Void)?
    let toggle: () -> Void
    @State private var cached: ActivityPresentation?
    @State private var cachedRevision: DetailSource?
    @State private var outputFailed = false
    @State private var outputRetry = 0
    init(
        row: ReadRow,
        expanded: Bool,
        conversationID: String = "",
        outputScope: String = "",
        loadOutput: (@Sendable (ToolOutputIdentity) async throws -> ToolOutput)? = nil,
        projectSubagents: [ProjectSubagentSummary] = [],
        projectSubagentsAvailable: Bool = true,
        projectSubagentFreshIDs: Set<String> = [],
        openProjectSubagent: ((ProjectSubagentSummary) -> Void)? = nil,
        openFile: ((String) -> Void)? = nil,
        toggle: @escaping () -> Void
    ) {
        self.row = row
        self.expanded = expanded
        self.conversationID = conversationID
        self.outputScope = outputScope
        self.loadOutput = loadOutput
        self.projectSubagents = projectSubagents
        self.projectSubagentsAvailable = projectSubagentsAvailable
        self.projectSubagentFreshIDs = projectSubagentFreshIDs
        self.openProjectSubagent = openProjectSubagent
        self.openFile = openFile
        self.toggle = toggle
    }
    @ViewBuilder private func detailLabel(_ activity: ActivityPresentation) -> some View {
        if activity.usesCommandLabel, let command = row.commandSummary {
            CommandDisclosureLabel(summary: command, symbol: activity.symbol,
                                   durationIdentifier: "command-duration:" + row.id)
                .foregroundStyle(activity.failed ? Color.red : Color.primary)
                // One line with an ellipsis; holding the row copies the whole command.
                .contextMenu {
                    Button("Copy command", systemImage: "doc.on.doc") { UIPasteboard.general.string = command.displayCommand }
                }
        } else {
            // At accessibility sizes the status goes under the title, so neither
            // breaks inside a word.
            let layout = dynamicTypeSize.isAccessibilitySize
                ? AnyLayout(VStackLayout(alignment: .leading, spacing: 2))
                : AnyLayout(HStackLayout(alignment: .firstTextBaseline))
            layout {
                Label(activity.title, systemImage: activity.symbol).font(.subheadline.weight(.medium))
                if !dynamicTypeSize.isAccessibilitySize { Spacer() }
                Text(activity.status).font(.caption)
                    .foregroundStyle(activity.failed ? Color.red : Color.secondary)
            }.foregroundStyle(.primary)
                // The Spacer between title and status is empty space; without a
                // shape a tap there never reaches the disclosure.
                .contentShape(Rectangle())
        }
    }
    var body: some View {
        let source = detailSource
        // Full output belongs only to this expansion and source revision.
        let activity = (expanded && cachedRevision == source ? cached : nil) ?? row.activitySummary
        let needsOutput = row.item?.outputTruncated == true && cachedRevision != source
        if row.item?.type == "reasoning" {
            ThinkingActivityView(row: row, isRunning: row.activitySummary?.isRunning == true)
        } else if !projectSubagents.isEmpty, let openProjectSubagent {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(projectSubagents) { projectSubagent in
                    let available = projectSubagentsAvailable
                        && projectSubagentFreshIDs.contains(projectSubagent.threadId)
                    Button {
                        openProjectSubagent(projectSubagent)
                    } label: {
                        SubagentActivityLabel(title: projectSubagent.title,
                                              statusLabel: projectSubagent.statusLabel(available: available),
                                              symbol: projectSubagent.usesAvatar ? SubagentActivityLabel.agentSymbol : projectSubagent.statusSymbol,
                                              showsStatus: projectSubagent.usesAvatar)
                    }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier("project-subagent-row:" + projectSubagent.threadId)
                    .accessibilityLabel(projectSubagent.title + ", "
                                        + projectSubagent.statusLabel(available: available))
                    .accessibilityHint("Open read-only agent task.")
                }
            }
        } else if let activity {
        Group {
        if let file = row.fileChangeSummary {
            VStack(alignment: .leading, spacing: 6) {
                HStack(alignment: .center, spacing: 0) {
                    FileChangeDisclosureLabel(summary: file, failed: activity.failed, openFile: openFile)
                    Button(action: toggle) {
                        Image(systemName: expanded ? "chevron.down" : "chevron.right")
                            .font(.subheadline.weight(.semibold)).frame(width: 44, height: 44)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel((expanded ? "Hide changes: " : "Show changes: ") + file.title)
                    .accessibilityIdentifier("file-change-toggle:" + row.id)
                }
                if expanded {
                    ForEach(Array(activity.details.enumerated()), id: \.offset) { _, detail in
                        if ![.files, .additions, .deletions].contains(detail.kind) || !activity.details.contains(where: \.isDiff) {
                            VStack(alignment: .leading, spacing: 4) {
                                if let path = detail.filePath, let openFile {
                                    Button { openFile(path) } label: { Text(detail.title).underline().frame(minHeight: 44, alignment: .leading).contentShape(Rectangle()) }
                                        .font(.caption).buttonStyle(.plain).foregroundStyle(theme.accent)
                                        .accessibilityIdentifier("file-change-detail-open:" + path)
                                        .accessibilityHint("Open this file in Files")
                                        .accessibilityAddTraits(.isLink)
                                } else { Text(detail.title).font(.caption).foregroundStyle(.secondary) }
                                ToolTextPanel(text: detail.text, diff: detail.isDiff)
                            }
                        }
                    }
                }
            }
        } else {
        // An explicit header button, not DisclosureGroup: inside the conversation
        // list a tap on a DisclosureGroup label never reached its binding, so
        // tool rows (MCP calls) could not be opened.
        VStack(alignment: .leading, spacing: 0) {
            Button(action: toggle) {
                HStack(spacing: 8) {
                    detailLabel(activity)
                    Image(systemName: expanded ? "chevron.down" : "chevron.right")
                        .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                }
                .frame(minHeight: 44)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            if expanded {
            VStack(alignment: .leading, spacing: 10) {
                if needsOutput {
                    if outputFailed {
                        HStack {
                            Text("Couldn’t load output.").font(.subheadline).foregroundStyle(.secondary)
                            Button("Retry") { outputRetry += 1 }
                                .frame(minWidth: 44, minHeight: 44)
                                .accessibilityIdentifier("tool-output-retry")
                        }
                        .accessibilityElement(children: .contain)
                        .accessibilityIdentifier("tool-output-unavailable")
                    } else {
                        ProgressView().accessibilityLabel("Loading output")
                            .accessibilityIdentifier("tool-output-loading")
                    }
                } else if activity.kind == .commandExecution {
                    ToolTextPanel(text: activity.commandPanelText ?? "$ ")
                    if activity.failed, let exit = activity.detail(.exitCode) {
                        Text("Exited with code \(exit)").font(.caption).foregroundStyle(.secondary)
                    }
                    if let error = activity.detail(.error) {
                        Text(error).font(.callout).foregroundStyle(.secondary)
                    }
                } else {
                    ForEach(Array(activity.details.enumerated()), id: \.offset) { _, detail in
                        if activity.kind != .fileChange || ![.files, .additions, .deletions].contains(detail.kind) || !activity.details.contains(where: \.isDiff) {
                            VStack(alignment: .leading, spacing: 4) {
                                Text(detail.title).font(.caption).foregroundStyle(.secondary)
                                ToolTextPanel(text: detail.text, diff: detail.isDiff, title: detail.title)
                            }
                        }
                    }
                }
            }.padding(.top, 6)
            }
        }
        }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(activity.accessibilityLabel)
        .accessibilityValue(expanded ? "Expanded" : "Collapsed")
        .accessibilityIdentifier("activity-detail")
        .task(id: DetailRevision(expanded: expanded, source: source, retry: outputRetry)) {
            guard expanded else {
                cached = nil; cachedRevision = nil; outputFailed = false
                return
            }
            guard cachedRevision != source else { return }
            outputFailed = false
            do {
                let presentation = try await ToolOutputDetail.prepare(row: row, expanded: true,
                    conversationId: conversationID, load: loadOutput)
                guard !Task.isCancelled, source == detailSource else { return }
                cachedRevision = source; cached = presentation
            } catch {
                guard !Task.isCancelled, source == detailSource else { return }
                outputFailed = true
            }
        }
        .onDisappear { cached = nil; cachedRevision = nil }
        }
    }
    private var detailSource: DetailSource {
        DetailSource(conversationID: conversationID, scope: outputScope, turnID: row.turnId,
                     itemID: row.item?.id, payload: row.item?.payload, state: row.item?.state, text: row.text)
    }
    private struct DetailRevision: Equatable {
        let expanded: Bool
        let source: DetailSource
        let retry: Int
    }
    private struct DetailSource: Equatable {
        let conversationID: String
        let scope: String
        let turnID: String?
        let itemID: String?
        let payload: [String: ThreadValue]?
        let state: String?
        let text: String?
    }
}

extension ConnectionModel {
    func toolOutput(_ identity: ToolOutputIdentity) async throws -> ToolOutput {
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let scope = pairingFence
        let result = try await api.toolOutput(identity, connection: saved)
        try Task.checkCancellation()
        guard scope == pairingFence, !accessEnded else { throw CancellationError() }
        return result
    }
}


/// Tool content remains selectable, inert text with bounded scrolling.
struct ToolTextPanel: View {
    let text: String
    var diff = false
    /// What the output is, for VoiceOver and the reader; commands and diffs
    /// say so without one.
    var title: String? = nil
    @State private var reading = false
    private var name: String { title ?? (diff ? "Diff output" : "Command output") }
    var body: some View {
        ToolTextView(text:text,diff:diff)
        // Keep the full selectable text out of the accessibility tree. UIKit
        // otherwise serializes thousands of lines for every XCTest/VoiceOver
        // snapshot, even though the visible viewport is bounded below. The
        // reader shows the whole text a page at a time instead.
        .accessibilityHidden(true)
        .frame(height: min(260, CGFloat(max(1, text.prefix(2000).filter { $0 == "\n" }.count + 1)) * 20 + 20))
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
        .accessibilityElement(children: .ignore)
        .accessibilityIdentifier("tool-output")
        .accessibilityLabel(name)
        .accessibilityHint("Read output shows the full text, a page at a time.")
        .accessibilityAction(named: Text("Read output")) { reading = true }
        .accessibilityAction(named: Text(diff ? "Copy full diff" : "Copy full output")) {
            UIPasteboard.general.string = text
        }
        .overlay(alignment: .topTrailing) {
            // Visible too, for Voice Control and anyone who wants a larger view.
            Button { reading = true } label: {
                Image(systemName: "arrow.up.left.and.arrow.down.right")
                    .font(.caption.weight(.semibold))
                    .frame(width: 28, height: 28)
                    .background(.regularMaterial, in: Circle())
                    .frame(width: 44, height: 44)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
            .accessibilityLabel("Read output")
            .accessibilityIdentifier("tool-output-read")
        }
        .sheet(isPresented: $reading) { ToolOutputReader(title: name, text: text, diff: diff) }
    }
}

/// The complete output of a tool, a bounded page at a time: selectable,
/// readable by VoiceOver line by line, and copied whole.
private struct ToolOutputReader: View {
    let title: String
    let text: String
    let diff: Bool
    @Environment(\.dismiss) private var dismiss
    @State private var pages: [String]?
    @State private var page = 0
    @State private var copied = false
    var body: some View {
        NavigationStack {
            Group {
                if let pages, !pages.isEmpty {
                    let shown = min(page, pages.count - 1)
                    VStack(spacing: 0) {
                        ToolTextView(text: pages[shown], diff: diff, document: shown,
                                     accessibilityTitle: pages.count > 1 ? "\(title), page \(shown + 1) of \(pages.count)" : title)
                        if pages.count > 1 { pager(shown, count: pages.count) }
                    }
                } else {
                    ProgressView("Preparing output…")
                }
            }
            .wonderPage()
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button(copied ? "Copied" : "Copy All", systemImage: copied ? "checkmark" : "doc.on.doc") {
                        UIPasteboard.general.string = text
                        copied = true
                    }
                    .accessibilityIdentifier("tool-output-reader-copy")
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }.accessibilityIdentifier("tool-output-reader-done")
                }
            }
        }
        .task(id: text) {
            let source = text
            let split = await Task.detached(priority: .userInitiated) { ToolOutputPages.split(source) }.value
            guard !Task.isCancelled else { return }
            pages = split
            page = min(page, split.count - 1)
        }
    }

    private func pager(_ shown: Int, count: Int) -> some View {
        HStack {
            Button("Previous page", systemImage: "chevron.left") { page = max(shown - 1, 0) }
                .labelStyle(.iconOnly).frame(minWidth: 44, minHeight: 44)
                .disabled(shown == 0)
                .accessibilityIdentifier("tool-output-reader-previous")
            Spacer()
            Text("Page \(shown + 1) of \(count)")
                .font(.footnote).monospacedDigit().foregroundStyle(.secondary)
                .accessibilityIdentifier("tool-output-reader-page")
                .accessibilityAdjustableAction { direction in
                    switch direction {
                    case .increment: page = min(shown + 1, count - 1)
                    case .decrement: page = max(shown - 1, 0)
                    @unknown default: break
                    }
                }
            Spacer()
            Button("Next page", systemImage: "chevron.right") { page = min(shown + 1, count - 1) }
                .labelStyle(.iconOnly).frame(minWidth: 44, minHeight: 44)
                .disabled(shown == count - 1)
                .accessibilityIdentifier("tool-output-reader-next")
        }
        .padding(.horizontal)
        .background(.bar)
    }
}

/// TextKit lays out a bounded viewport instead of thousands of SwiftUI diff rows.
private struct ToolTextView: UIViewRepresentable {
    let text: String
    let diff: Bool
    /// A different page of the reader: start it at the top.
    var document = 0
    /// Set only in the reader, where one bounded page is one accessible element.
    var accessibilityTitle: String? = nil
    func makeUIView(context: Context) -> UITextView {
        let view=UITextView(usingTextLayoutManager: false)
        view.isEditable=false; view.isSelectable=true; view.backgroundColor = .clear
        view.textContainerInset=UIEdgeInsets(top:10,left:10,bottom:10,right:10)
        view.font = context.environment.wonderTypography.codeUIFont(.callout, size: 16)
        view.adjustsFontForContentSizeCategory=true
        let accessible = accessibilityTitle != nil
        view.isAccessibilityElement=accessible
        view.accessibilityElementsHidden = !accessible
        if accessible { view.accessibilityIdentifier = "tool-output-reader-text" }
        return view
    }
    func updateUIView(_ view: UITextView, context: Context) {
        context.coordinator.apply(text, diff: diff, font: context.environment.wonderTypography.codeUIFont(.callout, size: 16),
                                  color: context.environment.wonderTheme.textUIColor, document: document, to: view)
        if let accessibilityTitle {
            view.accessibilityLabel = accessibilityTitle
            view.accessibilityValue = text
        }
    }
    func makeCoordinator() -> ToolTextRenderer { ToolTextRenderer() }
}

/// Puts tool text in a text view without losing the reader's place: the same
/// input is skipped, output that only grew is appended (its last line
/// restyled), and any other change keeps a clamped selection and scroll offset
/// unless the reader is scrolling.
@MainActor final class ToolTextRenderer {
    private(set) var text: String?
    private var diff = false
    private var color: UIColor?
    private var font: UIFont?
    private var document = 0

    func apply(_ text: String, diff: Bool, font: UIFont, color: UIColor, document: Int = 0, to view: UITextView) {
        let sameStyle = diff == self.diff && color == self.color && font == self.font && document == self.document
        guard !sameStyle || self.text != text else { return }
        let change = ToolTextChange.between(self.text, text, sameStyle: sameStyle)
        let newDocument = document != self.document
        self.text = text; self.diff = diff; self.color = color; self.font = font; self.document = document
        let selection = view.selectedRange
        let offset = view.contentOffset
        switch change {
        case .unchanged:
            return
        case .append(let from) where from <= view.textStorage.length:
            let storage = view.textStorage
            let tail = Self.styled((text as NSString).substring(from: from), diff: diff, font: font, color: color)
            storage.beginEditing()
            storage.replaceCharacters(in: NSRange(location: from, length: storage.length - from), with: tail)
            storage.endEditing()
        default:
            view.attributedText = Self.styled(text, diff: diff, font: font, color: color)
        }
        if newDocument {
            view.selectedRange = NSRange(location: 0, length: 0)
            // Lay the new text out first: a later layout pass would otherwise
            // restore the previous page's offset.
            view.layoutIfNeeded()
            view.setContentOffset(CGPoint(x: 0, y: -view.adjustedContentInset.top), animated: false)
            return
        }
        if let kept = ToolTextChange.clamped(selection, toLength: view.textStorage.length) { view.selectedRange = kept }
        guard !(view.isTracking || view.isDragging || view.isDecelerating) else { return }
        view.layoutIfNeeded()
        let insets = view.adjustedContentInset
        let y = ToolTextChange.clampedOffset(Double(offset.y), contentHeight: Double(view.contentSize.height),
                                             viewportHeight: Double(view.bounds.height),
                                             topInset: Double(insets.top), bottomInset: Double(insets.bottom))
        if abs(Double(view.contentOffset.y) - y) > 0.5 { view.setContentOffset(CGPoint(x: offset.x, y: CGFloat(y)), animated: false) }
    }

    static func styled(_ text: String, diff: Bool, font: UIFont, color: UIColor) -> NSAttributedString {
        let output=NSMutableAttributedString(string:text,attributes:[.font:font,.foregroundColor:color])
        if diff {
            let source=text as NSString
            source.enumerateSubstrings(in:NSRange(location:0,length:source.length),options:.byLines) { line, range, _, _ in
                if let line, (line.hasPrefix("+") && !line.hasPrefix("+++")) || (line.hasPrefix("-") && !line.hasPrefix("---")) {
                    output.addAttribute(.backgroundColor,value:(line.hasPrefix("+") ? UIColor.systemGreen : UIColor.systemRed).withAlphaComponent(0.14),range:range)
                }
            }
        }
        return output
    }
}


/// A visual that a later one in this conversation updates
/// (`wonder_show_visual` with `replace`): one line where it was.
/// Tapping it opens the replaced version in Files.
struct UpdatedPageNotice: View {
    let model: ConnectionModel
    let chat: ChatSummary
    let file: ConversationFile
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    @State private var showingPrevious = false
    var body: some View {
        Button { showingPrevious = true } label: {
            Label {
                Text(PublishedPageView.title(of: file)).fontWeight(.medium) + Text(" · Updated below")
            } icon: {
                Image(systemName: "arrow.down.doc")
            }
            .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .font(typography.font(.footnote))
        .foregroundStyle(theme.secondaryText)
        .accessibilityHint("Opens the earlier version.")
        .accessibilityIdentifier("html-render-updated")
        .sheet(isPresented: $showingPrevious) { PreviewDeck(model: model, chat: chat, initialFile: file) }
    }
}

struct ToolFilePreview: View {
    let model: ConnectionModel
    let chat: ChatSummary
    let file: ConversationFile
    /// One picture of a gallery row: a fixed-height tile instead of its own box.
    var tile = false
    /// The pictures shown with this one; the viewer pages through them.
    var gallery: [ConversationFile] = []
    @State private var thumbnail: UIImage?
    @State private var failed = false
    /// The computer kept this picture by reference and the original is gone.
    @State private var gone = false
    @State private var showingPreview = false
    @State private var showingPhoto = false
    @State private var retry = 0
    private var request: ToolImageRequest { ToolImageRequest(scope: model.imagePreviewScope, chatID: chat.id, file: file, size: tile ? .tile : .single, retry: retry) }
    var body: some View {
        if file.mimeType?.lowercased() == "image/svg+xml" {
            InlineSVGPreview(model: model, chat: chat, file: file)
        } else if ConversationFile.publishedPageTypes.contains(file.mimeType?.lowercased() ?? "") {
            // Only a published page (T3's html_render, Cupola's
            // wonder_show_visual) is an HTML or wonder-ui tool file.
            PublishedPageView(model: model, chat: chat, file: file)
        } else if file.isPlayableMedia {
            InlineMediaPlayer(model: model, chat: chat, file: file)
        } else {
            imageOrDocument
        }
    }
    private var imageOrDocument: some View {
        VStack(alignment: .leading, spacing: 6) {
            Button {
                if PhotoViewerRouting.isImage(file) { showingPhoto = true }
                else { showingPreview = true }
            } label: {
                VStack(alignment: .leading, spacing: 6) {
                    if PhotoViewerRouting.isImage(file) {
                        ToolImageCanvas(image: thumbnail, failed: failed,
                                        layout: tile ? nil : ToolImageLayout.single(file),
                                        tileWidth: tile ? ToolImageLayout.tileWidth(file) : nil)
                    } else {
                        Label(file.name, systemImage: "doc")
                            .font(.subheadline).frame(minHeight: 44, alignment: .leading)
                    }
                }
            }.buttonStyle(.plain).accessibilityLabel("Preview \(file.name)")
                .accessibilityIdentifier("tool-image:\(file.id)")
                .accessibilityValue(PhotoViewerRouting.isImage(file) ? (thumbnail != nil ? "Loaded" : gone ? "No longer on your computer" : failed ? "Unavailable" : "Loading") : "")
                .overlay(alignment: .topLeading) {
                    if gone {
                        Text("This picture is no longer on your computer.")
                            .font(.caption).foregroundStyle(.secondary).padding(8)
                    } else if failed {
                        Button("Retry preview", systemImage: "arrow.clockwise") { retry += 1 }
                            .font(.caption).frame(minHeight: 44).padding(8)
                    }
                }
        }
        .task(id: request) { await load(request) }
        .onDisappear { thumbnail = nil }
        .sheet(isPresented: $showingPreview) { PreviewDeck(model: model, chat: chat, initialFile: file) }
        .fullScreenCover(isPresented: $showingPhoto) {
            PhotoViewer(model: model, chat: chat, file: file, galleryFiles: gallery, initialFileID: file.id)
        }
    }
    private func load(_ request: ToolImageRequest) async {
        thumbnail = nil; failed = false; gone = false
        guard PhotoViewerRouting.isImage(file) else { return }
        do {
            let image = try await model.imagePreviews.image(for: request) { [model, file, chat, size = request.size] in
                try await model.thumbnail(file, chat: chat, size: size)
            }
            try Task.checkCancellation()
            guard request == self.request else { return }
            thumbnail = image
        } catch {
            guard !Task.isCancelled, request == self.request else { return }
            failed = true
            gone = (error as? FileFailure) == .gone
        }
    }
}

/// Reserve the same space before, during and after loading. Lazy-stack scroll
/// anchoring must not compensate for an image growing underneath a gesture.
/// The space comes from the picture's pixel size the Mac reported, so it is
/// known before any bytes arrive; without one, a fixed box fits the picture.
struct ToolImageCanvas: View {
    let image: UIImage?
    let failed: Bool
    var layout: ToolImageLayout? = nil
    var tileWidth: Double? = nil
    @Environment(\.wonderTheme) private var theme
    var body: some View {
        Group {
            if let tileWidth {
                theme.surface.frame(width: tileWidth, height: ToolImageLayout.galleryHeight)
            } else if let layout {
                theme.surface.aspectRatio(layout.aspectRatio, contentMode: .fit).frame(maxWidth: layout.maxWidth)
            } else {
                theme.surface.frame(maxWidth: 360).frame(height: 220)
            }
        }
        .overlay {
            if let image {
                Image(uiImage: image).resizable()
                    .aspectRatio(contentMode: layout == nil && tileWidth == nil ? .fit : .fill)
            } else if !failed { ProgressView() }
        }
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }
}

/// Pictures a tool made one after another: one row that scrolls sideways;
/// a picture opens in the viewer, which swipes through the row's pictures.
struct ToolImageGallery: View {
    let model: ConnectionModel
    let chat: ChatSummary
    let files: [ConversationFile]
    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            LazyHStack(alignment: .top, spacing: 8) {
                ForEach(files) { file in
                    ToolFilePreview(model: model, chat: chat, file: file, tile: true, gallery: files)
                }
            }
        }
        .frame(height: ToolImageLayout.galleryHeight)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("\(files.count) pictures")
        .accessibilityIdentifier("tool-gallery:\(files.first?.id ?? "")")
    }
}

/// How large a conversation picture is drawn, as the longest edge in pixels
/// it is fetched, decoded and cached at. Each size is cached on its own, so a
/// 72-point chip never holds a full-width picture in memory, and the Mac
/// sends a JPEG no larger than the box needs.
enum PicturePreviewSize: Int, Hashable, Sendable, CaseIterable {
    /// A 72-point attachment chip in a message, the queue or the composer.
    case chip = 256
    /// One tile of a row of pictures, 200 points tall.
    case tile = 600
    /// One picture on its own row: a 360-point thumbnail on a 3x display.
    case single = 1080
    var maxPixels: Int { rawValue }
}

struct ToolImageRequest: Hashable, Sendable {
    let scope: UUID
    let chatID: String
    let fileID: String
    let sha256: String?
    let mimeType: String?
    let byteSize: Int?
    let state: String
    let updatedAt: String
    let size: PicturePreviewSize
    let retry: Int
    init(scope: UUID, chatID: String, file: ConversationFile, size: PicturePreviewSize, retry: Int = 0) {
        self.init(scope: scope, chatID: chatID, fileID: file.id, sha256: file.sha256,
            mimeType: file.mimeType, byteSize: file.byteSize, state: file.state,
            updatedAt: file.updatedAt, size: size, retry: retry)
    }
    init(scope: UUID, chatID: String, fileID: String, sha256: String?, mimeType: String?, byteSize: Int?, state: String, updatedAt: String, size: PicturePreviewSize, retry: Int = 0) {
        self.scope = scope; self.chatID = chatID; self.fileID = fileID
        self.sha256 = sha256; self.mimeType = mimeType; self.byteSize = byteSize
        self.state = state; self.updatedAt = updatedAt; self.size = size; self.retry = retry
    }
}

/// One connection owns a small disposable cache and at most two downloads/decodes.
/// Requests shared by visible rows are deduplicated; leaving the screen cancels
/// work once its last consumer disappears, including work still in the queue.
@MainActor final class ToolImagePreviews {
    private struct Job {
        let id = UUID()
        let load: @Sendable () async throws -> Data
        var waiters: [UUID: CheckedContinuation<UIImage, Error>] = [:]
        var task: Task<UIImage, Error>?
    }
    private let maximumBytes: Int
    private var cache: [ToolImageRequest: UIImage] = [:]
    private var order: [ToolImageRequest] = []
    private(set) var cachedBytes = 0
    private var jobs: [ToolImageRequest: Job] = [:]
    private var queue: [ToolImageRequest] = []
    private var running = 0
    init(maximumBytes: Int = 16 * 1024 * 1024) { self.maximumBytes = maximumBytes }

    func image(for key: ToolImageRequest, load: @escaping @Sendable () async throws -> Data) async throws -> UIImage {
        try Task.checkCancellation()
        if let image = cache[key] {
            order.removeAll { $0 == key }; order.append(key)
            #if WONDER_DIAGNOSTICS
            DiagnosticJournal.shared.record(DiagnosticEvent(operation: "image.cache", phase: "hit", count: 1))
            #endif
            return image
        }
        let consumer = UUID()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                if jobs[key] == nil { jobs[key] = Job(load: load); queue.append(key) }
                jobs[key]?.waiters[consumer] = continuation
                startNext()
            }
        } onCancel: {
            Task { @MainActor in self.cancel(key, consumer: consumer) }
        }
    }

    func invalidate() {
        removeCachedImages()
        for job in jobs.values {
            job.task?.cancel()
            for waiter in job.waiters.values { waiter.resume(throwing: CancellationError()) }
        }
        jobs.removeAll(); queue.removeAll()
    }

    func removeCachedImages() { cache.removeAll(); order.removeAll(); cachedBytes = 0 }

    private func cancel(_ key: ToolImageRequest, consumer: UUID) {
        guard let waiter = jobs[key]?.waiters.removeValue(forKey: consumer) else { return }
        waiter.resume(throwing: CancellationError())
        if jobs[key]?.waiters.isEmpty == true {
            jobs.removeValue(forKey: key)?.task?.cancel()
            queue.removeAll { $0 == key }
        }
    }

    private func startNext() {
        while running < 2, !queue.isEmpty {
            let key = queue.removeFirst()
            guard let job = jobs[key] else { continue }
            running += 1
            let worker = Task.detached(priority: .utility) { [load = job.load, maxPixels = key.size.maxPixels] in
                #if WONDER_DIAGNOSTICS
                let started = ProcessInfo.processInfo.systemUptime
                #endif
                let data = try await load()
                try Task.checkCancellation()
                #if WONDER_DIAGNOSTICS
                let received = ProcessInfo.processInfo.systemUptime
                DiagnosticJournal.shared.record(DiagnosticEvent(operation: "image.download", durationMs: (received - started) * 1000, bytes: UInt64(data.count)))
                #endif
                guard let image = ToolPreviewImage.decode(data, maxPixels: maxPixels) else { throw FileFailure.unsupported }
                #if WONDER_DIAGNOSTICS
                DiagnosticJournal.shared.record(DiagnosticEvent(operation: "image.decode", durationMs: (ProcessInfo.processInfo.systemUptime - received) * 1000))
                #endif
                try Task.checkCancellation()
                return image
            }
            jobs[key]?.task = worker
            Task {
                let result = await worker.result
                running -= 1
                if jobs[key]?.id == job.id, let finished = jobs.removeValue(forKey: key) {
                    if case .success(let image) = result { retain(image, for: key) }
                    for waiter in finished.waiters.values { waiter.resume(with: result) }
                }
                startNext()
            }
        }
    }

    private func retain(_ image: UIImage, for key: ToolImageRequest) {
        let cost = Self.cost(image)
        guard cost <= maximumBytes else { return }
        // Bytes are the real bound; the count only caps bookkeeping, and is
        // high enough that a screenful of chips stays cached while scrolling.
        while cachedBytes + cost > maximumBytes || order.count >= Self.maximumItems {
            guard !order.isEmpty else { break }
            if let removed = cache.removeValue(forKey: order.removeFirst()) { cachedBytes -= Self.cost(removed) }
        }
        cache[key] = image; order.append(key); cachedBytes += cost
    }
    static let maximumItems = 64
    private static func cost(_ image: UIImage) -> Int {
        guard let cg = image.cgImage else { return 0 }
        return cg.bytesPerRow * cg.height
    }
}

enum ToolPreviewImage {
    /// A 360-point thumbnail needs at most 1080 pixels, even on a 3x display;
    /// smaller boxes pass their own `PicturePreviewSize`.
    nonisolated static func decode(_ data: Data, maxPixels: Int = PicturePreviewSize.single.maxPixels) -> UIImage? {
        autoreleasepool {
            guard let source=CGImageSourceCreateWithData(data as CFData,[kCGImageSourceShouldCache:false] as CFDictionary),
                  let image=CGImageSourceCreateThumbnailAtIndex(source,0,[kCGImageSourceCreateThumbnailFromImageAlways:true,kCGImageSourceCreateThumbnailWithTransform:true,kCGImageSourceThumbnailMaxPixelSize:maxPixels,kCGImageSourceShouldCacheImmediately:true] as CFDictionary) else { return nil }
            return UIImage(cgImage:image)
        }
    }
}

/// "Changed 2 files · +5 −1" after a finished reply that edited files. Its
/// counts come from the reply's saved edits (a Codex reply's whole-turn diff,
/// which also covers files written from the shell), prepared with the timeline.
struct ReplyChangesRow: View {
    let edits: ResponseEditedFiles
    let open: () -> Void
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize

    init(edits: ResponseEditedFiles, open: @escaping () -> Void) {
        self.edits = edits
        self.open = open
    }

    var body: some View {
        Button(action: open) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: "doc.badge.ellipsis").foregroundStyle(.secondary)
                if dynamicTypeSize.isAccessibilitySize {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(edits.title)
                        counts
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Text(edits.title).lineLimit(1)
                    counts
                    Spacer(minLength: 0)
                }
                Image(systemName: "chevron.right").font(.caption2).foregroundStyle(.secondary)
            }
            .font(.subheadline.weight(.medium))
            .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(edits.accessibilityLabel)
        .accessibilityHint("Shows the files this reply changed")
        .accessibilityIdentifier("reply-changes:" + edits.turnID)
    }

    private var counts: some View {
        HStack(spacing: 6) {
            if let added = edits.additions {
                Text("+\(added)").foregroundStyle(DiffColors.added(colorScheme))
            }
            if let removed = edits.deletions, removed > 0 || edits.additions == nil {
                Text("−\(removed)").foregroundStyle(DiffColors.removed(colorScheme))
            }
        }
        .monospacedDigit()
        .fixedSize(horizontal: true, vertical: false)
    }
}

/// The files one reply changed, each opening its saved diff: a sheet on a
/// phone-width window, the companion pane in a wide one.
struct ReplyChangesView: View {
    /// Nil once the reply's edits are no longer in the loaded conversation.
    let edits: ResponseEditedFiles?
    private let dismiss = ToolDismiss()
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.companionPane) private var pane
    @Environment(\.toolHasGestureDismissal) private var gestureDismissal
    @State private var verticalBar = false
    @State private var path: [FileTarget] = []
    /// A reference the pushed diff and its header read, so a width measured after the push reaches them.
    @State private var measured = ChangedFilesWidth()
    private var inlineHeader: Bool { CompanionPaneChrome.usesInlineHeader(pane, verticalBar: verticalBar) }

    private struct FileTarget: Hashable { let path: String }


    init(edits: ResponseEditedFiles?) {
        self.edits = edits
        #if WONDER_DIAGNOSTICS
        // Opens the Nth file's diff at launch, where a scenario can't tap (the unfolded Duo, #387).
        let arguments = ProcessInfo.processInfo.arguments
        if let flag = arguments.firstIndex(of: "-diagnostics-reply-changes-open"), flag + 1 < arguments.count,
           let number = Int(arguments[flag + 1]), let files = edits?.files, files.indices.contains(number - 1) {
            _path = State(initialValue: [FileTarget(path: files[number - 1].path)])
        }
        #endif
    }

    var body: some View {
        NavigationStack(path: $path) {
            WonderList {
                if let edits, !edits.files.isEmpty {
                    Section {
                        ForEach(edits.files) { file in
                            NavigationLink(value: FileTarget(path: file.path)) { row(file) }
                                .accessibilityIdentifier("reply-file:" + file.path)
                        }
                    }
                } else {
                    Text("These changes are no longer in this conversation.")
                        .font(.subheadline).foregroundStyle(.secondary)
                        .accessibilityIdentifier("reply-changes-unavailable")
                }
            }
            // No section header above the files: the list starts under the title, not a header's height below it.
            .environment(\.defaultMinListHeaderHeight, 0)
            .contentMargins(.top, 8, for: .scrollContent)
            .accessibilityIdentifier("reply-changes")
            .navigationTitle("Changed files")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                if !inlineHeader && (pane != nil || !gestureDismissal) {
                    ToolbarItem(placement: .confirmationAction) {
                        HStack {
                            ToolCloseControl { doneButton }
                            CompanionExpandButton()
                        }
                    }
                }
            }
            .modifier(paneHeader("Changed files"))
            .navigationDestination(for: FileTarget.self) { target in
                if let file = edits?.files.first(where: { $0.path == target.path }) {
                    ReplyFileDiffView(file: file, measured: measured)
                        // One header row: back, the file's relative path with its language and counts, the layout menu.
                        .modifier(paneHeader(file.path, showsBack: true,
                                             showsDiffLayout: !file.patches.isEmpty,
                                             subtitle: AnyView(ChangedFileSubtitle(file: file))))
                } else {
                    WonderList { Text("This file is no longer listed.") }
                        .navigationTitle("Changed files")
                        .navigationBarTitleDisplayMode(.inline)
                        .modifier(paneHeader("Changed files", showsBack: true))
                }
            }
        }
        .readsVerticalBar($verticalBar)
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { measured.width = $0 }
    }

    private var doneButton: some View {
        Button { dismiss() } label: {
            Text("Close").frame(minWidth: inlineHeader ? 44 : nil, minHeight: inlineHeader ? 44 : nil)
                .contentShape(Rectangle())
        }.accessibilityIdentifier("reply-changes-done")
    }

    private func paneHeader(_ title: String, showsBack: Bool = false, showsDiffLayout: Bool = false,
                            subtitle: AnyView? = nil) -> some ViewModifier {
        CompanionPaneNavigationHeader(enabled: inlineHeader, title: title, showsBack: showsBack, back: goBack, subtitle: subtitle) {
            if showsDiffLayout { DiffLayoutMenu(measured: measured) }
            ToolCloseControl { doneButton }
        }
    }

    private func goBack() {
        if !path.isEmpty { path.removeLast() }
    }

    private func row(_ file: ResponseEditedFile) -> some View {
        ChangedFileRow(file: file)
    }
}

/// The width of the Changed files pane. Two diff columns need 300 pt each, so a narrower pane (the Duo's half,
/// a phone) shows the unified diff only and does not offer the layout choice.
@MainActor @Observable final class ChangedFilesWidth {
    var width: CGFloat = 0
    var canSplit: Bool { width >= 601 }
}

/// The header's diff layout menu; absent where the pane can only show the unified diff.
private struct DiffLayoutMenu: View {
    let measured: ChangedFilesWidth
    @AppStorage("wonder.diff.layout") private var diffLayout = DiffLayout.automatic
    @Environment(\.horizontalSizeClass) private var sizeClass

    var body: some View {
        if measured.canSplit {
            Menu {
                Picker("Diff layout", selection: Binding(get: { diffLayout.split(sizeClass) },
                    set: { diffLayout = $0 ? .split : .unified })) {
                    Text("Unified").tag(false)
                    Text("Side by side").tag(true)
                }
                .pickerStyle(.inline)
            } label: {
                Label("Diff layout", systemImage: "rectangle.split.2x1").labelStyle(.iconOnly)
                    .frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
            }
            .accessibilityIdentifier("diff-layout")
        }
    }
}

/// A language badge's name for a file: "Swift" for `Chat.swift`, the uppercased
/// extension for one it doesn't know, "Text" when the name has none.
enum FileLanguageBadge {
    private static let names: [String: String] = [
        "swift": "Swift", "rs": "Rust", "py": "Python", "js": "JavaScript", "mjs": "JavaScript", "jsx": "JSX",
        "ts": "TypeScript", "tsx": "TSX", "html": "HTML", "htm": "HTML", "css": "CSS", "scss": "SCSS",
        "json": "JSON", "md": "Markdown", "markdown": "Markdown", "yml": "YAML", "yaml": "YAML", "toml": "TOML",
        "sh": "Shell", "bash": "Shell", "zsh": "Shell", "c": "C", "h": "C", "cc": "C++", "cpp": "C++", "hpp": "C++",
        "m": "Objective-C", "mm": "Objective-C++", "java": "Java", "kt": "Kotlin", "go": "Go", "rb": "Ruby",
        "php": "PHP", "sql": "SQL", "xml": "XML", "plist": "Plist", "txt": "Text", "cs": "C#", "lua": "Lua",
    ]
    private static let whole: [String: String] = ["dockerfile": "Dockerfile", "makefile": "Makefile"]

    static func name(forPath path: String) -> String {
        let file = (path as NSString).lastPathComponent.lowercased()
        if let name = whole[file] { return name }
        let ext = (file as NSString).pathExtension
        guard !ext.isEmpty else { return "Text" }
        return names[ext] ?? ext.uppercased()
    }
}

/// The small rounded tag naming a file's language.
private struct LanguageBadge: View {
    let language: String
    var body: some View {
        Text(language)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 6).padding(.vertical, 2)
            .background(Color.secondary.opacity(0.16), in: RoundedRectangle(cornerRadius: 4))
            .fixedSize()
    }
}

/// "+3 −1": both counts whenever they are known.
private struct ChangeCounts: View {
    let additions: Int?
    let deletions: Int?
    @Environment(\.colorScheme) private var scheme
    var body: some View {
        HStack(spacing: 6) {
            if let additions { Text("+\(additions)").foregroundStyle(DiffColors.added(scheme)) }
            if let deletions { Text("−\(deletions)").foregroundStyle(DiffColors.removed(scheme)) }
        }
        .font(.caption.monospacedDigit())
        .fixedSize()
    }
}

extension ResponseEditedFile {
    /// "Sources/ChatView.swift, Swift, 2 added lines, 1 removed lines".
    fileprivate var rowAccessibilityLabel: String {
        ([path, FileLanguageBadge.name(forPath: path)]
            + [additions.map { "\($0) added lines" }, deletions.map { "\($0) removed lines" }].compactMap { $0 }).joined(separator: ", ")
    }
}

/// One file the reply changed: its relative path, language badge and counts.
private struct ChangedFileRow: View {
    let file: ResponseEditedFile

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: "doc.text").foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 4) {
                Text(file.path).font(.body).lineLimit(3).truncationMode(.middle)
                    .fixedSize(horizontal: false, vertical: true)
                HStack(spacing: 8) {
                    LanguageBadge(language: FileLanguageBadge.name(forPath: file.path))
                    ChangeCounts(additions: file.additions, deletions: file.deletions)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(file.rowAccessibilityLabel)
        .accessibilityHint("Shows this file's diff")
    }
}

/// The language badge and counts under the file's path in the diff's header row.
private struct ChangedFileSubtitle: View {
    let file: ResponseEditedFile

    var body: some View {
        HStack(spacing: 8) {
            LanguageBadge(language: FileLanguageBadge.name(forPath: file.path))
            ChangeCounts(additions: file.additions, deletions: file.deletions)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(file.rowAccessibilityLabel)
        .accessibilityAddTraits(.isHeader)
        .accessibilityIdentifier("reply-diff-header")
    }
}

/// One file's saved patches from a reply, parsed off the main thread when
/// opened, in the shared diff renderer.
private struct ReplyFileDiffView: View {
    let file: ResponseEditedFile
    let measured: ChangedFilesWidth
    @State private var prepared: PreparedDiff?
    @AppStorage("wonder.diff.layout") private var layout = DiffLayout.automatic
    @Environment(\.horizontalSizeClass) private var sizeClass
    @Environment(\.wonderTheme) private var theme
    @Environment(\.companionPane) private var pane
    @State private var verticalBar = false

    var body: some View {
        Group {
            if file.patches.isEmpty {
                ContentUnavailableView {
                    Label("No diff saved", systemImage: "doc.text.magnifyingglass")
                } description: {
                    Text(DiffDocument.filenameOnlyNote)
                }
                .accessibilityIdentifier("changes-diff-message")
            } else if let prepared {
                DiffDocument(sections: [DiffDocument.Section(id: file.path, title: file.path,
                    additions: file.additions, deletions: file.deletions, expanded: true, diff: prepared,
                    note: file.partial ? "Only the first part of this diff was saved. Review the full change on your computer." : nil,
                    identifier: "changes-diff-file")], split: layout.split(sizeClass) && measured.canSplit, showsHeaders: false) { _ in }
            } else {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
                    .accessibilityIdentifier("changes-diff-loading")
            }
        }
        // Folded Duo: the tool is a sheet and the system's vertical bar floats over its trailing edge; the
        // lists keep a 16 pt margin there, so the diff does too (measured on the Duo simulator: the diff then
        // ends at 366 pt and the bar starts at 396). In the unfolded pane the bar sits outside it.
        .padding(.trailing, verticalBar && pane == nil ? 16 : 0)
        .readsVerticalBar($verticalBar)
        .background(theme.page.ignoresSafeArea())
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if prepared != nil && measured.canSplit {
                ToolbarItem(placement: .primaryAction) { DiffLayoutPicker(layout: $layout, split: layout.split(sizeClass)) }
            }
        }
        .task(id: file.path) {
            guard prepared == nil, !file.patches.isEmpty else { return }
            let patches = file.patches, path = file.path
            let result = await Task.detached(priority: .userInitiated) { PreparedDiff(patches: patches, path: path) }.value
            guard !Task.isCancelled else { return }
            prepared = result
        }
    }
}
