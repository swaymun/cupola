import SwiftUI
import UIKit
import WonderPairing
import os

/// Prepared viewer content per (file hash, view, theme), bounded in count and
/// bytes so reopening a file or flipping a toggle does not redo the work.
final class TextViewerCache: @unchecked Sendable {
    static let shared = TextViewerCache()
    private let cache: NSCache<NSString, AnyObject> = {
        let cache = NSCache<NSString, AnyObject>()
        cache.countLimit = 40
        cache.totalCostLimit = 8_000_000
        return cache
    }()

    static func key(sha: String, view: String, theme: WonderTheme? = nil, typography: WonderTypography? = nil) -> String {
        "\(sha)|\(view)|\(theme?.cacheKey ?? "-")|\(typography?.cacheKey ?? "-")"
    }
    func value<T: AnyObject>(_ type: T.Type, for key: String) -> T? { cache.object(forKey: key as NSString) as? T }
    func store(_ value: AnyObject, for key: String, cost: Int) {
        cache.setObject(value, forKey: key as NSString, cost: cost)
    }
}

extension UIColor {
    convenience init(hex: UInt32) {
        self.init(red: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
                  blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
    }
}

/// The Preview | Source switch shared by Markdown and JSON files. The choice is
/// remembered per device and kind.
struct TextViewerModeBar: View {
    let kind: TextViewerKind
    @Binding var mode: TextViewerMode
    var onCopy: (() -> Void)?
    @State private var copied = false

    var body: some View {
        HStack(spacing: 8) {
            if onCopy != nil { Color.clear.frame(width: 44, height: 1).accessibilityHidden(true) }
            Spacer(minLength: 0)
            Picker("View", selection: Binding(get: { mode }, set: { newValue in
                mode = newValue
                TextViewerPreference.remember(newValue, for: kind)
            })) {
                Text(kind.titles.rendered).tag(TextViewerMode.rendered)
                Text(kind.titles.source).tag(TextViewerMode.source)
            }
            .pickerStyle(.segmented)
            .frame(maxWidth: 260)
            .accessibilityIdentifier("workspace-viewer-mode")
            Spacer(minLength: 0)
            if let onCopy {
                Button {
                    onCopy()
                    copied = true
                    Task { try? await Task.sleep(for: .seconds(1.5)); copied = false }
                } label: {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        .frame(minWidth: 44, minHeight: 44)
                }
                .accessibilityLabel(copied ? "Copied" : "Copy")
                .accessibilityIdentifier("workspace-viewer-copy")
            }
        }
        .padding(.horizontal, 12)
        .frame(minHeight: 44)
    }
}

/// Rendered Markdown with the message renderer. The text is cut into chunks of a
/// few KB off the main thread and shown in a lazy stack, so each parse is small
/// and only visible chunks are laid out. Past a size limit the rest waits behind
/// "Show all"; the Source view is always the whole file.
/// What a Markdown Preview needs to show a project file's local images and
/// open its relative links. Absent for a file that is not in a project root,
/// such as an attachment.
struct MarkdownReferenceContext {
    /// The file's root-relative path, which relative references resolve against.
    let documentPath: String
    /// Loads a root-relative image, bounded and downsampled off the main thread.
    let loadImage: @MainActor (String) async throws -> UIImage
    /// Opens a root-relative file in the viewer; returns why it could not.
    let open: @MainActor (String) async -> String?
}

enum MarkdownImageFailure: Error {
    case missing, notImage, tooLarge, unreadable
    var message: String {
        switch self {
        case .missing: "Image not found in the project"
        case .notImage: "This file isn’t an image"
        case .tooLarge: "Image is too large to show here"
        case .unreadable: "Image can’t be shown"
        }
    }
}

struct MarkdownFileView: View {
    let text: String
    let sha: String
    @Environment(\.wonderTypography) private var typography
    /// The app's link opener (Settings → Link opening), read above the
    /// override below so web links follow it and the override never calls itself.
    @Environment(\.openURL) private var openWebLink
    var references: MarkdownReferenceContext? = nil
    static let initialLimitBytes = 64 * 1024
    @State private var chunks: [[MarkdownPreviewPiece]]?
    @State private var chunkTexts: [String] = []
    @State private var showAll = false
    @State private var linkNotice: String?

    var body: some View {
        ScrollView {
            if let chunks {
                let visible = showAll ? chunks.count : MarkdownChunker.visibleCount(of: chunkTexts, limitBytes: Self.initialLimitBytes)
                LazyVStack(alignment: .leading, spacing: 12) {
                    ForEach(0..<visible, id: \.self) { index in
                        VStack(alignment: .leading, spacing: 12) {
                            ForEach(Array(chunks[index].enumerated()), id: \.offset) { _, piece in
                                switch piece {
                                case .markdown(let text):
                                    AgentMessageText(text: text, keepsRelativeLinks: true)
                                case .image(let alt, let source, let link):
                                    MarkdownLocalImage(alt: alt, source: source, link: link, references: references,
                                                       openLocal: openLocal)
                                }
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    if visible < chunks.count {
                        Button("Show all (\(ByteCountFormatter.string(fromByteCount: Int64(text.utf8.count), countStyle: .file)))") { showAll = true }
                            .buttonStyle(.bordered)
                            .frame(minHeight: 44)
                            .accessibilityIdentifier("workspace-markdown-show-all")
                    }
                }
                .padding(16)
                .frame(maxWidth: 760, alignment: .leading)
                .frame(maxWidth: .infinity)
            } else {
                ProgressView().padding(40).frame(maxWidth: .infinity)
            }
        }
        .font(typography.font(.body))
        .safeAreaInset(edge: .bottom) {
            if let linkNotice {
                Text(linkNotice).font(.footnote)
                    .padding(.horizontal, 14).padding(.vertical, 10)
                    .background(.regularMaterial, in: Capsule())
                    .padding(.bottom, 8)
                    .accessibilityIdentifier("workspace-markdown-link-notice")
                    .task(id: linkNotice) {
                        try? await Task.sleep(for: .seconds(4))
                        if !Task.isCancelled { self.linkNotice = nil }
                    }
            }
        }
        .environment(\.openURL, OpenURLAction { url in
            guard url.scheme == nil else { openWebLink(url); return .handled }
            openLocal(MarkdownReference.resolve(url.relativeString, documentPath: references?.documentPath ?? ""))
            return .handled
        })
        .accessibilityIdentifier("workspace-markdown-preview")
        .task(id: sha + (references?.documentPath ?? "")) {
            showAll = false
            let path = references?.documentPath ?? ""
            let key = TextViewerCache.key(sha: sha, view: "markdown-pieces:" + path)
            if let cached = TextViewerCache.shared.value(ChunkBox.self, for: key) {
                chunkTexts = cached.texts; chunks = cached.chunks; return
            }
            chunks = nil
            let source = text
            let result = await Task.detached(priority: .userInitiated) {
                let texts = MarkdownChunker.chunks(source)
                return ChunkBox(texts: texts, chunks: texts.map { MarkdownPreviewPiece.pieces($0, documentPath: path) })
            }.value
            guard !Task.isCancelled else { return }
            TextViewerCache.shared.store(result, for: key, cost: source.utf8.count * 3)
            chunkTexts = result.texts; chunks = result.chunks
        }
    }

    private func openLocal(_ reference: MarkdownReference) {
        switch reference {
        case .local(let path):
            guard let references else { linkNotice = "Open this file from the project’s Files to follow its links."; return }
            Task { linkNotice = await references.open(path) }
        case .web(let url): openWebLink(url)
        case .anchor: break
        case .blocked: linkNotice = "This link points outside the project."
        }
    }

    private final class ChunkBox: @unchecked Sendable {
        let texts: [String]
        let chunks: [[MarkdownPreviewPiece]]
        init(texts: [String], chunks: [[MarkdownPreviewPiece]]) { self.texts = texts; self.chunks = chunks }
    }
}

/// An image a project README shows from its own folder. It loads through the
/// workspace file path; a missing, blocked or unreadable image says so in place.
private struct MarkdownLocalImage: View {
    let alt: String
    let source: MarkdownReference
    let link: MarkdownReference?
    let references: MarkdownReferenceContext?
    let openLocal: (MarkdownReference) -> Void
    @Environment(\.wonderTheme) private var theme
    @State private var loaded: (path: String, result: Result<UIImage, MarkdownImageFailure>)?

    var body: some View {
        Group {
            if case .local(let path) = source, references != nil {
                if let loaded, loaded.path == path {
                    switch loaded.result {
                    case .success(let image): imageView(image)
                    case .failure(let failure): placeholder(failure.message, detail: path, symbol: "photo.badge.exclamationmark")
                    }
                } else {
                    ProgressView().frame(maxWidth: .infinity, minHeight: 120)
                        .accessibilityLabel("Loading image \(alt)")
                }
            } else if case .local(let path) = source {
                placeholder("Open this file from the project’s Files to see its images", detail: path, symbol: "photo")
            } else {
                placeholder("Image outside this project isn’t shown", detail: nil, symbol: "lock")
            }
        }
        .accessibilityIdentifier("workspace-markdown-image")
        .task(id: localPath) {
            guard let localPath, let references else { return }
            await load(localPath, references)
        }
    }
    private var localPath: String? { if case .local(let path) = source { path } else { nil } }

    @ViewBuilder private func imageView(_ image: UIImage) -> some View {
        let view = Image(uiImage: image).resizable().scaledToFit()
            .frame(maxWidth: min(image.size.width, 728), maxHeight: 520, alignment: .leading)
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .accessibilityLabel(alt.isEmpty ? "Image" : alt)
        if let link, link != .anchor {
            Button { openLocal(link) } label: { view }.buttonStyle(.plain)
        } else {
            view.accessibilityAddTraits(.isImage)
        }
    }

    private func placeholder(_ message: String, detail: String?, symbol: String) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: symbol).foregroundStyle(theme.secondaryText).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(alt.isEmpty ? message : "\(alt): \(message)").font(.subheadline)
                if let detail { Text(detail).font(.caption.monospaced()).foregroundStyle(theme.secondaryText).lineLimit(2) }
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(theme.codeBackground, in: RoundedRectangle(cornerRadius: 8))
        .accessibilityElement(children: .combine)
    }

    private func load(_ path: String, _ references: MarkdownReferenceContext) async {
        if loaded?.path == path { return }
        let result: Result<UIImage, MarkdownImageFailure>
        do { result = .success(try await references.loadImage(path)) }
        catch is CancellationError { return }
        catch let failure as MarkdownImageFailure { result = .failure(failure) }
        catch { result = .failure(.unreadable) }
        guard !Task.isCancelled else { return }
        loaded = (path, result)
    }
}

/// Source and raw text is monospaced and exact: long tokens break at any character
/// and never take a hyphen. Layout only; the characters are unchanged.
enum SourceTextLayout {
    static func paragraph() -> NSParagraphStyle {
        let style = NSMutableParagraphStyle()
        style.lineBreakMode = .byCharWrapping
        style.hyphenationFactor = 0
        return style
    }

    static func apply(to storage: NSTextStorage) {
        guard storage.length > 0 else { return }
        storage.addAttribute(.paragraphStyle, value: paragraph(), range: NSRange(location: 0, length: storage.length))
    }
}

/// Read-only, selectable, scrollable attributed text. UITextView lays out only
/// what is on screen, so a long formatted document stays cheap. Updates are
/// idempotent: the text is replaced only when `identity` changes.
struct ReadOnlyAttributedTextView: UIViewRepresentable {
    let text: NSAttributedString
    let identity: String

    func makeCoordinator() -> Coordinator { Coordinator() }
    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        view.isEditable = false
        view.isSelectable = true
        view.isScrollEnabled = true
        view.backgroundColor = .clear
        view.textContainerInset = UIEdgeInsets(top: 16, left: 16, bottom: 16, right: 16)
        view.textContainer.lineFragmentPadding = 0
        view.attributedText = text
        SourceTextLayout.apply(to: view.textStorage)
        context.coordinator.identity = identity
        return view
    }
    func updateUIView(_ view: UITextView, context: Context) {
        guard context.coordinator.identity != identity else { return }
        context.coordinator.identity = identity
        view.attributedText = text
        SourceTextLayout.apply(to: view.textStorage)
        view.setContentOffset(CGPoint(x: 0, y: -view.adjustedContentInset.top), animated: false)
    }
    final class Coordinator { var identity = "" }
}

private final class PreparedJSON: @unchecked Sendable {
    let shown: String
    let attributed: NSAttributedString
    let notice: String?
    init(shown: String, attributed: NSAttributedString, notice: String?) {
        self.shown = shown; self.attributed = attributed; self.notice = notice
    }

    /// Pure: formats and colours off the main thread. Files cut at the preview
    /// limit cannot be valid JSON, so they are shown as written.
    static func make(text: String, truncated: Bool, palette: ThemePalette, typography: WonderTypography) -> PreparedJSON {
        let font = typography.codeUIFont(.body, size: 15)
        func plain(_ notice: String) -> PreparedJSON {
            PreparedJSON(shown: text, attributed: attributed(lines: nil, plain: text, palette: palette, font: font), notice: notice)
        }
        if truncated { return plain("This file is too large to format here, so the first part is shown as written.") }
        switch JSONFormatter.format(text) {
        case .formatted(let pretty):
            return PreparedJSON(shown: pretty, attributed: attributed(lines: JSONFormatter.highlightedLines(pretty),
                                                                      plain: pretty, palette: palette, font: font), notice: nil)
        case .invalid(let error):
            return plain("This file isn’t valid JSON; showing it as-is. \(error.summary).")
        case .tooLarge:
            return plain("This file is too large to format here; showing it as-is.")
        }
    }

    /// Coloured when `lines` are given, otherwise `plain` in the theme's text colour.
    private static func attributed(lines: [[SyntaxSpan]]?, plain: String, palette: ThemePalette, font: UIFont) -> NSAttributedString {
        let base = UIColor(hex: palette.primaryText)
        guard let lines else {
            return NSAttributedString(string: plain, attributes: [.font: font, .foregroundColor: base])
        }
        let output = NSMutableAttributedString()
        for (index, spans) in lines.enumerated() {
            if index > 0 { output.append(NSAttributedString(string: "\n", attributes: [.font: font])) }
            for span in spans {
                let color = span.role.map { UIColor(hex: palette.color(for: $0)) } ?? base
                output.append(NSAttributedString(string: span.text, attributes: [.font: font, .foregroundColor: color]))
            }
        }
        return output
    }
}

private struct ViewerNotice: View {
    let text: String
    let id: String
    @Environment(\.wonderTheme) private var theme
    var body: some View {
        Text(text)
            .font(.footnote)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12).padding(.vertical, 6)
            .background(theme.surface)
            .accessibilityIdentifier(id)
    }
}

/// Pretty-printed, syntax-coloured JSON. Key order and number text are exactly
/// as written. Formatting runs off the main thread and is cached per file,
/// view and theme; `onShownText` reports what Copy should put on the pasteboard.
struct JSONFileView: View {
    let text: String
    let sha: String
    let truncated: Bool
    let onShownText: (String) -> Void
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    @State private var prepared: PreparedJSON?

    var body: some View {
        let key = TextViewerCache.key(sha: sha, view: "json-formatted", theme: theme, typography: typography)
        VStack(spacing: 0) {
            if let notice = prepared?.notice { ViewerNotice(text: notice, id: "workspace-json-notice") }
            if let prepared {
                ReadOnlyAttributedTextView(text: prepared.attributed, identity: key)
                    .accessibilityIdentifier("workspace-json-formatted")
            } else {
                ProgressView("Formatting…").frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .task(id: key) {
            if let cached = TextViewerCache.shared.value(PreparedJSON.self, for: key) {
                prepared = cached; onShownText(cached.shown); return
            }
            prepared = nil
            let (source, truncated, palette, typography) = (text, truncated, theme.palette, typography)
            let result = await Task.detached(priority: .userInitiated) {
                PreparedJSON.make(text: source, truncated: truncated, palette: palette, typography: typography)
            }.value
            guard !Task.isCancelled else { return }
            TextViewerCache.shared.store(result, for: key, cost: source.utf8.count * 6)
            prepared = result
            onShownText(result.shown)
        }
    }
}

private final class PreparedJSONL {
    let document: JSONLDocument
    init(_ document: JSONLDocument) { self.document = document }
}

private final class PreparedRecordText {
    let value: AttributedString
    init(_ value: AttributedString) { self.value = value }
}

/// A formatted JSON Lines record's text, coloured off the main thread.
private struct RecordText: View {
    let text: String
    let sha: String
    let line: Int
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    @State private var colored: (key: String, text: AttributedString)?

    var body: some View {
        let key = TextViewerCache.key(sha: sha, view: "jsonl-record-\(line)", theme: theme)
        let shown = TextViewerCache.shared.value(PreparedRecordText.self, for: key)?.value
            ?? (colored?.key == key ? colored?.text : nil)
        Text(shown ?? AttributedString(text))
            .font(typography.codeFont(.footnote))
            .foregroundStyle(theme.primaryText)
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
            .task(id: key) {
                guard shown == nil, let lines = JSONFormatter.highlightedLines(text, maxBytes: 64 * 1024) else { return }
                let palette = theme.palette
                let result = await Task.detached(priority: .userInitiated) {
                    CodeHighlightCache.attributed(lines: lines, palette: palette)
                }.value
                guard !Task.isCancelled else { return }
                TextViewerCache.shared.store(PreparedRecordText(result), for: key, cost: text.utf8.count * 6)
                colored = (key, result)
            }
    }
}

/// A JSON Lines file: one collapsible record per line, with its line number.
/// Invalid and oversize lines stay visible as written and are marked. Records
/// live in a lazy stack and are formatted off the main thread.
struct JSONLFileView: View {
    let text: String
    let sha: String
    let truncated: Bool
    let onShownText: (String) -> Void
    @Environment(\.wonderTypography) private var typography
    @State private var document: JSONLDocument?
    @State private var expanded: Set<Int> = []

    var body: some View {
        Group { content }.task(id: sha) { await load() }
    }

    @ViewBuilder private var content: some View {
        if let document {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    Text(document.records.count == 1 ? "1 record" : "\(document.records.count) records")
                        .font(.footnote).foregroundStyle(.secondary)
                        .padding(.horizontal, 16).padding(.vertical, 8)
                    ForEach(document.records) { record in
                        row(record)
                        Divider()
                    }
                    if document.omittedLines > 0 {
                        Text("\(document.omittedLines) more lines aren’t shown here. Switch to Raw to read them.")
                            .font(.footnote).foregroundStyle(.secondary)
                            .padding(16)
                            .accessibilityIdentifier("workspace-jsonl-omitted")
                    }
                }
            }
            .accessibilityIdentifier("workspace-jsonl-records")
        } else {
            ProgressView("Formatting…").frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    @ViewBuilder private func row(_ record: JSONLRecord) -> some View {
        if let formatted = record.formatted {
            let isOpen = expanded.contains(record.line)
            VStack(alignment: .leading, spacing: 6) {
                Button {
                    if isOpen { expanded.remove(record.line) } else { expanded.insert(record.line) }
                } label: {
                    HStack(spacing: 8) {
                        Image(systemName: "chevron.right").font(.caption.weight(.semibold))
                            .rotationEffect(.degrees(isOpen ? 90 : 0)).foregroundStyle(.secondary)
                        Text("Line \(record.line)").font(.caption.weight(.semibold).monospacedDigit())
                        if !isOpen {
                            Text(record.raw).font(typography.codeFont(.caption))
                                .foregroundStyle(.secondary).lineLimit(1)
                        }
                        Spacer(minLength: 0)
                    }
                    .frame(minHeight: 44).contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Line \(record.line)")
                .accessibilityValue(isOpen ? "Expanded" : "Collapsed")
                .accessibilityIdentifier("jsonl-record:\(record.line)")
                if isOpen { RecordText(text: formatted, sha: sha, line: record.line).padding(.bottom, 10) }
            }
            .padding(.horizontal, 16)
        } else {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 8) {
                    Text("Line \(record.line)").font(.caption.weight(.semibold).monospacedDigit())
                    Text(record.isOversize ? "Too large to format" : "Not valid JSON")
                        .font(.caption).foregroundStyle(.orange)
                }
                Text(record.raw).font(typography.codeFont(.footnote)).textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                if let error = record.error {
                    Text(error.summary).font(.caption).foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 16).padding(.vertical, 10)
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("jsonl-record:\(record.line)")
        }
    }

    private func load() async {
        let key = TextViewerCache.key(sha: sha, view: "jsonl-records")
        if let cached = TextViewerCache.shared.value(PreparedJSONL.self, for: key) {
            apply(cached.document); return
        }
        let (source, truncated) = (text, truncated)
        let result = await Task.detached(priority: .userInitiated) {
            JSONLDocument(parsing: source, isTruncated: truncated)
        }.value
        guard !Task.isCancelled else { return }
        TextViewerCache.shared.store(PreparedJSONL(result), for: key, cost: source.utf8.count * 4)
        apply(result)
    }
    private func apply(_ result: JSONLDocument) {
        document = result
        expanded = result.records.first(where: { $0.formatted != nil }).map { [$0.line] } ?? []
        onShownText(result.shownText)
    }
}

/// Syntax roles per (file hash, language), so reopening a file or changing the
/// theme does not tokenize again. Roles and ranges only; colours are chosen
/// when they are applied. Bounded in bytes, emptied on a memory warning, and
/// separate from the file's own data.
final class SyntaxRunCache: @unchecked Sendable {
    static let shared = SyntaxRunCache()
    private final class Box { let runs: [SyntaxRun]; init(_ runs: [SyntaxRun]) { self.runs = runs } }
    private let cache: NSCache<NSString, Box> = {
        let cache = NSCache<NSString, Box>()
        cache.countLimit = 24
        cache.totalCostLimit = 6_000_000
        return cache
    }()
    private var memoryWarning: NSObjectProtocol?

    init() {
        memoryWarning = NotificationCenter.default.addObserver(
            forName: UIApplication.didReceiveMemoryWarningNotification, object: nil, queue: nil
        ) { [weak self] _ in self?.removeAll() }
    }

    static func key(sha: String, language: String) -> String { "\(sha)|\(language)" }
    func runs(for key: String) -> [SyntaxRun]? { cache.object(forKey: key as NSString)?.runs }
    func store(_ runs: [SyntaxRun], for key: String) {
        cache.setObject(Box(runs), forKey: key as NSString, cost: runs.count * MemoryLayout<SyntaxRun>.stride)
    }
    func removeAll() { cache.removeAllObjects() }
}

/// Colours a read-only source text view by syntax, chosen by the file's name.
///
/// Tokenizing runs off the main actor in chunks of whole lines that carry the
/// tokenizer's state. The first chunk reaches past the lines on screen and
/// those are coloured first; the rest follows chunk by chunk. Each batch is
/// one `beginEditing`/`endEditing` of foreground colours only, so the text,
/// selection and scroll position are untouched. Roles are kept, so a theme
/// change repaints without tokenizing again. Owned by a text view's
/// coordinator; `cancel()` when the view goes away.
@MainActor final class SourceTextColorist {
    /// UTF-16 units tokenized per background chunk after the first.
    static let chunkLength = 16 * 1024
    /// Text around the visible lines coloured with them.
    static let viewportMargin = 4 * 1024
    /// Runs applied per main-thread batch when painting known roles.
    static let batchRuns = 3_000

    private var tokenizing: Task<Void, Never>?
    private var painting: Task<Void, Never>?
    private var source: String?
    private var runs: [SyntaxRun] = []
    private var palette: ThemePalette?
    private var base: UIColor?
    private var colors: [SyntaxRole: UIColor] = [:]

    /// Starts colouring new text, or repaints for a new theme. Idempotent:
    /// call it on every update, after the view's text and base colour are set.
    /// `textReplaced` says the view's text was just assigned, which resets
    /// its colours.
    func update(_ view: UITextView, text: String, fileName: String?, revision: String,
                palette: ThemePalette, base: UIColor, textReplaced: Bool) {
        guard let fileName, let language = SourceSyntax.language(fileName: fileName) else {
            cancel(); source = nil; return
        }
        let key = SyntaxRunCache.key(sha: revision, language: language)
        let recolour = palette != self.palette || base != self.base
        if recolour {
            self.palette = palette; self.base = base
            colors = Dictionary(uniqueKeysWithValues: Self.roles.map { ($0, UIColor(hex: palette.color(for: $0))) })
        }
        if key != source {
            cancel()
            source = key; runs = []
            start(view, text: text, fileName: fileName, key: key)
        } else if recolour || textReplaced {
            repaint(view)
        }
    }

    func cancel() {
        tokenizing?.cancel(); tokenizing = nil
        painting?.cancel(); painting = nil
    }

    private static let roles: [SyntaxRole] = [.keyword, .string, .comment, .number, .type, .attribute, .function, .punctuation]

    private func start(_ view: UITextView, text: String, fileName: String, key: String) {
        if let cached = SyntaxRunCache.shared.runs(for: key) {
            runs = cached
            paint(cached, in: view, resetBase: false, operation: "syntax.cached")
            return
        }
        let utf16Length = view.textStorage.length
        tokenizing = Task { [weak self, weak view] in
            let started = ProcessInfo.processInfo.systemUptime
            var timing = SyntaxTiming(utf16Length: utf16Length)
            let made = await Task.detached(priority: .userInitiated) { SyntaxChunker(text: text, fileName: fileName) }.value
            guard var chunker = made, !Task.isCancelled, let self, let view else { return }
            var length = max(NSMaxRange(Self.visibleRange(in: view)) + Self.viewportMargin, Self.chunkLength)
            while !chunker.isFinished {
                let current = chunker, wanted = length
                let (next, chunk, tokenizeMs) = await Task.detached(priority: .userInitiated) {
                    var chunker = current
                    let start = ProcessInfo.processInfo.systemUptime
                    let chunk = chunker.next(utf16Length: wanted)
                    return (chunker, chunk, (ProcessInfo.processInfo.systemUptime - start) * 1000)
                }.value
                guard !Task.isCancelled, self.source == key else { return }
                chunker = next
                timing.tokenizeMs += tokenizeMs
                self.runs += chunk.runs
                if timing.batches == 0 {
                    // The first chunk reaches past the visible lines: those first.
                    let rest = self.paintVisible(chunk.runs, in: view, resetBase: false, timing: &timing)
                    timing.reportViewport(started: started)
                    await self.paintRest(rest, in: view, timing: &timing)
                } else {
                    self.apply(chunk.runs[...], to: view, timing: &timing)
                }
                guard !Task.isCancelled else { return }
                length = Self.chunkLength
            }
            SyntaxRunCache.shared.store(self.runs, for: key)
            timing.report("syntax.complete", started: started, runs: self.runs.count)
        }
    }

    /// Repaints known roles after a theme change or a text reset; tokenizing
    /// that is still running continues with the new colours.
    private func repaint(_ view: UITextView) {
        paint(runs, in: view, resetBase: true, operation: "syntax.repaint")
    }

    /// Paints known runs: the visible lines now, in this update, so no frame
    /// shows them uncoloured; the rest in batches after.
    private func paint(_ runs: [SyntaxRun], in view: UITextView, resetBase: Bool, operation: String) {
        painting?.cancel()
        let started = ProcessInfo.processInfo.systemUptime
        var timing = SyntaxTiming(utf16Length: view.textStorage.length)
        let rest = paintVisible(runs, in: view, resetBase: resetBase, timing: &timing)
        timing.reportViewport(started: started)
        let visibleTiming = timing
        painting = Task { [weak self, weak view] in
            guard let self, let view else { return }
            var timing = visibleTiming
            await self.paintRest(rest, in: view, timing: &timing)
            guard !Task.isCancelled else { return }
            timing.report(operation, started: started, runs: runs.count)
        }
    }

    /// Applies the runs around the visible lines and returns those before and after.
    private func paintVisible(_ runs: [SyntaxRun], in view: UITextView, resetBase: Bool,
                              timing: inout SyntaxTiming) -> [ArraySlice<SyntaxRun>] {
        let visible = Self.visibleRange(in: view)
        let window = NSRange(location: max(0, visible.location - Self.viewportMargin),
                             length: visible.length + 2 * Self.viewportMargin)
        let first = runs.firstIndex { NSMaxRange($0.range) > window.location } ?? runs.endIndex
        let last = runs[first...].firstIndex { $0.range.location >= NSMaxRange(window) } ?? runs.endIndex
        apply(runs[first..<last], to: view, resetBase: resetBase, timing: &timing)
        return [runs[last...], runs[..<first]]
    }

    /// Applies runs in bounded batches, leaving the main thread free between them.
    private func paintRest(_ slices: [ArraySlice<SyntaxRun>], in view: UITextView, timing: inout SyntaxTiming) async {
        for slice in slices {
            var index = slice.startIndex
            while index < slice.endIndex {
                try? await Task.sleep(for: .milliseconds(1))
                guard !Task.isCancelled else { return }
                let end = min(index + Self.batchRuns, slice.endIndex)
                apply(slice[index..<end], to: view, timing: &timing)
                index = end
            }
        }
    }

    private func apply(_ runs: ArraySlice<SyntaxRun>, to view: UITextView, resetBase: Bool = false,
                       timing: inout SyntaxTiming) {
        let storage = view.textStorage
        guard !runs.isEmpty || resetBase, storage.length > 0 else { return }
        let start = ProcessInfo.processInfo.systemUptime
        storage.beginEditing()
        if resetBase, let base {
            storage.addAttribute(.foregroundColor, value: base, range: NSRange(location: 0, length: storage.length))
        }
        for run in runs where NSMaxRange(run.range) <= storage.length {
            if let color = colors[run.role] { storage.addAttribute(.foregroundColor, value: color, range: run.range) }
        }
        storage.endEditing()
        timing.add(batchMs: (ProcessInfo.processInfo.systemUptime - start) * 1000, view: view)
    }

    /// The UTF-16 range on screen; the start of the text before the first layout.
    static func visibleRange(in view: UITextView) -> NSRange {
        let bounds = view.bounds
        guard bounds.height > 0,
              let top = view.closestPosition(to: CGPoint(x: bounds.minX, y: bounds.minY)),
              let bottom = view.closestPosition(to: CGPoint(x: bounds.maxX, y: bounds.maxY)) else {
            return NSRange(location: 0, length: min(view.textStorage.length, 8 * 1024))
        }
        let start = view.offset(from: view.beginningOfDocument, to: top)
        let end = view.offset(from: view.beginningOfDocument, to: bottom)
        return NSRange(location: min(start, end), length: abs(end - start))
    }
}

/// Main-thread and background time for one colouring pass, recorded in the
/// Diagnostics journal (durations, counts and sizes; no text or file names).
private struct SyntaxTiming {
    let utf16Length: Int
    var tokenizeMs = 0.0
    var mainMs = 0.0
    var maxBatchMs = 0.0
    var layoutMs = 0.0
    var batches = 0
    var viewportMs: Double?

    init(utf16Length: Int) { self.utf16Length = utf16Length }

    @MainActor mutating func add(batchMs: Double, view: UITextView) {
        mainMs += batchMs; maxBatchMs = max(maxBatchMs, batchMs); batches += 1
        #if WONDER_DIAGNOSTICS
        // Optional: time the layout the edit causes, which UIKit would
        // otherwise do at the next display pass.
        if Self.measuresLayout {
            let start = ProcessInfo.processInfo.systemUptime
            view.layoutIfNeeded()
            layoutMs += (ProcessInfo.processInfo.systemUptime - start) * 1000
        }
        #endif
    }

    mutating func reportViewport(started: TimeInterval) {
        guard viewportMs == nil else { return }
        viewportMs = (ProcessInfo.processInfo.systemUptime - started) * 1000
    }

    func report(_ operation: String, started: TimeInterval, runs: Int) {
        #if WONDER_DIAGNOSTICS
        let totalMs = (ProcessInfo.processInfo.systemUptime - started) * 1000
        let metrics = ["viewportMs": viewportMs ?? 0, "tokenizeMs": tokenizeMs, "mainMs": mainMs,
                       "maxBatchMs": maxBatchMs, "layoutMs": layoutMs, "batches": Double(batches)]
        DiagnosticJournal.shared.record(DiagnosticEvent(operation: operation, durationMs: totalMs, count: UInt64(runs),
                                                        bytes: UInt64(utf16Length * 2), metrics: metrics))
        Self.log.log("""
            \(operation, privacy: .public) utf16=\(utf16Length) runs=\(runs) viewport=\(viewportMs ?? 0, format: .fixed(precision: 2))ms \
            total=\(totalMs, format: .fixed(precision: 2))ms tokenize=\(tokenizeMs, format: .fixed(precision: 2))ms \
            main=\(mainMs, format: .fixed(precision: 2))ms maxBatch=\(maxBatchMs, format: .fixed(precision: 2))ms \
            layout=\(layoutMs, format: .fixed(precision: 2))ms batches=\(batches)
            """)
        #endif
    }

    #if WONDER_DIAGNOSTICS
    static let log = Logger(subsystem: "com.swaymun.wonder", category: "syntax")
    static let measuresLayout = ProcessInfo.processInfo.arguments.contains("-diagnostics-syntax-layout-timing")
    #endif
}
