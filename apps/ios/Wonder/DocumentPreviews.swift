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
import CryptoKit
import os

struct PreviewDeck: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    var initialFile: ConversationFile? = nil
    var attachmentIDs: [String]? = nil
    private var listedFiles: [ConversationFile] {
        (model.files[chat.id] ?? []).filter { attachmentIDs?.contains($0.id) ?? true }
    }
    @Environment(\.dismiss) private var dismiss
    @State private var selected: ConversationFile?
    @State private var bytes: Data?
    @State private var exporting = false
    @State private var busy = false
    @State private var failure: String?
    @State private var photoFile: ConversationFile?
    @State private var visualPage: String?
    /// The selected text or HTML file as it is shown: its first 128 KB at
    /// most, prepared off the main thread when it was downloaded.
    @State private var prepared: PreparedText?
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    private struct PreparedText {
        let text: String?
        let truncated: Bool
    }
    var body: some View {
        NavigationStack {
            Group {
                if busy { ProgressView("Loading verified file…") }
                else if let selected, let bytes, !PhotoViewerRouting.isImage(selected) {
                    ScrollView {
                        VStack(alignment: .leading, spacing: 12) {
                            if selected.mimeType == "application/pdf" {
                                PDFPreview(data: bytes, revision: Self.revision(of: selected)).frame(minHeight: 550)
                            }
                            else if selected.mimeType == "text/html", let html = prepared?.text {
                                Button("Open visual view", systemImage: "doc.richtext") { visualPage = html }
                                    .frame(minHeight: 44)
                                    .accessibilityHint("Shows the page full screen as text, tables and charts.")
                                    .accessibilityIdentifier("html-file-visual")
                                ConstrainedHTML(html: html).frame(minHeight: 550)
                            }
                            // A native visual and an SVG draw from their own verified download.
                            else if selected.mimeType?.lowercased() == ConversationFile.wonderUIType { PublishedPageView(model: model, chat: chat, file: selected) }
                            else if selected.mimeType?.lowercased() == "image/svg+xml" { InlineSVGPreview(model: model, chat: chat, file: selected) }
                            else if selected.mimeType?.hasPrefix("text/") == true, let text = prepared?.text {
                                if prepared?.truncated == true {
                                    Text("Showing the first 128 KB. Save a copy to read the whole file.")
                                        .font(.footnote)
                                        .accessibilityIdentifier("file-preview-truncated")
                                }
                                // The same bounded viewer as Files: UITextView lays out only what is on screen.
                                ReadOnlyAttributedTextView(
                                    text: NSAttributedString(string: text, attributes: [
                                        .font: typography.codeUIFont(.body, size: 15),
                                        .foregroundColor: theme.textUIColor]),
                                    identity: "\(Self.revision(of: selected))|\(theme.cacheKey)|\(typography.cacheKey)")
                                    .frame(minHeight: 550)
                                    .accessibilityIdentifier("file-preview-text")
                            }
                            else if selected.mimeType == "text/html", prepared?.truncated == true {
                                ContentUnavailableView("Preview unavailable", systemImage: "doc",
                                    description: Text("This HTML file is too large to preview safely. Save a copy to open it elsewhere."))
                                    .accessibilityIdentifier("file-preview-unavailable")
                            }
                            else {
                                ContentUnavailableView("Preview unavailable", systemImage: "doc", description: Text("This file type does not have a preview in Cupola."))
                                    .accessibilityIdentifier("file-preview-unavailable")
                            }
                            Button("Save a copy", systemImage: "square.and.arrow.up") { exporting = true }.frame(minHeight: 44)
                            DisclosureGroup("Details") {
                                Text("Verified download · \(selected.updatedAt)").font(.caption)
                                Text(selected.sha256 ?? "").font(.caption.monospaced()).textSelection(.enabled)
                                Text(ByteCountFormatter.string(fromByteCount: Int64(bytes.count), countStyle: .file)).font(.caption)
                            }
                        }.padding()
                    }
                } else {
                    WonderList {
                        ForEach(listedFiles) { file in
                            Button {
                                if PhotoViewerRouting.isImage(file) { photoFile = file }
                                else { open(file) }
                            } label: {
                                Label(file.name, systemImage: PhotoViewerRouting.isImage(file) ? "photo" : "doc")
                            }
                            .disabled(!PhotoViewerRouting.isImage(file) && file.state != "available")
                            .accessibilityIdentifier("file-preview:\(file.id)")
                        }
                    }.overlay { if listedFiles.isEmpty { ContentUnavailableView(attachmentIDs == nil ? "No files yet" : "Attachments unavailable", systemImage: "doc", description: attachmentIDs == nil ? nil : Text("These attachments could not be found on your computer.")) } }
                }
            }
            .safeAreaInset(edge: .bottom) { if let failure { VStack { FailureDetails(message: failure); Button("Try again") { if let selected { open(selected) } else { refresh() } } }.padding() } }
            .fileExporter(isPresented: $exporting, document: ExportedFile(data: bytes ?? Data()), contentType: selected?.mimeType.flatMap { UTType(mimeType: $0) } ?? .data, defaultFilename: selected?.name) { result in
                if case .failure = result { failure = "The copy could not be saved. Try another location." }
            }
            .navigationTitle(selected?.name ?? (attachmentIDs == nil ? "Files" : "Attachments"))
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button(selected == nil ? "Done" : "Files") { if selected == nil { dismiss() } else { selected = nil; bytes = nil; prepared = nil } }.disabled(busy).accessibilityIdentifier(selected == nil ? "conversation-files-done" : "conversation-files-back") }
            }
            .sheet(isPresented: Binding(get: { visualPage != nil }, set: { if !$0 { visualPage = nil } })) {
                if let visualPage {
                    ExpandedMessageSheet(title: selected?.name ?? "Page", content: .page(visualPage, fence: false))
                }
            }
            .fullScreenCover(item: $photoFile) { file in
                PhotoViewer(
                    model: model,
                    chat: chat,
                    file: file,
                    galleryFiles: ConversationAttachmentGallery.imageFiles(listedFiles),
                    initialFileID: file.id
                )
            }
            .task {
                if let initialFile {
                    if PhotoViewerRouting.isImage(initialFile) { photoFile = initialFile }
                    else { open(initialFile) }
                    return
                }
                refresh()
                if model.previewMode, let index = ProcessInfo.processInfo.arguments.firstIndex(of: "-preview-document"), index + 1 < ProcessInfo.processInfo.arguments.count,
                   let file = model.files[chat.id]?.first(where: { $0.name == ProcessInfo.processInfo.arguments[index + 1] }) {
                    if PhotoViewerRouting.isImage(file) { photoFile = file }
                    else { open(file) }
                }
            }
        }
    }
    private func refresh() {
        busy = true; failure = nil
        Task { defer { busy = false }; do { try await model.loadFiles(chat) } catch { failure = "Files could not be loaded. Check your computer." } }
    }
    private func open(_ file: ConversationFile) {
        selected = file; bytes = nil; prepared = nil; failure = nil; busy = true
        Task {
            defer { busy = false }
            do {
                let data = try await model.download(file, chat: chat)
                let mime = file.mimeType ?? ""
                // Up to 8 MB arrives; only what the preview shows is decoded.
                var text: PreparedText?
                if mime.hasPrefix("text/") {
                    let (shown, truncated) = await WorkspaceRevisionState.prepareText(data, mimeType: mime)
                    text = PreparedText(text: shown, truncated: truncated)
                }
                guard selected?.id == file.id else { return }
                prepared = text
                bytes = data
            } catch { failure = "This file could not be verified or downloaded. Check your computer." }
        }
    }
    /// Downloads are verified against their SHA-256; a file listed without
    /// one is named by its identity and time instead of hashing it again.
    nonisolated static func revision(of file: ConversationFile) -> String {
        file.sha256 ?? "\(file.id)@\(file.updatedAt)"
    }
}
/// Parses PDFs off the main thread. Each document is made here and handed to
/// the main actor (`sending`), which owns it from then on: nothing on this
/// actor keeps or touches it again, so PDFView is its only user.
actor PDFDocumentPreparer {
    static let shared = PDFDocumentPreparer()
    #if WONDER_DIAGNOSTICS
    private let log = Logger(subsystem: "com.swaymun.wonder", category: "pdf-preview")
    #endif

    /// nil for a file PDFKit can't read, one with no pages, or a cancelled request.
    func prepare(_ data: Data) -> sending PDFDocument? {
        guard !Task.isCancelled else { return nil }
        #if WONDER_DIAGNOSTICS
        let started = ContinuousClock.now
        #endif
        let document = parsedPDFDocument(data)
        #if WONDER_DIAGNOSTICS
        let milliseconds = Int(started.duration(to: .now) / .milliseconds(1))
        let pages = document?.pageCount ?? 0, bytes = data.count
        log.notice("pdf parsed \(pages > 0 ? "ok" : "failed", privacy: .public) in \(milliseconds, privacy: .public) ms (\(bytes, privacy: .public) bytes, \(pages, privacy: .public) pages)")
        #endif
        return document
    }
}

/// A selected PDF retains its parsed document and viewport while SwiftUI
/// replaces the inline PDFView with a full-screen PDFView or vice versa. It is
/// the document's one owner: a new revision is parsed off the main thread
/// while the current one stays on screen, and a newer revision supersedes a
/// parse still running.
@MainActor final class PDFPreviewSession {
    private struct Viewport {
        let pageIndex: Int
        let point: CGPoint?
        let scale: CGFloat
    }
    private(set) var document: PDFDocument?
    /// The revision `document` (or a failed parse, when it is nil) shows.
    private(set) var revision: String?
    weak var activeView: PDFView?
    private var viewport: Viewport?
    private var preparing: (revision: String, task: Task<Void, Never>)?

    /// The revision the caller last asked to show.
    private var requestedRevision: String?

    /// The revision being parsed, if any (Diagnostics tests read it).
    var preparingRevision: String? { preparing?.revision }
    /// Whether the page on screen is the revision the caller holds. While a
    /// replacement parses, the old page stays visible but the caller's bytes
    /// are already the new ones, so a comment there would be anchored to a
    /// revision the reader can't see: Comment is withheld until it's ready.
    var showsRequestedRevision: Bool {
        requestedRevision != nil && revision == requestedRevision && preparing == nil
    }

    func capture() {
        guard let view = activeView, let document = view.document,
              let page = view.currentDestination?.page ?? view.currentPage else { return }
        let destination = view.currentDestination
        let bounds = page.bounds(for: view.displayBox)
        let point: CGPoint?
        if let destination, bounds.width > 0, bounds.height > 0 {
            point = CGPoint(x: (destination.point.x - bounds.minX) / bounds.width,
                            y: (destination.point.y - bounds.minY) / bounds.height)
        } else { point = nil }
        viewport = Viewport(pageIndex: document.index(for: page), point: point,
                            scale: view.scaleFactor)
    }

    /// Shows `revision` in `view`. A revision already parsed is installed at
    /// once (inline to full screen and back); a new one is parsed first.
    fileprivate func show(_ view: PDFPreviewView, data: Data, revision: String) {
        requestedRevision = revision
        view.session = self
        if activeView !== view {
            activeView = view
            if self.revision == revision {
                install(document, in: view)
                return
            }
            // Another file, or a revision not parsed yet: show nothing stale.
            if view.document !== document { view.document = nil }
        } else if self.revision == revision {
            // The file went back to the revision on screen.
            cancel()
            return
        }
        if view.document == nil {
            view.showParseFailure(false)
            view.showLoading(true)
        }
        if preparing?.revision == revision { return }
        preparing?.task.cancel()
        let task = Task { [weak self] in
            let next = await PDFDocumentPreparer.shared.prepare(data)
            guard let self, !Task.isCancelled, self.preparing?.revision == revision else { return }
            self.preparing = nil
            self.finish(next, revision: revision)
        }
        preparing = (revision, task)
    }

    /// The owning view went away; a parse still running is no longer wanted.
    func cancel() {
        preparing?.task.cancel()
        preparing = nil
    }

    private func finish(_ next: PDFDocument?, revision: String) {
        let previous = document
        document = next
        self.revision = revision
        guard let view = activeView as? PDFPreviewView else { return }
        if let next, let previous, view.document === previous {
            replace(previous, with: next, in: view)
            view.showLoading(false)
            view.showParseFailure(false)
            capture()
        } else {
            install(next, in: view)
        }
    }

    private func install(_ document: PDFDocument?, in view: PDFPreviewView) {
        if view.document !== document { view.document = document }
        view.showLoading(false)
        view.showParseFailure(document == nil)
        guard document != nil, viewport != nil else { return }
        view.restoreAfterLayout = { [weak self, weak view] in
            guard let self, let view, self.activeView === view else { return }
            self.restore(in: view)
        }
        view.setNeedsLayout()
    }

    /// Swaps in a new revision of the open file at the same reading point and zoom.
    private func replace(_ old: PDFDocument, with document: PDFDocument, in view: PDFView) {
        let destination = view.currentDestination
        let oldPage = destination?.page ?? view.currentPage
        let pageIndex = oldPage.map { old.index(for: $0) } ?? 0
        let scale = view.scaleFactor
        view.document = document
        if scale.isFinite, scale > 0 {
            view.scaleFactor = min(view.maxScaleFactor, max(view.minScaleFactor, scale))
        }
        guard document.pageCount > 0,
              let page = document.page(at: min(pageIndex, document.pageCount - 1)) else { return }
        guard let destination, let oldPage else { view.go(to: page); return }
        let oldBounds = oldPage.bounds(for: view.displayBox)
        let newBounds = page.bounds(for: view.displayBox)
        guard oldBounds.width > 0, oldBounds.height > 0 else { view.go(to: page); return }
        let x = (destination.point.x - oldBounds.minX) / oldBounds.width
        let y = (destination.point.y - oldBounds.minY) / oldBounds.height
        // PDFKit treats a destination's Y as the top edge of the
        // viewport; currentDestination reports its bottom edge.
        let viewportHeight = view.convert(view.bounds, to: page).height
        view.go(to: PDFDestination(page: page, at: CGPoint(
            x: newBounds.minX + x * newBounds.width,
            y: newBounds.minY + y * newBounds.height + viewportHeight)))
    }

    private func restore(in view: PDFView) {
        guard let viewport, let document = view.document, document.pageCount > 0,
              let page = document.page(at: min(viewport.pageIndex, document.pageCount - 1)) else { return }
        if viewport.scale.isFinite, viewport.scale > 0 {
            view.scaleFactor = min(view.maxScaleFactor, max(view.minScaleFactor, viewport.scale))
        }
        guard let point = viewport.point else { view.go(to: page); return }
        let bounds = page.bounds(for: view.displayBox)
        let viewportHeight = view.convert(view.bounds, to: page).height
        view.go(to: PDFDestination(page: page, at: CGPoint(
            x: bounds.minX + point.x * bounds.width,
            y: bounds.minY + point.y * bounds.height + viewportHeight)))
    }
}

private final class PDFPreviewView: PDFView {
    var restoreAfterLayout: (() -> Void)?
    /// Offered in the text selection menu when the preview accepts comments.
    var onComment: ((PDFSelection) -> Void)?
    /// The owner of the shown document; Comment needs it to show the caller's revision.
    weak var session: PDFPreviewSession?

    override func buildMenu(with builder: UIMenuBuilder) {
        super.buildMenu(with: builder)
        guard builder.system == .context, session?.showsRequestedRevision == true,
              let onComment, let selection = currentSelection,
              !(selection.string ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        let comment = UIAction(title: "Comment", image: UIImage(systemName: "text.bubble"),
                               identifier: UIAction.Identifier("wonder.pdf.comment")) { [weak self] _ in
            // A newer revision may have arrived while the menu was open.
            guard self?.session?.showsRequestedRevision == true else { return }
            onComment(selection)
        }
        builder.insertChild(UIMenu(options: .displayInline, children: [comment]), atStartOfMenu: .standardEdit)
    }
    private lazy var failureLabel: UILabel = {
        let label = UILabel()
        label.text = "Can't open this PDF."
        label.textColor = .secondaryLabel // theme-exempt: a notice on PDFKit's own page
        label.font = .preferredFont(forTextStyle: .body)
        label.adjustsFontForContentSizeCategory = true
        label.textAlignment = .center
        label.numberOfLines = 0
        label.backgroundColor = .systemBackground // theme-exempt: a notice on PDFKit's own page
        label.translatesAutoresizingMaskIntoConstraints = false
        label.accessibilityIdentifier = "workspace-pdf-error"
        return label
    }()
    private lazy var loadingIndicator: UIActivityIndicatorView = {
        let indicator = UIActivityIndicatorView(style: .large)
        indicator.hidesWhenStopped = true
        indicator.translatesAutoresizingMaskIntoConstraints = false
        indicator.isAccessibilityElement = true
        indicator.accessibilityLabel = "Preparing PDF"
        indicator.accessibilityIdentifier = "workspace-pdf-loading"
        return indicator
    }()

    /// A spinner while the first revision is parsed; never over a shown page.
    func showLoading(_ loading: Bool) {
        if loading && loadingIndicator.superview == nil {
            addSubview(loadingIndicator)
            NSLayoutConstraint.activate([
                loadingIndicator.centerXAnchor.constraint(equalTo: centerXAnchor),
                loadingIndicator.centerYAnchor.constraint(equalTo: centerYAnchor)
            ])
        }
        if loading {
            loadingIndicator.startAnimating()
            bringSubviewToFront(loadingIndicator)
        } else if loadingIndicator.superview != nil {
            loadingIndicator.stopAnimating()
        }
    }

    func showParseFailure(_ failed: Bool) {
        if failed && failureLabel.superview == nil {
            addSubview(failureLabel)
            NSLayoutConstraint.activate([
                failureLabel.leadingAnchor.constraint(equalTo: leadingAnchor),
                failureLabel.trailingAnchor.constraint(equalTo: trailingAnchor),
                failureLabel.topAnchor.constraint(equalTo: topAnchor),
                failureLabel.bottomAnchor.constraint(equalTo: bottomAnchor)
            ])
        }
        if failureLabel.superview != nil { failureLabel.isHidden = !failed }
        if failed { bringSubviewToFront(failureLabel) }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        guard bounds.width > 0, bounds.height > 0 else { return }
        let restore = restoreAfterLayout
        restoreAfterLayout = nil
        restore?()
    }
}

/// A PDF parsed off the main thread. `revision` names the bytes (their
/// SHA-256 where known): the file is parsed again only when it changes.
struct PDFPreview: UIViewRepresentable {
    let data: Data
    let revision: String
    var session: PDFPreviewSession? = nil
    var onComment: ((PDFSelection) -> Void)? = nil
    final class Coordinator {
        var session: PDFPreviewSession
        /// Made here when the caller has none; the caller's otherwise.
        init(session: PDFPreviewSession) { self.session = session }
    }
    func makeCoordinator() -> Coordinator { Coordinator(session: session ?? PDFPreviewSession()) }
    func makeUIView(context: Context) -> PDFView {
        let view = PDFPreviewView()
        view.autoScales = true
        view.onComment = onComment
        context.coordinator.session.show(view, data: data, revision: revision)
        return view
    }
    static func dismantleUIView(_ view: PDFView, coordinator: Coordinator) {
        let session = coordinator.session
        guard session.activeView === view else { return }
        session.capture()
        session.activeView = nil
        session.cancel()
    }
    func updateUIView(_ view: PDFView, context: Context) {
        guard let view = view as? PDFPreviewView else { return }
        view.onComment = onComment
        if let session, session !== context.coordinator.session {
            // The caller opened another file in the same place.
            context.coordinator.session.cancel()
            context.coordinator.session = session
        }
        context.coordinator.session.show(view, data: data, revision: revision)
    }
}

/// A PDF text selection, kept as its area on the first selected page.
struct PDFTextComment: Identifiable {
    let id = UUID()
    let page: Int
    let area: CGRect
    let excerpt: String
    /// The bytes of the revision the selection was made on, pinned when the
    /// comment began so a revision arriving meanwhile can't move it.
    var source: Data?

    @MainActor init?(_ selection: PDFSelection) {
        guard let page = selection.pages.first, let document = page.document else { return nil }
        let box = page.bounds(for: .cropBox), bounds = selection.bounds(for: page)
        guard box.width > 0, box.height > 0, !bounds.isEmpty else { return nil }
        // PDF space starts at the bottom left; annotation areas start at the top left.
        let x = (bounds.minX - box.minX) / box.width, y = (box.maxY - bounds.maxY) / box.height
        let clamped = CGRect(x: x, y: y, width: bounds.width / box.width, height: bounds.height / box.height)
            .intersection(CGRect(x: 0, y: 0, width: 1, height: 1))
        guard !clamped.isNull, clamped.width > 0, clamped.height > 0 else { return nil }
        self.page = document.index(for: page) + 1
        self.area = clamped
        self.excerpt = (selection.string ?? "").split(whereSeparator: \.isNewline).joined(separator: " ")
            .trimmingCharacters(in: .whitespaces)
    }
}

struct WorkspacePDFCommentSheet: View {
    let comment: PDFTextComment
    let source: Data
    let context: ArtifactPreviewContext
    let onAnnotation: (ArtifactAnnotation) throws -> Void
    var initialNote = ""
    var annotationUnavailableReason: String?
    @Environment(\.dismiss) private var dismiss
    @State private var note = ""
    @State private var failure: String?
    @FocusState private var focused: Bool
    var body: some View {
        NavigationStack {
            WonderForm {
                Section("Page \(comment.page)") {
                    Text(comment.excerpt.isEmpty ? "Selected area" : "“\(String(comment.excerpt.prefix(300)))”")
                        .font(.callout).foregroundStyle(.secondary).lineLimit(4)
                        .accessibilityIdentifier("annotation-pdf-excerpt")
                }
                Section {
                    TextField("Add a comment", text: $note, axis: .vertical)
                        .lineLimit(3...8).focused($focused)
                        .accessibilityIdentifier("annotation-pdf-note")
                    if note.utf8.count > 4096 { Text("Keep the comment under 4,096 bytes.").font(.footnote).foregroundStyle(.red) }
                    if let annotationUnavailableReason { Text(annotationUnavailableReason).font(.footnote).foregroundStyle(.secondary) }
                    if let failure { Text(failure).font(.footnote).foregroundStyle(.red) }
                }
            }
            .navigationTitle("Comment").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Add") { add() }
                        .disabled(annotationUnavailableReason != nil || note.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                                  || note.utf8.count > 4096)
                        .accessibilityIdentifier("annotation-pdf-add")
                }
            }
            .onAppear { if note.isEmpty { note = initialNote }; focused = true }
        }
        .presentationDetents([.medium, .large]).presentationDragIndicator(.hidden)
    }
    private func add() {
        do {
            try onAnnotation(ArtifactAnnotation(projectId: context.projectID, conversationId: context.conversationID,
                rootId: context.rootID, path: context.path, source: source, pdfPage: comment.page,
                x: Double(comment.area.minX), y: Double(comment.area.minY),
                width: Double(comment.area.width), height: Double(comment.area.height), note: note))
            dismiss()
        } catch { failure = "The comment could not be saved. Reopen the file and try again." }
    }
}

private func parsedPDFDocument(_ data: Data) -> PDFDocument? {
    guard let document = PDFDocument(data: data), document.pageCount > 0 else { return nil }
    return document
}

struct WorkspaceAnnotationNoteField: UIViewRepresentable {
    @ObservedObject var draft: WorkspaceTextAnnotationDraft

    func makeCoordinator() -> Coordinator { Coordinator(draft: draft) }

    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        context.coordinator.isUpdating = true
        draft.activeNoteFieldID = context.coordinator.id
        view.delegate = context.coordinator
        view.font = .preferredFont(forTextStyle: .body)
        view.adjustsFontForContentSizeCategory = true
        view.textColor = context.environment.wonderTheme.textUIColor
        view.tintColor = .tintColor
        view.backgroundColor = .clear
        view.textContainerInset = UIEdgeInsets(top: 8, left: 0, bottom: 8, right: 0)
        view.textContainer.lineFragmentPadding = 0
        view.isScrollEnabled = true
        view.text = draft.note
        view.selectedRange = Self.clamp(draft.noteSelection, length: (view.text as NSString).length)
        view.accessibilityLabel = "Add a comment"
        view.accessibilityIdentifier = "annotation-note"
        context.coordinator.isUpdating = false
        let editorID = context.coordinator.id
        Task { @MainActor [weak view] in
            await Task.yield()
            guard let view, view.window != nil, draft.activeNoteFieldID == editorID else { return }
            view.becomeFirstResponder()
        }
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        context.coordinator.draft = draft
        let textColor = context.environment.wonderTheme.textUIColor
        if view.textColor != textColor { view.textColor = textColor }
        guard draft.activeNoteFieldID == context.coordinator.id,
              view.markedTextRange == nil,
              view.text != draft.note else { return }
        context.coordinator.isUpdating = true
        view.text = draft.note
        view.selectedRange = Self.clamp(draft.noteSelection, length: (view.text as NSString).length)
        context.coordinator.isUpdating = false
    }

    static func dismantleUIView(_ view: UITextView, coordinator: Coordinator) {
        if coordinator.draft.activeNoteFieldID == coordinator.id {
            coordinator.draft.activeNoteFieldID = nil
        }
        view.delegate = nil
    }

    func sizeThatFits(_ proposal: ProposedViewSize, uiView: UITextView, context: Context) -> CGSize? {
        guard let width = proposal.width, width > 0 else { return nil }
        let line = uiView.font?.lineHeight ?? 20
        let content = uiView.sizeThatFits(CGSize(width: width, height: .greatestFiniteMagnitude)).height
        return CGSize(width: width, height: min(max(content, max(44, 2 * line)), 4 * line + 16))
    }

    private static func clamp(_ range: NSRange?, length: Int) -> NSRange {
        guard let range, range.location >= 0 else { return NSRange(location: length, length: 0) }
        let start = min(range.location, length)
        return NSRange(location: start, length: min(range.length, length - start))
    }

    final class Coordinator: NSObject, UITextViewDelegate {
        let id = UUID()
        var draft: WorkspaceTextAnnotationDraft
        var isUpdating = false

        init(draft: WorkspaceTextAnnotationDraft) { self.draft = draft }

        func textViewDidChange(_ view: UITextView) { publish(view) }
        func textViewDidChangeSelection(_ view: UITextView) { publish(view) }
        func textViewDidEndEditing(_ view: UITextView) { publish(view) }

        private func publish(_ view: UITextView) {
            guard !isUpdating, view.markedTextRange == nil,
                  draft.activeNoteFieldID == id else { return }
            let text = view.text ?? ""
            let range = view.selectedRange
            // User input is outside SwiftUI layout. Publish it before a tap on
            // Save, Cancel, or Full screen can replace this editor.
            if draft.note != text { draft.note = text }
            if draft.noteSelection != range { draft.noteSelection = range }
        }
    }
}
@MainActor final class WorkspaceHTMLPreviewSession {
    weak var activeView: WKWebView?
    private var sourceID: String?
    private var readingFraction: CGFloat = 0
    private var activeReady = false

    func capture() {
        guard activeReady, let view = activeView else { return }
        let scroll = view.scrollView
        let available = max(0, scroll.contentSize.height - scroll.bounds.height)
        readingFraction = available > 0 ? min(1, max(0, scroll.contentOffset.y / available)) : 0
    }

    func install(_ view: WKWebView, sourceID: String) {
        if self.sourceID != sourceID {
            self.sourceID = sourceID
            readingFraction = 0
        }
        activeView = view
        activeReady = false
    }

    @discardableResult func restore(_ view: WKWebView, sourceID: String) -> Bool {
        guard activeView === view, self.sourceID == sourceID else { return true }
        view.scrollView.layoutIfNeeded()
        guard view.scrollView.bounds.height > 0, view.scrollView.contentSize.height > 0 else { return false }
        let available = max(0, view.scrollView.contentSize.height - view.scrollView.bounds.height)
        guard readingFraction == 0 || available > 0 else { return false }
        view.scrollView.setContentOffset(CGPoint(x: 0, y: readingFraction * available), animated: false)
        activeReady = true
        return true
    }
}

private final class WorkspaceHTMLWebView: WKWebView {
    var afterLayout: (() -> Void)?
    override func layoutSubviews() {
        super.layoutSubviews()
        afterLayout?()
    }
}

struct ConstrainedHTML: UIViewRepresentable {
    let html: String
    var sourceID: String? = nil
    var session: WorkspaceHTMLPreviewSession? = nil
    /// Called when the page finishes loading (Diagnostics scenarios read it).
    var onLoad: (@MainActor () -> Void)? = nil
    final class Guard: NSObject, WKNavigationDelegate {
        var loadedHTML: String?
        var onLoad: (@MainActor () -> Void)?
        var sourceID: String?
        weak var session: WorkspaceHTMLPreviewSession?
        private var contentSizeObservation: NSKeyValueObservation?
        private var currentNavigation: WKNavigation?

        init(session: WorkspaceHTMLPreviewSession?, sourceID: String?) {
            self.session = session
            self.sourceID = sourceID
        }
        func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void) {
            decisionHandler(action.request.url?.absoluteString == "about:blank" ? .allow : .cancel)
        }
        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            guard let currentNavigation, navigation === currentNavigation else { return }
            if let onLoad { MainActor.assumeIsolated { onLoad() } }
            contentSizeObservation?.invalidate()
            guard sourceID != nil, session != nil else { return }
            let expectedNavigation = currentNavigation
            (webView as? WorkspaceHTMLWebView)?.afterLayout = { [weak self, weak webView] in
                guard let self, let webView else { return }
                self.restoreIfReady(webView, navigation: expectedNavigation)
            }
            contentSizeObservation = webView.scrollView.observe(\.contentSize, options: [.initial, .new]) {
                [weak self, weak webView] _, _ in
                Task { @MainActor in
                    guard let self, let webView else { return }
                    self.restoreIfReady(webView, navigation: expectedNavigation)
                }
            }
            webView.setNeedsLayout()
        }
        private func restoreIfReady(_ view: WKWebView, navigation: WKNavigation) {
            guard currentNavigation === navigation, let sourceID,
                  session?.restore(view, sourceID: sourceID) == true else { return }
            contentSizeObservation?.invalidate()
            contentSizeObservation = nil
            (view as? WorkspaceHTMLWebView)?.afterLayout = nil
        }
        func load(_ html: String, in view: WKWebView) {
            contentSizeObservation?.invalidate()
            contentSizeObservation = nil
            (view as? WorkspaceHTMLWebView)?.afterLayout = nil
            loadedHTML = html
            let csp = "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; img-src data: blob:; connect-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'"
            currentNavigation = view.loadHTMLString("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><style>body{font:-apple-system-body}button,input{font:inherit}</style><meta http-equiv=\"Content-Security-Policy\" content=\"\(csp)\">" + html, baseURL: nil)
        }
    }
    func makeCoordinator() -> Guard { Guard(session: session, sourceID: sourceID) }
    func makeUIView(context: Context) -> WKWebView {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false
        configuration.defaultWebpagePreferences.allowsContentJavaScript = false
        let view = WorkspaceHTMLWebView(frame: .zero, configuration: configuration)
        view.navigationDelegate = context.coordinator
        context.coordinator.onLoad = onLoad
        if let sourceID { session?.install(view, sourceID: sourceID) }
        context.coordinator.load(html, in: view)
        return view
    }
    func updateUIView(_ view: WKWebView, context: Context) {
        context.coordinator.onLoad = onLoad
        guard context.coordinator.loadedHTML != html || context.coordinator.sourceID != sourceID else { return }
        context.coordinator.sourceID = sourceID
        if let sourceID { session?.install(view, sourceID: sourceID) }
        context.coordinator.load(html, in: view)
    }
}

struct ExportedFile: FileDocument {
    static var readableContentTypes: [UTType] { [.data] }
    let data: Data
    init(data: Data) { self.data = data }
    init(configuration: ReadConfiguration) throws { data = configuration.file.regularFileContents ?? Data() }
    func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper { FileWrapper(regularFileWithContents: data) }
}

/// T3-compatible presentation tokens, resolved from the current Cupola theme.
/// These are defaults, not a rewrite of the author's CSS. A page's original
/// bytes remain the integrity/cache identity; presentation never changes them.
struct HTMLArtifactTheme: Equatable {
    let appearance: String
    let variables: [String: String]

    /// `overPhoto` leaves the page transparent so a full-screen page shows the theme's photo like the sheet around it.
    init(theme: WonderTheme, fonts: WonderTypography = WonderTypography(), textScale: CGFloat = 1, overPhoto: Bool = false) {
        let p = theme.palette
        func hex(_ value: UInt32) -> String { String(format: "#%06x", value) }
        func css(_ color: UIColor) -> String {
            let resolved = color.resolvedColor(with: UITraitCollection(userInterfaceStyle: theme.isDark ? .dark : .light))
            var r: CGFloat = 0, g: CGFloat = 0, b: CGFloat = 0, a: CGFloat = 0
            resolved.getRed(&r, green: &g, blue: &b, alpha: &a)
            return "rgba(\(Int(r * 255)),\(Int(g * 255)),\(Int(b * 255)),\(a))"
        }
        appearance = theme.isDark ? "dark" : "light"
        let background = css(theme.pageUIColor), foreground = css(theme.textUIColor)
        let surface = css(theme.surfaceUIColor), accent = hex(p.accent)
        let secondary = theme.isDefault ? css(.secondaryLabel) : hex(p.secondaryText)
        let border = theme.isDefault ? css(.separator) : hex(p.separator)
        let sans: String = switch fonts.message {
        case .system: "-apple-system, system-ui, sans-serif"
        case .serif: "ui-serif, Georgia, serif"
        case .rounded: "ui-rounded, -apple-system, sans-serif"
        case .monospaced: "ui-monospace, Menlo, monospace"
        case .inter: "Inter, -apple-system, sans-serif"
        case .atkinson: "'Atkinson Hyperlegible', -apple-system, sans-serif"
        }
        let mono: String = switch fonts.code {
        case .system: "ui-monospace, 'SF Mono', Menlo, monospace"
        case .jetBrains: "'JetBrains Mono', ui-monospace, monospace"
        case .plex: "'IBM Plex Mono', ui-monospace, monospace"
        }
        var values = [
            "--background": overPhoto && theme.photo != nil ? "transparent" : background, "--foreground": foreground,
            "--muted": surface, "--muted-foreground": secondary,
            "--card": surface, "--card-foreground": foreground,
            "--popover": surface, "--popover-foreground": foreground,
            "--secondary": surface, "--secondary-foreground": foreground,
            "--border": border, "--input": border, "--ring": accent,
            "--primary": accent, "--primary-foreground": hex(ThemePalette.contrast(p.accent, 0xffffff) >= 4.5 ? 0xffffff : 0x000000),
            "--accent": accent, "--accent-foreground": accent,
            "--accent-surface": "color-mix(in srgb, \(accent) 16%, \(background))",
            "--accent-surface-foreground": accent,
            "--code-background": hex(p.codeBackground), "--code-foreground": foreground,
            "--radius": "0.625rem", "--font-sans": sans, "--font-mono": mono,
            "--cupola-page-font-size": "\(14 * textScale)px"
        ]
        for (name, role) in [("destructive", StatusRole.error), ("warning", .warning), ("success", .success), ("info", .info)] {
            let color = hex(p.status(role))
            values["--\(name)"] = color
            values["--\(name)-foreground"] = color
            if name == "destructive" || name == "warning" {
                values["--\(name)-surface"] = "color-mix(in srgb, \(color) 16%, \(background))"
            }
        }
        for (index, color) in p.series.enumerated() { values["--chart-\(index + 1)"] = hex(color) }
        variables = values
    }

    /// Setting host tokens on the root also overrides the fallback palette
    /// embedded in already-published T3 attachments. Never reload for a theme change.
    var script: String {
        let data = try! JSONSerialization.data(withJSONObject: variables, options: [.sortedKeys])
        let json = String(decoding: data, as: UTF8.self)
        return """
        (() => {
          const root = document.documentElement;
          if (!root) return;
          const variables = \(json);
          for (const [key, value] of Object.entries(variables)) root.style.setProperty(key, value);
          root.style.colorScheme = '\(appearance)';
          window.dispatchEvent(new MessageEvent('message', {data: {jsonrpc: '2.0',
            method: 'ui/notifications/host-context-changed', params: {theme: '\(appearance)', styles: {variables}}}}));
        })();
        """
    }
}

/// Published HTML has its own sandbox; ordinary workspace/SVG previews keep
/// ConstrainedHTML's scripts-off policy. No network or native action bridge.
@MainActor enum HTMLArtifactSandbox {
    static let policy = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; "
        + "font-src data:; media-src 'none'; connect-src 'none'; frame-src 'none'; worker-src 'none'; object-src 'none'; "
        + "form-action 'none'; base-uri 'none'"
    static let baseCSS = """
    html{background:var(--background);color:var(--foreground);font-family:var(--font-sans);font-size:var(--cupola-page-font-size,14px);line-height:1.5;-webkit-font-smoothing:antialiased;-webkit-text-size-adjust:100%;scrollbar-width:none}
    html::-webkit-scrollbar{display:none}body{margin:0}code,kbd,pre,samp{font-family:var(--font-mono)}
    :where(:focus-visible){outline:2px solid var(--ring);outline-offset:-2px}
    """
    private enum Failure: Error { case contentRulesUnavailable }
    private static var compilation: Task<WKContentRuleList, Error>?
    static func rules() async throws -> WKContentRuleList {
        if let compilation { return try await compilation.value }
        let task = Task { @MainActor in
            let rules = "[" + ([#"{"trigger":{"url-filter":".*"},"action":{"type":"block"}}"#]
                + ["data", "about", "blob"].map { #"{"trigger":{"url-filter":"^"# + $0 + #":"},"action":{"type":"ignore-previous-rules"}}"# })
                .joined(separator: ",") + "]"
            guard let compiled = try await WKContentRuleListStore.default().compileContentRuleList(
                forIdentifier: "cupola-html-artifact-offline", encodedContentRuleList: rules) else { throw Failure.contentRulesUnavailable }
            return compiled
        }
        compilation = task
        do { return try await task.value } catch { compilation = nil; throw error }
    }

    /// In an isolated content world: page scripts cannot call this handler or
    /// replace the observer. Changes coalesce to one size report per frame.
    static let sizingScript = """
    (() => {
      let pending = false, last = -1;
      const report = () => {
        pending = false;
        const root = document.documentElement;
        if (!root) return;
        const height = Math.ceil(root.scrollHeight > root.clientHeight ? root.scrollHeight : root.getBoundingClientRect().height);
        if (!Number.isFinite(height) || height === last) return;
        last = height;
        window.webkit.messageHandlers.cupolaPageSize.postMessage(height);
      };
      const schedule = () => { if (!pending) { pending = true; requestAnimationFrame(report); } };
      const observer = new ResizeObserver(schedule);
      observer.observe(document.documentElement);
      if (document.body) observer.observe(document.body);
      window.addEventListener('load', schedule);
      schedule();
    })();
    """
}

/// Inline frames fit their contents up to a phone-sized preview; long pages
/// scroll inside that frame and Expand opens the same original HTML full screen.
struct HTMLArtifactView: View {
    let html: String
    var inline = false
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var fonts
    @ScaledMetric(relativeTo: .body) private var textScale = 1.0
    @State private var height: CGFloat = 80
    @State private var loaded = false
    @State private var failure = false
    @State private var retry = 0

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if failure {
                Label("This page couldn’t be displayed.", systemImage: "exclamationmark.triangle")
                Button("Try again") { failure = false; loaded = false; retry += 1 }
                    .frame(minHeight: 44)
            } else {
                browser
                    .overlay { if !loaded { ProgressView("Loading page") } }
            }
        }
        .accessibilityIdentifier("html-visual-page")
    }

    @ViewBuilder private var browser: some View {
        let view = HTMLArtifactWebView(html: html, theme: HTMLArtifactTheme(theme: theme, fonts: fonts, textScale: textScale, overPhoto: !inline),
            onHeight: { height = min(20_000, max(1, $0)) }, onLoad: { loaded = true }, onFailure: { failure = true })
            .id(retry)
        if inline { view.frame(height: min(600, height)) }
        else { view.frame(maxWidth: .infinity, maxHeight: .infinity) }
    }
}

struct HTMLArtifactWebView: UIViewRepresentable {
    let html: String
    let theme: HTMLArtifactTheme
    var onHeight: @MainActor (CGFloat) -> Void = { _ in }
    var onLoad: @MainActor () -> Void = {}
    var onFailure: @MainActor () -> Void = {}

    @MainActor final class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate, WKScriptMessageHandler {
        var html: String?
        var theme: HTMLArtifactTheme?
        var onHeight: @MainActor (CGFloat) -> Void = { _ in }
        var onLoad: @MainActor () -> Void = {}
        var onFailure: @MainActor () -> Void = {}
        var task: Task<Void, Never>?
        weak var view: WKWebView?

        func update(_ value: HTMLArtifactWebView, view: WKWebView) {
            self.view = view
            onHeight = value.onHeight; onLoad = value.onLoad; onFailure = value.onFailure
            view.overrideUserInterfaceStyle = value.theme.appearance == "dark" ? .dark : .light
            guard html != value.html else {
                if theme != value.theme {
                    theme = value.theme
                    view.evaluateJavaScript(value.theme.script)
                }
                return
            }
            html = value.html; theme = value.theme
            task?.cancel()
            view.stopLoading()
            let controller = view.configuration.userContentController
            controller.removeAllUserScripts()
            controller.addUserScript(WKUserScript(source: HTMLArtifactSandbox.sizingScript, injectionTime: .atDocumentEnd,
                                                  forMainFrameOnly: true, in: .defaultClient))
            task = Task { @MainActor [weak self, weak view] in
                do {
                    let rules = try await HTMLArtifactSandbox.rules()
                    guard !Task.isCancelled, let self, let view else { return }
                    guard value.html.utf8.count <= HTMLVisualPage.Limits.inputBytes else { self.onFailure(); return }
                    // Compile before loading: a blocker failure must never allow a page to run.
                    controller.add(rules)
                    // Prefix is parsed before any author script or resource. Additional
                    // author CSPs can only tighten it. No file/base URL is granted.
                    let prefix = "<!doctype html><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">"
                        + "<meta http-equiv=\"Content-Security-Policy\" content=\"\(HTMLArtifactSandbox.policy)\">"
                        + "<style>\(HTMLArtifactSandbox.baseCSS)</style>"
                        + "<script>\((self.theme ?? value.theme).script)</script>"
                    view.loadHTMLString(prefix + value.html, baseURL: nil)
                } catch { if !Task.isCancelled { self?.onFailure() } }
            }
        }

        func userContentController(_ userContentController: WKUserContentController, didReceive message: WKScriptMessage) {
            guard message.webView === view, message.frameInfo.isMainFrame,
                  let number = message.body as? NSNumber, number.doubleValue.isFinite else { return }
            onHeight(CGFloat(number.doubleValue))
        }
        func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                     decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void) {
            decisionHandler(action.request.url?.absoluteString == "about:blank" && action.targetFrame?.isMainFrame == true
                && action.navigationType == .other ? .allow : .cancel)
        }
        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) { onLoad() }
        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
            if (error as NSError).code != NSURLErrorCancelled { onFailure() }
        }
        func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
            if (error as NSError).code != NSURLErrorCancelled { onFailure() }
        }
        func webViewWebContentProcessDidTerminate(_ webView: WKWebView) { onFailure() }
        func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String,
                     initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping @MainActor () -> Void) { completionHandler() }
        func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String,
                     initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping @MainActor (Bool) -> Void) { completionHandler(false) }
        func webView(_ webView: WKWebView, runJavaScriptTextInputPanelWithPrompt prompt: String, defaultText: String?,
                     initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping @MainActor (String?) -> Void) { completionHandler(nil) }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }
    func makeUIView(context: Context) -> WKWebView { makeView(coordinator: context.coordinator) }
    /// Also used by the WebKit contract checks, so they exercise the product configuration.
    func makeView(coordinator: Coordinator) -> WKWebView {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false
        configuration.defaultWebpagePreferences.allowsContentJavaScript = true
        configuration.mediaTypesRequiringUserActionForPlayback = .all
        configuration.userContentController.add(coordinator, contentWorld: .defaultClient, name: "cupolaPageSize")
        let view = WKWebView(frame: .zero, configuration: configuration)
        view.navigationDelegate = coordinator; view.uiDelegate = coordinator
        view.isOpaque = false; view.backgroundColor = .clear; view.scrollView.backgroundColor = .clear
        coordinator.update(self, view: view)
        return view
    }
    func updateUIView(_ view: WKWebView, context: Context) { context.coordinator.update(self, view: view) }
    static func dismantleUIView(_ view: WKWebView, coordinator: Coordinator) {
        coordinator.task?.cancel(); coordinator.task = nil
        view.stopLoading(); view.navigationDelegate = nil; view.uiDelegate = nil
        view.configuration.userContentController.removeAllScriptMessageHandlers()
        view.configuration.userContentController.removeAllUserScripts()
        coordinator.view = nil
    }
}
