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

struct WorkspaceMediaPreview: View {
    @ObservedObject var selection: WorkspaceMediaSelection
    var onClose: (() -> Void)? = nil
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack { // theme-exempt: shown inside WorkspaceBrowser over its host's page
            Group {
                if let failure = selection.failure {
                    ContentUnavailableView("Playback unavailable", systemImage: "play.slash",
                                           description: Text(failure))
                        .accessibilityIdentifier("workspace-media-unavailable")
                } else if let player = selection.player {
                    VideoPlayer(player: player)
                        .overlay(alignment: .bottom) {
                            if selection.blocked {
                                Text("Finish dictation or voice to play this.")
                                    .font(.caption).padding(8)
                                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
                                    .padding(.bottom, 56)
                                    .accessibilityIdentifier("workspace-media-blocked")
                            }
                        }
                        .accessibilityIdentifier("workspace-media-player")
                } else {
                    ProgressView("Loading media…")
                        .accessibilityIdentifier("workspace-media-loading")
                }
            }
            .navigationTitle(onClose == nil ? selection.name : "")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar(onClose == nil ? .visible : .hidden, for: .navigationBar)
            .toolbar {
                if onClose == nil {
                    Button("Done") { dismiss() }
                        .accessibilityIdentifier("workspace-media-close")
                }
            }
            .onAppear { selection.start() }
        }
    }
}

struct WorkspaceRootMenuButton: View {
    let root: WorkspaceRoot
    let action: (WorkspaceRoot) -> Void

    var body: some View {
        Button(HomeFolder.abbreviated(root.path)) { action(root) }
            .accessibilityIdentifier("workspace-root:\(root.id)")
    }
}

struct WorkspaceImagePreview: View {
    @Environment(\.wonderTheme) private var theme
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let item: WorkspacePreviewSelection
    let sketchUnavailableReason: String?
    @ObservedObject var revision: WorkspaceRevisionState
    let refresh: WorkspaceRevisionRead?
    let onRevision: ((String) async -> Void)?
    let onClose: () -> Void
    let attachSketch: (Data) async -> SketchAttachmentResult
    @State private var showingSketch = false
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        PhotoViewer(model: model, chat: chat, name: item.name, mimeType: item.mimeType,
                    sourceID: item.id, localData: revision.currentData, file: nil, revision: revision.currentSha256,
                    onClose: onClose, showsHeader: false, accessory: AnyView(sketchButton))
            .overlay(alignment: .top) {
                if let refreshFailure = revision.refreshFailure {
                    Text(refreshFailure).font(.footnote)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(10)
                        .foregroundStyle(.primary)
                        .background(theme.surface, in: RoundedRectangle(cornerRadius: 10))
                        .padding(.horizontal, 16).padding(.top, 8)
                        .accessibilityIdentifier("workspace-image-refresh-error")
                }
            }
            .sheet(isPresented: $showingSketch) {
                SketchSheet(backgroundData: revision.currentData,
                            contextToken: model.cameraContextID.uuidString + ":" + model.assignmentScope + ":" + chat.id,
                            attach: attachSketch)
            }
            .onChange(of: showingSketch, initial: true) { _, editing in
                revision.holdsRevision = editing
                if !editing { Task { await revision.showOfferedRevision(mimeType: nil) } }
            }
            .task(id: scenePhase == .active) {
                guard scenePhase == .active else { return }
                await revision.watch(mimeType: nil, refresh: refresh, onRevision: onRevision,
                    failureMessage: "Can't reach your computer to update this image.")
            }
    }
}


extension WorkspaceImagePreview {
    fileprivate var sketchButton: some View {
        VStack(spacing: 6) {
            Button("Sketch", systemImage: "pencil.tip.crop.circle") { showingSketch = true }
                .labelStyle(.titleAndIcon)
                .font(.body.weight(.semibold))
                .buttonStyle(.wonderProminent)
                .buttonBorderShape(.capsule)
                .disabled(sketchUnavailableReason != nil)
                // An unavailable reason shows (and is read) as the text below.
                .accessibilityHint(sketchUnavailableReason == nil ? "Mark up this image and attach a PNG to your message." : "")
                .accessibilityIdentifier("workspace-sketch-open")
            if let sketchUnavailableReason {
                Text(sketchUnavailableReason).font(.footnote)
                    .accessibilityIdentifier("workspace-sketch-unavailable")
            }
        }
    }
}

/// Native selection preserves the exact highlighted characters, including
/// Unicode and line endings, for a revision-bound comment.
/// Read-only text whose own selection menu offers Comment; choosing it pins
/// the selection for the comment bar.
private struct SelectablePreviewText: UIViewRepresentable {
    let text: String
    /// Text changes only with the source SHA; a new revision replaces it in
    /// place so the reader keeps their scroll position.
    let revision: String
    @Binding var selectedRange: NSRange?
    let selectionResetID: UUID
    var onComment: (() -> Void)? = nil
    var onSelecting: ((Bool) -> Void)? = nil
    /// The name that picks the syntax colouring; nil leaves the text plain.
    var syntaxFileName: String? = nil

    func makeCoordinator() -> Coordinator {
        Coordinator(selectedRange: $selectedRange, selectionResetID: selectionResetID)
    }
    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        context.coordinator.isUpdating = true
        view.delegate = context.coordinator
        view.isEditable = false
        view.isSelectable = true
        view.isScrollEnabled = true
        view.backgroundColor = .clear
        let textColor = context.environment.wonderTheme.textUIColor
        view.textColor = textColor
        context.coordinator.appliedTextColor = textColor
        view.tintColor = .tintColor
        view.textContainerInset = UIEdgeInsets(top: 16, left: 16, bottom: 16, right: 16)
        view.textContainer.lineFragmentPadding = 0
        view.adjustsFontForContentSizeCategory = true
        let font = context.environment.wonderTypography.codeUIFont(.body, size: 15)
        view.font = font
        context.coordinator.appliedFont = font
        view.text = text
        SourceTextLayout.apply(to: view.textStorage)
        if let selectedRange { view.selectedRange = selectedRange }
        if let selectedRange, selectedRange.length > 0,
           NSMaxRange(selectedRange) <= view.textStorage.length {
            view.textStorage.addAttribute(.backgroundColor, value: UIColor.systemYellow.withAlphaComponent(0.35),
                                          range: selectedRange)
            context.coordinator.highlightedRange = selectedRange
        }
        context.coordinator.appliedRange = selectedRange
        context.coordinator.appliedRevision = revision
        context.coordinator.colorist.update(view, text: text, fileName: syntaxFileName, revision: revision,
                                            palette: context.environment.wonderTheme.palette, base: textColor,
                                            textReplaced: true)
        context.coordinator.onComment = onComment
        context.coordinator.onSelecting = onSelecting
        context.coordinator.isUpdating = false
        return view
    }
    func updateUIView(_ view: UITextView, context: Context) {
        context.coordinator.selectedRange = $selectedRange
        context.coordinator.onComment = onComment
        context.coordinator.onSelecting = onSelecting
        let reset = context.coordinator.selectionResetID != selectionResetID
        if reset { context.coordinator.selectionResetID = selectionResetID }
        context.coordinator.isUpdating = true
        // Text is immutable within one source SHA. Do not compare or assign
        // 128 KB on every note keystroke.
        var textReplaced = false
        if context.coordinator.appliedRevision != revision {
            context.coordinator.appliedRevision = revision
            textReplaced = true
            let offset = view.contentOffset
            view.text = text
            SourceTextLayout.apply(to: view.textStorage)
            context.coordinator.highlightedRange = nil
            view.layoutIfNeeded()
            let maxY = max(-view.adjustedContentInset.top,
                           view.contentSize.height - view.bounds.height + view.adjustedContentInset.bottom)
            view.contentOffset = CGPoint(x: offset.x, y: min(offset.y, maxY))
        }
        let font = context.environment.wonderTypography.codeUIFont(.body, size: 15)
        if context.coordinator.appliedFont != font {
            context.coordinator.appliedFont = font
            view.font = font
        }
        // A theme change recolours the text; the colourist repaints syntax
        // colours on top from the roles it already has.
        let textColor = context.environment.wonderTheme.textUIColor
        if context.coordinator.appliedTextColor != textColor {
            context.coordinator.appliedTextColor = textColor
            view.textColor = textColor
            textReplaced = true
        }
        context.coordinator.colorist.update(view, text: text, fileName: syntaxFileName, revision: revision,
                                            palette: context.environment.wonderTheme.palette, base: textColor,
                                            textReplaced: textReplaced)
        // The reader's own selection stays until a comment pins or clears one.
        if reset {
            if view.selectedRange.length > 0 {
                view.selectedRange = NSRange(location: view.selectedRange.location, length: 0)
            }
        } else if let selectedRange, context.coordinator.appliedRange != selectedRange,
                  view.selectedRange != selectedRange {
            view.selectedRange = selectedRange
        }
        context.coordinator.appliedRange = selectedRange
        let highlightedRange = selectedRange.flatMap { range in
            range.length > 0 && NSMaxRange(range) <= view.textStorage.length ? range : nil
        }
        if context.coordinator.highlightedRange != highlightedRange {
            if let previous = context.coordinator.highlightedRange {
                view.textStorage.removeAttribute(.backgroundColor, range: previous)
            }
            if let highlightedRange {
                view.textStorage.addAttribute(.backgroundColor, value: UIColor.systemYellow.withAlphaComponent(0.35),
                                              range: highlightedRange)
            }
            context.coordinator.highlightedRange = highlightedRange
        }
        context.coordinator.isUpdating = false
    }

    static func dismantleUIView(_ view: UITextView, coordinator: Coordinator) {
        coordinator.colorist.cancel()
    }

    final class Coordinator: NSObject, UITextViewDelegate {
        var selectedRange: Binding<NSRange?>
        var selectionResetID: UUID
        var appliedRange: NSRange?
        var highlightedRange: NSRange?
        var onComment: (() -> Void)?
        var onSelecting: ((Bool) -> Void)?
        var appliedRevision: String?
        let colorist = SourceTextColorist()
        var appliedTextColor: UIColor?
        var appliedFont: UIFont?
        var isUpdating = false
        private var selecting = false
        init(selectedRange: Binding<NSRange?>, selectionResetID: UUID) {
            self.selectedRange = selectedRange
            self.selectionResetID = selectionResetID
        }
        func textViewDidChangeSelection(_ textView: UITextView) {
            let active = textView.selectedRange.length > 0
            guard active != selecting else { return }
            selecting = active
            // Publish after the current UIKit callback, never from inside an update.
            DispatchQueue.main.async { [weak self] in self?.onSelecting?(active) }
        }
        func textView(_ textView: UITextView, editMenuForTextIn range: NSRange,
                      suggestedActions: [UIMenuElement]) -> UIMenu? {
            guard range.length > 0, onComment != nil else { return UIMenu(children: suggestedActions) }
            let comment = UIAction(title: "Comment", image: UIImage(systemName: "text.bubble")) { [weak self] _ in
                guard let self else { return }
                self.appliedRange = range
                self.selectedRange.wrappedValue = range
                self.onComment?()
            }
            return UIMenu(children: [comment] + suggestedActions)
        }
    }
}

struct WorkspaceDocumentPreview: View {
    let name: String
    let mimeType: String
    let pdfSession: PDFPreviewSession?
    let htmlSession: WorkspaceHTMLPreviewSession?
    let annotationContext: ArtifactPreviewContext?
    let annotationUnavailableReason: String?
    let onAnnotation: ((ArtifactAnnotation) throws -> Void)?
    let refresh: WorkspaceRevisionRead?
    let onRevision: ((String) async -> Void)?
    let onClose: (() -> Void)?
    let markdownReferences: MarkdownReferenceContext?
    /// Chosen once from the name and type; nil leaves the source plain.
    private let syntaxFileName: String?
    @ObservedObject var revision: WorkspaceRevisionState
    @ObservedObject var draft: WorkspaceTextAnnotationDraft
    init(name: String, mimeType: String, revision: WorkspaceRevisionState,
         draft: WorkspaceTextAnnotationDraft,
         pdfSession: PDFPreviewSession? = nil,
         htmlSession: WorkspaceHTMLPreviewSession? = nil,
         annotationContext: ArtifactPreviewContext? = nil,
         annotationUnavailableReason: String? = nil,
         onAnnotation: ((ArtifactAnnotation) throws -> Void)? = nil,
         refresh: WorkspaceRevisionRead? = nil,
         onRevision: ((String) async -> Void)? = nil,
         onClose: (() -> Void)? = nil,
         markdownReferences: MarkdownReferenceContext? = nil) {
        self.name = name; self.mimeType = mimeType; self.pdfSession = pdfSession
        self.markdownReferences = markdownReferences
        self.htmlSession = htmlSession
        self.revision = revision
        self.draft = draft
        self.annotationContext = annotationContext; self.annotationUnavailableReason = annotationUnavailableReason
        self.onAnnotation = onAnnotation
        self.refresh = refresh; self.onRevision = onRevision
        self.onClose = onClose
        let kind = Self.viewerKind(name: name, mimeType: mimeType)
        syntaxFileName = Self.syntaxFileName(name: name, mimeType: mimeType, kind: kind)
        _viewerMode = State(initialValue: kind.map { TextViewerPreference.mode(for: $0) } ?? .rendered)
    }
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    @State private var pdfComment: PDFTextComment?
    @State private var showingHTMLSource = false
    @State private var viewerMode: TextViewerMode
    /// What Copy puts on the pasteboard while a formatted JSON view is shown.
    @State private var shownCopyText: String?
    var body: some View {
        NavigationStack { // theme-exempt: shown inside WorkspaceBrowser over its host's page
            VStack(spacing: 0) {
                previewControls
                documentContent
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .clipped()
            }
            // The embedding browser owns the title. A hidden nested bar can
            // still publish its title into the folded display's window chrome.
            .navigationTitle(onClose == nil ? name : "").navigationBarTitleDisplayMode(.inline)
            .toolbar(onClose == nil ? .visible : .hidden, for: .navigationBar)
            .toolbar {
                if onClose == nil {
                    ToolbarItem(placement: .topBarTrailing) {
                        Button("Done") { dismiss() }
                            .accessibilityIdentifier("workspace-document-close")
                    }
                    if offersHTMLSource {
                        ToolbarItem(placement: .topBarLeading) { htmlSourceToggle }
                    }
                }
            }
            .safeAreaInset(edge: .bottom) {
                if canAnnotate, let selectedTextRange = draft.selectedRange, selectedTextRange.length > 0 {
                    VStack(alignment: .leading, spacing: 8) {
                        HStack(spacing: 10) {
                            Text(selectedExcerpt(for: selectedTextRange))
                                .font(.footnote).lineLimit(2)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .accessibilityIdentifier("annotation-selected-text")
                            Button("Cancel comment", systemImage: "xmark") {
                                draft.clearSelection(discardNote: true)
                            }
                            .labelStyle(.iconOnly)
                            .frame(minWidth: 44, minHeight: 44)
                            .accessibilityIdentifier("annotation-cancel")
                            if draft.editingSelection {
                                Button("Save comment", systemImage: "checkmark") { addAnnotation() }
                                    .labelStyle(.iconOnly)
                                    .frame(minWidth: 44, minHeight: 44)
                                    .disabled(!canStageAnnotation || draft.note.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || draft.note.utf8.count > 4096)
                                    .accessibilityIdentifier("annotation-add")
                            } else {
                                Button("Comment", systemImage: "text.bubble") { draft.editingSelection = true }
                                    .frame(minHeight: 44)
                                    .accessibilityIdentifier("annotation-comment")
                            }
                        }
                        if draft.editingSelection {
                            ZStack(alignment: .topLeading) {
                                WorkspaceAnnotationNoteField(draft: draft)
                                if draft.note.isEmpty {
                                    Text("Add a comment")
                                        .foregroundStyle(.secondary)
                                        .padding(.top, 8)
                                        .allowsHitTesting(false)
                                }
                            }
                            if let annotationUnavailableReason {
                                Text(annotationUnavailableReason)
                                    .font(.footnote).foregroundStyle(.secondary)
                                    .accessibilityIdentifier("annotation-comment-wait")
                            }
                        }
                        if draft.note.utf8.count > 4096 { Text("Keep the comment under 4,096 bytes.").font(.footnote).foregroundStyle(.red) }
                        if let error = draft.error { Text(error).font(.footnote).foregroundStyle(.red) }
                    }
                    .padding(.horizontal, 12).padding(.vertical, 8)
                    .frame(maxWidth: 600, alignment: .leading)
                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
                    .frame(maxWidth: .infinity)
                    .padding(.horizontal, 12).padding(.bottom, 4)
                }
            }
            .sheet(item: $pdfComment) { comment in
                if let annotationContext, let onAnnotation {
                    WorkspacePDFCommentSheet(comment: comment, source: comment.source ?? revision.currentData, context: annotationContext,
                                             onAnnotation: onAnnotation, initialNote: draft.note,
                                             annotationUnavailableReason: annotationUnavailableReason)
                }
            }
            .onChange(of: holdsRevision, initial: true) { _, holds in
                // A comment's byte offsets belong to the displayed revision.
                revision.holdsRevision = holds
                if !holds { Task { await revision.showOfferedRevision(mimeType: mimeType) } }
            }
            .task(id: scenePhase == .active) {
                guard scenePhase == .active else { return }
                await revision.watch(mimeType: mimeType, refresh: refresh, onRevision: onRevision,
                    failureMessage: "Can't reach your computer to update this file.")
            }
            .task(id: revision.currentSha256) { await revision.prepareTextIfNeeded(mimeType: mimeType) }
            .onChange(of: viewerMode) { _, _ in draft.clearSelection(discardNote: false) }
            .onChange(of: revision.currentSha256) { _, _ in shownCopyText = nil }
        }
    }

    // Controls own layout space rather than overlaying the scroll view's safe
    // area. Rendered Markdown and UIKit source can never scroll behind them.
    private var previewControls: some View {
        VStack(spacing: 0) {
            if let kind = viewerKind, revision.preparedText != nil {
                TextViewerModeBar(kind: kind, mode: $viewerMode,
                                  onCopy: kind == .markdown ? nil : { UIPasteboard.general.string = copyText })
            }
            if revision.previewTruncated, revision.preparedText != nil {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Showing the first 128 KB. Open the file on your computer to read the rest.")
                        .font(.footnote)
                        .accessibilityIdentifier("workspace-document-truncated")
                    // Only the start was read; a comment needs every byte.
                    if offersWholeRead {
                        Button(revision.loadingWhole ? "Loading the whole file…" : "Load the whole file to comment") {
                            Task { await revision.loadWhole(mimeType: mimeType, read: refresh, onRevision: onRevision) }
                        }
                        .font(.footnote.weight(.semibold))
                        .frame(minHeight: 44)
                        .disabled(revision.loadingWhole)
                        .accessibilityIdentifier("workspace-document-load-whole")
                    }
                    if let wholeFailure = revision.wholeFailure {
                        Text(wholeFailure).font(.footnote).foregroundStyle(.red)
                            .accessibilityIdentifier("workspace-document-load-whole-error")
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 12).padding(.vertical, 6)
                .background(theme.surface)
            }
            if mimeType == "application/pdf", annotationContext != nil,
               let annotationUnavailableReason {
                Text(annotationUnavailableReason)
                    .font(.footnote).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12).padding(.vertical, 6)
                    .accessibilityIdentifier("annotation-document-unavailable")
            }
            if onClose != nil && offersHTMLSource {
                HStack {
                    htmlSourceToggle
                    Spacer()
                }
                .padding(.horizontal, 12)
                .frame(minHeight: 44)
            }
            if let refreshFailure = revision.refreshFailure {
                Text(refreshFailure).font(.footnote).foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading).padding(10)
                    .accessibilityIdentifier("workspace-document-refresh-error")
            }

        }
        .background(theme.page)
    }

    private var documentContent: some View {
        Group {
            if mimeType == "application/pdf" {
                PDFPreview(data: revision.currentData, revision: revision.currentSha256,
                           session: pdfSession,
                           onComment: annotationContext != nil && onAnnotation != nil
                               ? { [shown = revision.currentData] selection in
                                   // Offered only while the page shows these bytes (PDFPreviewSession).
                                   var comment = PDFTextComment(selection)
                                   comment?.source = shown
                                   pdfComment = comment
                               } : nil)
                    .ignoresSafeArea(edges: .bottom)
                    .accessibilityIdentifier("workspace-pdf-preview")
            }
            else if revision.preparingText && revision.preparedText == nil { ProgressView("Preparing preview…") }
            else if mimeType == "text/html", !showingHTMLSource, let html = revision.preparedText {
                ConstrainedHTML(html: html, sourceID: revision.currentSha256, session: htmlSession)
            }
            else if showsRenderedViewer, let kind = viewerKind, let text = revision.preparedText {
                renderedViewer(kind, text: text)
            }
            else if let text = revision.preparedText, canAnnotate || syntaxFileName != nil {
                // Source code is coloured; it takes comments only where commenting is available.
                SelectablePreviewText(text: text, revision: revision.currentSha256,
                                      selectedRange: $draft.selectedRange,
                                      selectionResetID: draft.selectionResetID,
                                      onComment: canAnnotate ? { draft.editingSelection = true } : nil,
                                      onSelecting: canAnnotate ? { selecting in
                                          if draft.selectingText != selecting { draft.selectingText = selecting }
                                      } : nil,
                                      syntaxFileName: syntaxFileName)
                    .accessibilityIdentifier("annotation-selectable-text")
            } else if let text = revision.preparedText {
                // Raw source: a text view that breaks long tokens by character, never with a hyphen.
                ReadOnlyAttributedTextView(
                    text: NSAttributedString(string: text, attributes: [
                        .font: typography.codeUIFont(.body, size: 15),
                        .foregroundColor: theme.textUIColor]),
                    identity: "\(revision.currentSha256)|\(theme.cacheKey)|\(typography.cacheKey)")
            }
            else {
                ContentUnavailableView("Preview unavailable", systemImage: "doc",
                    description: Text(revision.previewTruncated
                        ? "This HTML file is too large to preview safely. Open it on your computer."
                        : "This file type does not have a preview in Cupola. Open it on your computer."))
                .accessibilityIdentifier("workspace-preview-unavailable")
            }
        }
    }

    private static func viewerKind(name: String, mimeType: String) -> TextViewerKind? {
        guard mimeType != "application/pdf", mimeType != "text/html" else { return nil }
        return TextViewerKind.detect(name: name, mimeType: mimeType)
    }
    private var viewerKind: TextViewerKind? { Self.viewerKind(name: name, mimeType: mimeType) }
    /// The representation on screen: formatted text when it is shown, else the file's text.
    private var copyText: String {
        (viewerMode == .rendered ? shownCopyText : nil) ?? revision.preparedText ?? ""
    }
    /// The rendered view replaces the source (and with it, commenting) until the
    /// reader switches to Source.
    private var showsRenderedViewer: Bool {
        viewerKind != nil && viewerMode == .rendered && revision.preparedText != nil
    }
    @ViewBuilder private func renderedViewer(_ kind: TextViewerKind, text: String) -> some View {
        switch kind {
        case .markdown: MarkdownFileView(text: text, sha: revision.currentSha256, references: markdownReferences)
        case .json:
            JSONFileView(text: text, sha: revision.currentSha256, truncated: revision.previewTruncated) { shownCopyText = $0 }
        case .jsonl:
            JSONLFileView(text: text, sha: revision.currentSha256, truncated: revision.previewTruncated) { shownCopyText = $0 }
        }
    }

    /// The name that picks the source colouring: the file's own, or for a
    /// Markdown, JSON or HTML file known by its type, that kind's.
    private static func syntaxFileName(name: String, mimeType: String, kind: TextViewerKind?) -> String? {
        if SourceSyntax.language(fileName: name) != nil { return name }
        switch kind {
        case .markdown: return "source.md"
        case .json, .jsonl: return "source.json"
        case nil: return mimeType == "text/html" ? "source.html" : nil
        }
    }

    private var holdsRevision: Bool {
        draft.selectedRange != nil || draft.selectingText || draft.editingSelection || pdfComment != nil
    }

    /// Any UTF-8 text file takes comments from its selection menu; rendered
    /// HTML does so from its source view.
    private var canAnnotate: Bool {
        !revision.preparingText && annotationContext != nil && onAnnotation != nil &&
        (canStageAnnotation || draft.selectedRange != nil) &&
        (mimeType != "text/html" || showingHTMLSource) && mimeType != "application/pdf" &&
        revision.preparedText != nil && !showsRenderedViewer && !revision.isPartial
    }
    /// A long file shown in part can be read whole to comment on it, when
    /// it fits on this device.
    private var offersWholeRead: Bool {
        revision.isPartial && annotationContext != nil && onAnnotation != nil && refresh != nil &&
        revision.totalBytes <= PairingAPI.workspacePreviewLimit
    }
    private var offersHTMLSource: Bool {
        mimeType == "text/html" && revision.preparedText != nil && annotationContext != nil && onAnnotation != nil
    }
    private var htmlSourceToggle: some View {
        Button(showingHTMLSource ? "Preview" : "Source",
               systemImage: showingHTMLSource ? "doc.richtext" : "chevron.left.forwardslash.chevron.right") {
            draft.clearSelection(discardNote: false)
            showingHTMLSource.toggle()
        }
        .accessibilityHint(showingHTMLSource ? "Show the rendered page" : "Show the HTML source to comment on it")
        .accessibilityIdentifier("workspace-html-source")
    }
    private var canStageAnnotation: Bool {
        annotationContext != nil && annotationUnavailableReason == nil
    }
    private func selectedExcerpt(for range: NSRange) -> String {
        guard let text = revision.preparedText, NSMaxRange(range) <= (text as NSString).length else { return "Selected text" }
        let selected = (text as NSString).substring(with: range)
        return String(selected.prefix(120)).replacingOccurrences(of: "\n", with: " ")
    }
    private func addAnnotation() {
        // A comment hashes its source as the file's revision: never a partial read.
        guard canStageAnnotation, !revision.isPartial, let annotationContext, let onAnnotation, let selectedTextRange = draft.selectedRange,
              let text = revision.preparedText, let range = Range(selectedTextRange, in: text) else { return }
        do {
            let start = text[..<range.lowerBound].utf8.count
            let end = start + text[range].utf8.count
            let annotation = try ArtifactAnnotation(projectId: annotationContext.projectID,
                conversationId: annotationContext.conversationID, rootId: annotationContext.rootID,
                path: annotationContext.path, source: revision.currentData, mimeType: mimeType,
                startByte: start, endByte: end, note: draft.note)
            try onAnnotation(annotation)
            draft.clearSelection(discardNote: true)
        } catch { draft.error = "The comment could not be saved. Reopen the file and try again." }
    }
}

struct ArtifactNoteEditor: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let attachment: ComposerAttachment
    let expectedScope: String
    let onReanchor: (ArtifactAnnotation) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var annotation: ArtifactAnnotation?
    @State private var note = ""
    @State private var failure: String?
    @State private var loading = true

    init(model: ConnectionModel, chat: ChatSummary, attachment: ComposerAttachment,
         onReanchor: @escaping (ArtifactAnnotation) -> Void) {
        self.model = model; self.chat = chat; self.attachment = attachment
        self.onReanchor = onReanchor
        expectedScope = model.assignmentScope
    }

    var body: some View {
        NavigationStack {
            WonderForm {
                if loading { ProgressView("Opening comment…") }
                else if let annotation {
                    Section("Commenting on") {
                        Text(annotation.path).lineLimit(2)
                        Text(anchorDescription(annotation.anchor)).foregroundStyle(.secondary)
                        if model.isAnnotationStale(attachment.id, chat: chat.id) {
                            Text("This file changed since you commented. Choose a new area or lines before sending.")
                                .foregroundStyle(.orange)
                                .accessibilityIdentifier("annotation-edit-stale")
                        }
                        if annotation.isDiff {
                            Text("Line comments cannot be added here. You can still remove this comment.")
                                .font(.footnote).foregroundStyle(.secondary)
                        } else if case .imageRegion = annotation.anchor {
                            Text("To mark up this image, remove this comment and open Sketch from the image in Files.")
                                .font(.footnote).foregroundStyle(.secondary)
                        } else {
                        Button("Choose a new selection in Files") {
                            do {
                                onReanchor(try annotation.replacingNote(note))
                                dismiss()
                            } catch {
                                failure = "This comment could not be kept. Check its text and try again."
                            }
                        }
                        .disabled(note.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || note.utf8.count > 4096)
                        .accessibilityIdentifier("annotation-edit-reanchor")
                        }
                    }
                    Section("Comment") {
                        TextEditor(text: $note)
                            .frame(minHeight: 110)
                            .accessibilityIdentifier("annotation-edit-note")
                        if note.utf8.count > 4096 { Text("Keep the comment under 4,096 bytes.").foregroundStyle(.red) }
                    }
                }
                if let failure { Text(failure).foregroundStyle(.red).accessibilityIdentifier("annotation-edit-error") }
            }
            .navigationTitle("Comment")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") { save() }
                        .disabled(annotation == nil || note.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || note.utf8.count > 4096)
                        .accessibilityIdentifier("annotation-edit-save")
                }
            }
            .task(id: attachment.id) { await load() }
        }
    }

    private func load() async {
        loading = true
        defer { loading = false }
        do {
            let data: Data
            if let local = attachment.data { data = local }
            else if let remote = attachment.remoteFile { data = try await model.download(remote, chat: chat) }
            else { throw FileFailure.notUploaded }
            guard !Task.isCancelled, !model.accessEnded,
                  expectedScope == model.assignmentScope else { throw FileFailure.integrity }
            let value = try ArtifactAnnotation.read(data)
            guard value.conversationId == chat.id,
                  value.projectId == model.projects.details[chat.id]?.projectId else { throw FileFailure.integrity }
            annotation = value
            note = value.note
        } catch { failure = "This comment could not be opened. Reopen Files to create a new one." }
    }
    private func save() {
        guard let annotation else { return }
        do {
            try model.stageAnnotation(annotation.replacingNote(note), chat: chat,
                                      expectedScope: expectedScope, replacing: attachment.id,
                                      preserveStaleOnReplace: true)
            dismiss()
        } catch { failure = "This comment could not be saved. Reopen Files to create a new one." }
    }
    private func anchorDescription(_ anchor: ArtifactAnnotation.Anchor) -> String {
        switch anchor {
        case .textLines(let start, let end): "Lines \(start)–\(end)"
        case .diffLines(let scope, _, _, _): "Selected lines in the \(scope == .branch ? "branch" : "uncommitted") diff"
        case .textRange: "Highlighted text"
        case .imageRegion: "Selected image area"
        case .pdfRegion(let page, _, _, _, _): "Selected area on page \(page)"
        }
    }
}
