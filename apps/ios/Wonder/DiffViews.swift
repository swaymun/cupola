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

enum DiffColors {
    static func added(_ scheme: ColorScheme) -> Color { scheme == .dark ? .green : Color(red: 0.1, green: 0.42, blue: 0.2) }
    static func removed(_ scheme: ColorScheme) -> Color { scheme == .dark ? .red : Color(red: 0.75, green: 0.12, blue: 0.16) }
}

enum DiffLayout: String {
    case automatic, unified, split
    func split(_ sizeClass: UserInterfaceSizeClass?) -> Bool {
        self == .split || (self == .automatic && sizeClass == .regular)
    }
}

struct DiffLayoutPicker: View {
    @Binding var layout: DiffLayout
    let split: Bool
    var body: some View {
        Picker("Diff layout", selection: Binding(get: { split }, set: { layout = $0 ? .split : .unified })) {
            Image(systemName: "rectangle.grid.1x2").accessibilityLabel("Unified").tag(false)
            Image(systemName: "rectangle.split.2x1").accessibilityLabel("Side by side").tag(true)
        }
        .pickerStyle(.segmented).fixedSize()
        .accessibilityIdentifier("diff-layout")
    }
}

/// A diff parsed off the main thread, with what its layout needs.
struct PreparedDiff: Sendable {
    let lines: [DiffLine]
    let rows: [SplitDiffRow]
    let text: String
    let longestLine: Int
    let numberDigits: Int
    let oversized: Bool
    /// Coloured code for added, removed and context lines, keyed by line id.
    /// Empty for file types without a highlighter.
    let highlighted: [Int: AttributedString]
    init(patches: [String], path: String? = nil) {
        let size = patches.reduce(0) { $0 + $1.utf8.count }
        oversized = size > 512 * 1024
        text = oversized ? "" : patches.enumerated().map { index, patch in
            patches.count == 1 ? patch : "Edit \(index + 1)\n" + patch
        }.joined(separator: "\n")
        // The file name heads each diff; git's own header lines repeat it.
        lines = DiffLine.parse(text).filter { line in
            !(line.kind == .note && ["diff ", "index ", "--- ", "+++ "].contains { line.text.hasPrefix($0) })
        }
        rows = SplitDiffRow.pair(lines)
        longestLine = lines.reduce(0) { max($0, $1.text.count) }
        numberDigits = max(2, String(lines.reduce(0) { max($0, $1.oldNumber ?? 0, $1.newNumber ?? 0) }).count)
        let parsed = lines
        highlighted = path.flatMap { SyntaxHighlighter(path: $0) }.map { Self.highlight(parsed, with: $0) } ?? [:]
    }

    private static func highlight(_ lines: [DiffLine], with highlighter: SyntaxHighlighter) -> [Int: AttributedString] {
        var highlighter = highlighter
        var result: [Int: AttributedString] = [:]
        for line in lines {
            let sign: String
            switch line.kind {
            case .added: sign = "+ "
            case .removed: sign = "− "
            case .context: sign = "  "
            case .hunk, .note: highlighter.reset(); continue
            }
            var text = AttributedString(sign)
            for span in highlighter.spans(line.text) {
                var run = AttributedString(span.text)
                if let role = span.role { run.foregroundColor = SyntaxColors.color(role) }
                text += run
            }
            result[line.id] = text
        }
        return result
    }
}

/// Files with collapsible diffs in one lazy stack.
struct DiffDocument: View {
    @Environment(\.wonderTheme) private var theme
    struct Section: Identifiable {
        let id: String
        let title: String
        let additions: Int?
        let deletions: Int?
        let expanded: Bool
        let diff: PreparedDiff?
        let note: String?
        let identifier: String
    }
    static let filenameOnlyNote = "Only the filename was saved for this edit."
    let sections: [Section]
    let split: Bool
    var showsHeaders = true
    let toggle: (String) -> Void
    @Environment(\.colorScheme) private var scheme
    var body: some View {
        GeometryReader { geometry in
            let column = max(300, (geometry.size.width - 1) / 2)
            ScrollView(split ? [.vertical, .horizontal] : .vertical) {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(sections) { section in
                        if showsHeaders { header(section, width: split ? column * 2 : geometry.size.width) }
                        if section.expanded { content(section, column: column) }
                    }
                }
                .frame(width: split ? column * 2 + 1 : geometry.size.width, alignment: .leading)
            }
            .accessibilityIdentifier("diff-document")
        }
    }
    private func header(_ section: Section, width: CGFloat) -> some View {
        Button { toggle(section.id) } label: {
            HStack(spacing: 10) {
                Image(systemName: section.expanded ? "chevron.down" : "chevron.right")
                    .font(.caption.weight(.semibold)).foregroundStyle(.secondary).frame(width: 14)
                Text(section.title).font(.subheadline).lineLimit(2).truncationMode(.middle)
                    .frame(maxWidth: .infinity, alignment: .leading)
                if let added = section.additions { Text("+\(added)").foregroundStyle(DiffColors.added(scheme)) }
                if let removed = section.deletions { Text("−\(removed)").foregroundStyle(DiffColors.removed(scheme)) }
            }
            .font(.caption.monospacedDigit())
            .padding(.horizontal, 16).frame(minHeight: 44)
            .frame(maxWidth: min(width, 900), alignment: .leading)
            .background(theme.page)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(([section.title] + [section.additions.map { "\($0) added lines" }, section.deletions.map { "\($0) removed lines" }].compactMap { $0 }).joined(separator: ", "))
        .accessibilityValue(section.expanded ? "Expanded" : "Collapsed")
        .accessibilityIdentifier(section.identifier)
    }
    @ViewBuilder private func content(_ section: Section, column: CGFloat) -> some View {
        if let note = section.note, section.diff == nil || note == Self.filenameOnlyNote {
            Text(note).font(.footnote).foregroundStyle(.secondary).padding(.horizontal, 16).padding(.vertical, 8)
        } else if let diff = section.diff {
            if diff.oversized {
                Text("This diff is too large to preview. Review these saved changes on your computer.")
                    .font(.footnote).foregroundStyle(.secondary).padding(.horizontal, 16).padding(.vertical, 8)
            } else {
                // One element reads the whole diff; the drawn rows stay lazy.
                Color.clear.frame(height: 1)
                    .accessibilityElement()
                    .accessibilityLabel(section.title + "\n" + diff.text)
                    .accessibilityIdentifier("workspace-diff-text")
                if split {
                    ForEach(diff.rows) { row in
                        SplitDiffRowView(row: row, digits: diff.numberDigits, column: column, highlighted: diff.highlighted).accessibilityHidden(true)
                    }
                } else {
                    ForEach(diff.lines) { line in
                        DiffLineView(line: line, digits: diff.numberDigits, highlighted: diff.highlighted[line.id]).accessibilityHidden(true)
                    }
                }
                if let note = section.note {
                    Text(note).font(.footnote).foregroundStyle(.secondary).padding(.horizontal, 16).padding(.vertical, 8)
                }
            }
        } else {
            ProgressView().frame(maxWidth: .infinity).padding(.vertical, 12)
        }
    }
}

private struct DiffLineView: View {
    let line: DiffLine
    let digits: Int
    let highlighted: AttributedString?
    @Environment(\.colorScheme) private var scheme
    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 0) {
            DiffNumber(value: line.kind == .added ? nil : line.oldNumber, digits: digits)
            DiffNumber(value: line.kind == .removed ? nil : line.newNumber, digits: digits)
            DiffText(line: line, highlighted: highlighted)
        }
        .background(DiffBackground.color(line.kind, scheme))
    }
}

private struct SplitDiffRowView: View {
    @Environment(\.wonderTheme) private var theme
    let row: SplitDiffRow
    let digits: Int
    let column: CGFloat
    let highlighted: [Int: AttributedString]
    @Environment(\.colorScheme) private var scheme
    var body: some View {
        if let line = row.left, line == row.right, line.kind == .hunk || line.kind == .note {
            DiffText(line: line, highlighted: nil).frame(width: column * 2, alignment: .leading)
                .background(DiffBackground.color(line.kind, scheme))
        } else {
            HStack(alignment: .top, spacing: 1) {
                side(row.left, number: row.left?.oldNumber)
                side(row.right, number: row.right?.newNumber)
            }
            .background(theme.rule)
        }
    }
    private func side(_ line: DiffLine?, number: Int?) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 0) {
            DiffNumber(value: line == nil ? nil : number, digits: digits)
            if let line { DiffText(line: line, highlighted: highlighted[line.id]) } else { Color.clear.frame(maxWidth: .infinity, minHeight: 1) }
        }
        .frame(width: column - 0.5, alignment: .leading)
        .frame(maxHeight: .infinity, alignment: .top)
        .background(line.map { DiffBackground.color($0.kind, scheme) } ?? theme.page)
    }
}

private struct DiffNumber: View {
    let value: Int?
    let digits: Int
    var body: some View {
        Text(value.map(String.init) ?? "")
            .font(.system(.caption2, design: .monospaced)).foregroundStyle(.tertiary)
            .frame(width: CGFloat(digits) * 8 + 10, alignment: .trailing)
            .padding(.trailing, 6)
            .accessibilityHidden(true)
    }
}

private struct DiffText: View {
    let line: DiffLine
    let highlighted: AttributedString?
    /// Read so a theme change re-resolves the syntax colours baked into `highlighted`.
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    private var plain: AttributedString {
        let sign: String = switch line.kind { case .added: "+"; case .removed: "−"; default: " " }
        return AttributedString((line.kind == .hunk || line.kind == .note ? "" : sign + " ") + line.text)
    }
    var body: some View {
        Text(highlighted ?? plain)
            .font(typography.codeFont(.footnote))
            .foregroundStyle(line.kind == .hunk || line.kind == .note ? Color.secondary : Color.primary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.vertical, 1).padding(.trailing, 8)
            .padding(.leading, line.kind == .hunk || line.kind == .note ? 12 : 0)
            .textSelection(.enabled)
    }
}

enum DiffBackground {
    static func color(_ kind: DiffLine.Kind, _ scheme: ColorScheme) -> Color {
        switch kind {
        case .added: Color.green.opacity(scheme == .dark ? 0.22 : 0.14)
        case .removed: Color.red.opacity(scheme == .dark ? 0.22 : 0.12)
        case .hunk: Color.accentColor.opacity(0.08)
        default: Color.clear
        }
    }
}

struct WorkspaceDiffPreview: View {
    let path: String
    let state: String
    let diff: String
    var showState = true
    var onClose: (() -> Void)? = nil
    @Environment(\.dismiss) private var dismiss
    @Environment(\.horizontalSizeClass) private var sizeClass
    @AppStorage("wonder.diff.layout") private var layout = DiffLayout.automatic
    @State private var prepared: PreparedDiff?
    var body: some View {
        NavigationStack { // theme-exempt: shown inside WorkspaceBrowser over its host's page
            Group {
                if diff.isEmpty {
                    Text("No textual diff is available for this change.").foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .accessibilityIdentifier("workspace-diff-text")
                } else {
                    DiffDocument(sections: [DiffDocument.Section(id: path, title: path, additions: nil, deletions: nil,
                        expanded: true, diff: prepared, note: nil, identifier: "workspace-diff-file")],
                                 split: layout.split(sizeClass), showsHeaders: false) { _ in }
                }
            }
            .navigationTitle(path).navigationBarTitleDisplayMode(.inline)
            .toolbar(onClose == nil ? .visible : .hidden, for: .navigationBar)
            .toolbar {
                if onClose == nil {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Done") { dismiss() }
                            .accessibilityIdentifier("workspace-diff-close")
                    }
                    ToolbarItem(placement: .primaryAction) { DiffLayoutPicker(layout: $layout, split: layout.split(sizeClass)) }
                }
            }
            .safeAreaInset(edge: .bottom) {
                if showState { Text(state).font(.caption).foregroundStyle(.secondary).padding(.horizontal).padding(.bottom, 8) }
            }
            .task(id: diff) {
                let diff = diff, path = path
                prepared = await Task.detached(priority: .userInitiated) { PreparedDiff(patches: [diff], path: path) }.value
            }
        }
    }
}
