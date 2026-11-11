import SwiftUI
import WonderPairing

/// Finished highlighted text per (code, language, theme), bounded in count and
/// bytes so scrolling back through a long chat does not re-tokenize.
final class CodeHighlightCache: @unchecked Sendable {
    static let shared = CodeHighlightCache()
    private final class Box { let value: AttributedString; init(_ value: AttributedString) { self.value = value } }
    private let cache: NSCache<NSString, Box> = {
        let cache = NSCache<NSString, Box>()
        cache.countLimit = 80
        cache.totalCostLimit = 4_000_000
        return cache
    }()

    static func key(code: String, language: String?, theme: WonderTheme) -> String {
        "\(theme.cacheKey)|\(language ?? "")|\(code.utf8.count)|\(code.hashValue)"
    }
    func value(for key: String) -> AttributedString? { cache.object(forKey: key as NSString)?.value }
    func store(_ value: AttributedString, for key: String, cost: Int) {
        cache.setObject(Box(value), forKey: key as NSString, cost: cost)
    }

    /// Tokenizes and colours a block with the theme's syntax palette. Pure, so
    /// callers run it off the main thread; nil when the block stays plain.
    static func highlight(_ code: String, language: String?, palette: ThemePalette) -> AttributedString? {
        guard let lines = CodeHighlighter.lines(code, language: language) else { return nil }
        return attributed(lines: lines, palette: palette)
    }

    /// Joins tokenized lines into coloured text with the theme's syntax palette.
    static func attributed(lines: [[SyntaxSpan]], palette: ThemePalette) -> AttributedString {
        var output = AttributedString()
        for (index, spans) in lines.enumerated() {
            if index > 0 { output.append(AttributedString("\n")) }
            for span in spans {
                var run = AttributedString(span.text)
                if let role = span.role { run.foregroundColor = Color(hex: palette.color(for: role)) }
                output.append(run)
            }
        }
        return output
    }
}

/// A fenced code block in a message. It shows plain text at once and swaps in
/// the coloured text when the off-main tokenizer finishes, or immediately from
/// the cache. Unknown languages and very long blocks stay plain.
struct CodeBlockView: View {
    let code: String
    let language: String?
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    @Environment(\.codeBlocksWrap) private var wraps
    @Environment(\.dynamicTypeSize) private var typeSize
    @State private var hasMoreToTheRight = false
    @State private var colored: (key: String, text: AttributedString)?
    @State private var showsAll = false
    /// A wrapped block taller than this many lines starts shortened.
    private static let wrappedLines = 12

    var body: some View {
        let key = CodeHighlightCache.key(code: code, language: language, theme: theme)
        let text = CodeHighlightCache.shared.value(for: key) ?? (colored?.key == key ? colored?.text : nil)
        Group {
            if wraps || typeSize.isAccessibilitySize { wrapped(text ?? AttributedString(code)) }
            else {
                ScrollView(.horizontal) {
                    Text(text ?? AttributedString(code))
                        .font(typography.codeFont(.body))
                        .foregroundStyle(theme.primaryText)
                        .textSelection(.enabled).fixedSize(horizontal: true, vertical: false)
                        .padding(10)
                }
                .onScrollGeometryChange(for: Bool.self) { geometry in
                    geometry.contentOffset.x + geometry.containerSize.width < geometry.contentSize.width - 1
                } action: { _, value in hasMoreToTheRight = value }
                // The hint sits on a fade edge so no code shows under it, and it overlays the block without
                // adding height: reserving room for it when the block overflowed fed back into the row's
                // layout and pinned the main thread on the unfolded Duo. It leaves once the end is reached.
                .overlay(alignment: .trailing) {
                    if hasMoreToTheRight {
                        HStack(spacing: 0) {
                            LinearGradient(colors: [theme.codeBackground.opacity(0), theme.codeBackground],
                                           startPoint: .leading, endPoint: .trailing)
                                .frame(width: 16)
                            theme.codeBackground.frame(width: 22)
                                .overlay(alignment: .bottom) {
                                    Image(systemName: "arrow.left.and.right")
                                        .font(.caption2)
                                        .foregroundStyle(theme.secondaryText)
                                        .padding(.bottom, 8)
                                }
                        }
                        .clipShape(UnevenRoundedRectangle(bottomTrailingRadius: 8, topTrailingRadius: 8))
                        .allowsHitTesting(false)
                        .accessibilityLabel("Scroll code horizontally")
                    }
                }
            }
        }
        .background(theme.codeBackground, in: RoundedRectangle(cornerRadius: 8))
        .task(id: key) {
            guard text == nil, CodeHighlighter.supports(language: language) else { return }
            // A streaming block changes on every delta; wait for it to settle.
            try? await Task.sleep(for: .milliseconds(150))
            guard !Task.isCancelled else { return }
            let (code, language, palette) = (code, language, theme.palette)
            let result = await Task.detached(priority: .userInitiated) {
                CodeHighlightCache.highlight(code, language: language, palette: palette)
            }.value
            guard !Task.isCancelled, let result else { return }
            CodeHighlightCache.shared.store(result, for: key, cost: code.utf8.count * 4)
            colored = (key, result)
        }
    }
}

extension CodeBlockView {
    /// Wrapped lines for reading on a narrow screen: no sideways scrolling. A
    /// long block shows its start and opens fully on a tap.
    @ViewBuilder fileprivate func wrapped(_ text: AttributedString) -> some View {
        // Wrapped width is unknown here; long single lines count by characters.
        let long = code.split(separator: "\n", omittingEmptySubsequences: false).reduce(0) { $0 + 1 + $1.count / 48 } > Self.wrappedLines
        VStack(alignment: .leading, spacing: 6) {
            Text(text)
                .font(typography.codeFont(.callout))
                .foregroundStyle(theme.primaryText)
                .textSelection(.enabled)
                .lineLimit(long && !showsAll ? Self.wrappedLines : nil)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            if long {
                Button(showsAll ? "Show less" : "Show all") { showsAll.toggle() }
                    .font(.caption.weight(.semibold))
                    .frame(minHeight: 44, alignment: .leading)
                    .accessibilityIdentifier("code-block-show-all")
            }
        }
        .padding(.horizontal, 10).padding(.top, 10).padding(.bottom, long ? 0 : 10)
    }
}

extension EnvironmentValues {
    /// Code blocks wrap their lines instead of scrolling sideways (an agent task's details).
    @Entry var codeBlocksWrap = false
}

/// A `!` shell command or slash command the owner ran in Claude Code: the
/// command, then its output and errors in monospace. Long output starts
/// collapsed; errors are marked apart from output.
struct CommandBlockCard: View {
    let block: ClaudeCommandBlock
    @Environment(\.wonderTheme) private var theme

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            header
            if !block.output.isEmpty {
                CommandOutputSection(title: "Output", text: block.output, lineCount: block.outputLineCount, isError: false)
            }
            if !block.errorOutput.isEmpty {
                CommandOutputSection(title: "Errors", text: block.errorOutput, lineCount: block.errorLineCount, isError: true)
            }
            ForEach(Array(block.notes.enumerated()), id: \.offset) { _, note in
                Text(note).font(.subheadline).foregroundStyle(theme.secondaryText)
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("command-card")
    }

    @ViewBuilder private var header: some View {
        switch block.kind {
        case .shell:
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text("$").foregroundStyle(theme.secondaryText).accessibilityHidden(true)
                Text(block.command.isEmpty ? "Shell command" : block.command)
                    .textSelection(.enabled)
            }
            .font(.system(.subheadline, design: .monospaced).weight(.medium))
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Shell command: \(block.command)")
            .accessibilityIdentifier("command-card-command")
        case .slash:
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(block.command.isEmpty ? "Command" : block.command)
                    .font(.system(.subheadline, design: .monospaced).weight(.semibold))
                    .foregroundStyle(theme.accent)
                    .padding(.horizontal, 8).padding(.vertical, 3)
                    .background(theme.accent.opacity(0.14), in: Capsule())
                    .accessibilityIdentifier("slash-command-chip")
                if !block.arguments.isEmpty {
                    Text(block.arguments).font(.subheadline).textSelection(.enabled)
                }
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Command \(block.command) \(block.arguments)")
            .accessibilityIdentifier("command-card-command")
        }
    }
}

/// One stream of command output. Short output shows at once; longer output
/// opens on a tap and shows a bounded prefix, with the whole text on Copy.
private struct CommandOutputSection: View {
    let title: String
    let text: String
    let lineCount: Int
    let isError: Bool
    @Environment(\.wonderTheme) private var theme
    @State private var expanded: Bool?
    private static let shortLines = 8
    private static let shownCharacters = 20_000

    var body: some View {
        let isExpanded = expanded ?? (lineCount <= Self.shortLines)
        let id = isError ? "command-card-errors" : "command-card-output"
        VStack(alignment: .leading, spacing: 4) {
            Button {
                expanded = !isExpanded
            } label: {
                HStack(spacing: 4) {
                    Image(systemName: "chevron.right").font(.caption2.weight(.semibold))
                        .rotationEffect(.degrees(isExpanded ? 90 : 0))
                    Text(title)
                    Text(lineCount == 1 ? "1 line" : "\(lineCount) lines").foregroundStyle(theme.secondaryText)
                }
                .font(.caption.weight(.semibold))
                .foregroundStyle(isError ? Color.red : theme.secondaryText)
                .frame(minHeight: 28)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("\(title), \(lineCount == 1 ? "1 line" : "\(lineCount) lines")")
            .accessibilityValue(isExpanded ? "Expanded" : "Collapsed")
            .accessibilityHint(isExpanded ? "Hides the \(title.lowercased())" : "Shows the \(title.lowercased())")
            .accessibilityIdentifier(id + "-toggle")
            if isExpanded {
                VStack(alignment: .leading, spacing: 4) {
                    ScrollView(.horizontal) {
                        Text(text.utf8.count > Self.shownCharacters ? String(text.prefix(Self.shownCharacters)) : text)
                            .font(.system(.caption, design: .monospaced))
                            .foregroundStyle(isError ? Color.red : theme.primaryText)
                            .textSelection(.enabled)
                            .fixedSize()
                            .padding(8)
                    }
                    if text.utf8.count > Self.shownCharacters {
                        Text("Showing the start of long output. Copy has all of it.")
                            .font(.caption2).foregroundStyle(theme.secondaryText)
                    }
                }
                .background(isError ? Color.red.opacity(0.10) : theme.codeBackground, in: RoundedRectangle(cornerRadius: 8))
                .overlay(alignment: .leading) {
                    if isError { Rectangle().fill(Color.red).frame(width: 3).clipShape(RoundedRectangle(cornerRadius: 1.5)) }
                }
                .accessibilityIdentifier(id)
            }
        }
    }
}
