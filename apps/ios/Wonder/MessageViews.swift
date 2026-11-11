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
import os

struct MessageAttachmentPresentation: Identifiable, Sendable {
    let id: String
    let file: ConversationFile?
    let name: String
    let mimeType: String?

    init(id: String, file: ConversationFile?) {
        self.id = id
        self.file = file
        self.name = file?.name ?? "Attachment"
        self.mimeType = file?.mimeType
    }

    var isImage: Bool { PhotoViewerRouting.isImage(mimeType: mimeType) }
    var iconName: String {
        if isImage { return "photo" }
        if mimeType == "application/pdf" { return "doc.richtext" }
        if mimeType?.hasPrefix("text/") == true { return "doc.text" }
        return "paperclip"
    }
}

enum ConversationAttachmentGallery {
    nonisolated static func imageFiles(_ files: [ConversationFile]) -> [ConversationFile] {
        files.filter(PhotoViewerRouting.isImage)
    }
}

struct MessagePhotoGallery: Identifiable {
    let files: [ConversationFile]
    let initialFileID: String
    var id: String { initialFileID + ":" + files.map(\.id).joined(separator: ",") }
}

struct MessageAttachmentStrip: View {
    let attachments: [MessageAttachmentPresentation]
    let imagePreviews: ToolImagePreviews
    let imagePreviewScope: UUID
    let chatID: String
    let loadRemoteData: @Sendable (ConversationFile) async throws -> Data
    let openImage: (MessageAttachmentPresentation) -> Void
    let openDocument: () -> Void

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(attachments) { attachment in
                    MessageAttachmentItem(
                        attachment: attachment,
                        imagePosition: attachment.isImage ? imagePosition(for: attachment) : nil,
                        imageCount: imageCount,
                        imagePreviews: imagePreviews,
                        imagePreviewScope: imagePreviewScope,
                        chatID: chatID,
                        loadRemoteData: loadRemoteData,
                        openImage: openImage,
                        openDocument: openDocument
                    )
                }
            }
            .padding(.horizontal, 2)
            .padding(.vertical, 4)
        }
        .frame(height: attachments.contains(where: \.isImage) ? 82 : 44)
        .accessibilityIdentifier("message-attachments")
    }

    private var imageCount: Int { attachments.filter(\.isImage).count }
    private func imagePosition(for attachment: MessageAttachmentPresentation) -> Int? {
        guard attachment.isImage else { return nil }
        return attachments.prefix { $0.id != attachment.id }.filter(\.isImage).count + 1
    }
}

private struct MessageAttachmentItem: View {
    @Environment(\.wonderTheme) private var theme
    let attachment: MessageAttachmentPresentation
    let imagePosition: Int?
    let imageCount: Int
    let imagePreviews: ToolImagePreviews
    let imagePreviewScope: UUID
    let chatID: String
    let loadRemoteData: @Sendable (ConversationFile) async throws -> Data
    let openImage: (MessageAttachmentPresentation) -> Void
    let openDocument: () -> Void
    @State private var thumbnail: UIImage?
    @State private var failed = false

    private var request: ToolImageRequest? {
        guard let file = attachment.file, attachment.isImage else { return nil }
        return ToolImageRequest(scope: imagePreviewScope, chatID: chatID, file: file, size: .chip)
    }

    var body: some View {
        Group {
            if attachment.isImage {
                Button { openImage(attachment) } label: {
                    ZStack {
                        theme.surface
                        if let thumbnail {
                            Image(uiImage: thumbnail)
                                .resizable()
                                .scaledToFill()
                                .frame(width: 72, height: 72)
                                .accessibilityHidden(true)
                        } else if failed || attachment.file == nil {
                            Image(systemName: "photo.badge.exclamationmark")
                                .foregroundStyle(.secondary)
                                .accessibilityHidden(true)
                        } else {
                            ProgressView().accessibilityHidden(true)
                        }
                    }
                    .frame(width: 72, height: 72)
                    .clipped()
                    .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
                    .overlay {
                        RoundedRectangle(cornerRadius: 10, style: .continuous)
                            .strokeBorder(Color.primary.opacity(0.14), lineWidth: 1)
                    }
                }
                .buttonStyle(.plain)
                .accessibilityLabel(imageLabel)
                .accessibilityIdentifier("message-attachment-open:\(attachment.id)")
                .accessibilityValue(thumbnail != nil ? "Loaded" : failed || attachment.file == nil ? "Unavailable" : "Loading")
            } else {
                Button(action: openDocument) {
                    Label(attachment.name, systemImage: attachment.iconName)
                        .font(.subheadline)
                        .lineLimit(1)
                        .frame(minHeight: 40)
                        .padding(.horizontal, 10)
                }
                .buttonStyle(.plain)
                .background(theme.chip, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay {
                    RoundedRectangle(cornerRadius: 10, style: .continuous)
                        .strokeBorder(Color.primary.opacity(0.12), lineWidth: 1)
                }
                .accessibilityLabel(attachment.name)
                .accessibilityIdentifier("message-attachment-file:\(attachment.id)")
            }
        }
        .task(id: request) { await loadThumbnail(request) }
        .onDisappear { thumbnail = nil }
    }

    private var imageLabel: String {
        if let imagePosition, imageCount > 1 {
            return "Open image \(attachment.name), \(imagePosition) of \(imageCount)"
        }
        return "Open image \(attachment.name)"
    }

    private func loadThumbnail(_ request: ToolImageRequest?) async {
        thumbnail = nil
        failed = false
        guard let request, let file = attachment.file else {
            if attachment.isImage { failed = true }
            return
        }
        do {
            let image = try await imagePreviews.image(for: request) { [loadRemoteData] in
                try await loadRemoteData(file)
            }
            try Task.checkCancellation()
            guard request == self.request else { return }
            thumbnail = image
        } catch {
            guard !Task.isCancelled, request == self.request else { return }
            failed = true
        }
    }
}

/// Wonder's report that delegated tasks finished: a compact system row, since
/// the owner did not write it and it needs no bubble.
struct AgentTasksFinishedRow: View {
    let title: String
    var body: some View {
        Label(title, systemImage: "checkmark.circle")
            .font(.caption).foregroundStyle(.secondary)
            .lineLimit(2).multilineTextAlignment(.center)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 2)
            .accessibilityIdentifier("agent-tasks-finished")
    }
}

struct MessageRow: View {
    @Environment(\.wonderTypography) private var typography
    let row: ReadRow
    var pending: PendingSend? = nil
    var recoveredBeforeReconnect = false
    var isSending = false
    /// The agent has not taken this sent message yet (`SendAcceptance.label`).
    var acceptance: String? = nil
    var canRetry = false
    var retry: () -> Void = {}
    /// Sends a message the Mac did not deliver again (see `row.undelivered`).
    var retryUndelivered: ((String) async throws -> Void)? = nil
    var attachments: [MessageAttachmentPresentation] = []
    /// The provider that wrote this reply, set where the thread changed provider.
    var providerNote: String? = nil
    var imagePreviews: ToolImagePreviews? = nil
    var imagePreviewScope = UUID()
    var chatID = "message"
    var loadRemoteData: (@Sendable (ConversationFile) async throws -> Data)? = nil
    var openImage: (MessageAttachmentPresentation) -> Void = { _ in }
    var openDocument: () -> Void = {}
    /// Set on a finished agent reply of a Project thread.
    var forkFromHere: (() -> Void)? = nil
    /// Set on the owner's sent message where the chat can be rewound to it.
    var editFromHere: (() -> Void)? = nil
    /// Returns an explanation when the existing draft cannot safely be filled.
    var draftNativeReply: ((String) -> String?)? = nil
    /// Set on the last reply of a finished turn: when it finished, shown under the bubble with Copy.
    private let finishedLabel: String?
    /// Prepare text metadata when the row input is constructed, never while
    /// SwiftUI repeatedly lays out the bubble. JSON decoding stays on the actor.
    private let nativeReplyRevision: NativeReplyRevision?
    /// Interactive replies and replies carrying an HTML page open full screen.
    private let expansion: ExpandedMessageKind?

    init(row: ReadRow, pending: PendingSend? = nil, recoveredBeforeReconnect: Bool = false,
         isSending: Bool = false, acceptance: String? = nil, canRetry: Bool = false,
         retry: @escaping () -> Void = {}, retryUndelivered: ((String) async throws -> Void)? = nil,
         attachments: [MessageAttachmentPresentation] = [], imagePreviews: ToolImagePreviews? = nil,
         imagePreviewScope: UUID = UUID(), chatID: String = "message",
         loadRemoteData: (@Sendable (ConversationFile) async throws -> Data)? = nil,
         openImage: @escaping (MessageAttachmentPresentation) -> Void = { _ in },
         openDocument: @escaping () -> Void = {}, forkFromHere: (() -> Void)? = nil,
         editFromHere: (() -> Void)? = nil, draftNativeReply: ((String) -> String?)? = nil,
         finishedAt: String? = nil, providerNote: String? = nil) {
        self.providerNote = providerNote
        self.row = row; self.pending = pending; self.recoveredBeforeReconnect = recoveredBeforeReconnect
        self.isSending = isSending; self.acceptance = acceptance; self.canRetry = canRetry
        self.retry = retry; self.retryUndelivered = retryUndelivered; self.attachments = attachments
        self.imagePreviews = imagePreviews; self.imagePreviewScope = imagePreviewScope; self.chatID = chatID
        self.loadRemoteData = loadRemoteData; self.openImage = openImage; self.openDocument = openDocument
        self.forkFromHere = forkFromHere; self.editFromHere = editFromHere; self.draftNativeReply = draftNativeReply
        finishedLabel = finishedAt.map { ReplyFinishedTime.label($0) }
        nativeReplyRevision = !row.isUser && row.text.contains("```wonder-ui") ? NativeReplyRevision(row.text) : nil
        expansion = row.isUser ? nil : nativeReplyRevision != nil ? .interactive : row.text.contains("```html") ? .page : nil
    }

    @State private var selectingText = false
    @State private var expanded = false
    /// The text's width once the bubble is too tall to lift whole; nil otherwise.
    @State private var tallPreviewWidth: CGFloat?
    var body: some View {
        if let source = row.source, source.isWake {
            AgentTasksFinishedRow(title: source.sourceTitle)
        } else {
            messageBody
        }
    }
    private var messageBody: some View {
        PreparedMessageLayout(isUser: row.isUser) {
            VStack(alignment: row.isUser ? .trailing : .leading, spacing: 4) {
                bubble
                if let finishedLabel, !row.text.isEmpty {
                    ReplyFooter(time: finishedLabel, text: row.text)
                }
                if let acceptance {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.mini)
                        Text(acceptance)
                    }
                    .font(.caption).foregroundStyle(.secondary)
                    .accessibilityElement(children: .combine)
                    .accessibilityIdentifier("message-acceptance")
                }
                if pending == nil, let undelivered = row.undelivered {
                    UndeliveredNotice(undelivered: undelivered, canRetry: canRetry && retryUndelivered != nil,
                                      retry: retryUndelivered ?? { _ in })
                }
            }
        } receipt: {
            if let pending {
                if isSending {
                    ProgressView().frame(width: 44, height: 44)
                        .accessibilityLabel("Sending message")
                } else if pending.receipt == nil {
                    Button(action: retry) {
                        Image(systemName: "exclamationmark.circle.fill")
                            .font(.body.weight(.semibold)).foregroundStyle(.red)
                            .frame(width: 44, height: 44)
                    }
                    .buttonStyle(.plain)
                    .disabled(!canRetry)
                    .accessibilityLabel(pending.rejected == true ? "Restore unsent message to draft" : "Delivery unconfirmed. Try again")
                    .accessibilityHint(canRetry ? "Retries this same message without creating a duplicate." : "Reconnect to your computer to try again.")
                    .accessibilityIdentifier("retry-message")
                } else if acceptance == nil {
                    Image(systemName: "checkmark").foregroundStyle(.secondary)
                        .frame(width: 44, height: 44)
                        .accessibilityLabel("Received by your computer")
                }
            }
        }
    }
    private var bubble: some View {
        VStack(alignment: .leading, spacing: 4) {
            if recoveredBeforeReconnect {
                Text("Delivery unconfirmed before reconnect")
                    .font(.caption).foregroundStyle(.secondary)
                    .accessibilityIdentifier("recovered-message-status")
                Button("Copy message", systemImage: "doc.on.doc") { UIPasteboard.general.string = row.text }
                    .font(.caption)
                    .accessibilityIdentifier("copy-recovered-message")
            }
            if let providerNote, !row.isUser {
                Text(providerNote)
                    .font(.caption.weight(.medium)).foregroundStyle(.secondary)
                    .accessibilityIdentifier("message-provider")
            }
            Group {
                if row.isUser {
                    VStack(alignment: .leading, spacing: 4) {
                        if let source = row.source, let label = source.fromLabel {
                            // Another thread or an automation wrote this; the owner did not.
                            Label(label, systemImage: source.symbolName)
                                .font(.caption.weight(.medium)).foregroundStyle(.secondary)
                                .lineLimit(1)
                                .accessibilityIdentifier("message-source-label")
                        }
                        if !attachments.isEmpty, let imagePreviews, let loadRemoteData {
                            MessageAttachmentStrip(
                                attachments: attachments,
                                imagePreviews: imagePreviews,
                                imagePreviewScope: imagePreviewScope,
                                chatID: chatID,
                                loadRemoteData: loadRemoteData,
                                openImage: openImage,
                                openDocument: openDocument
                            )
                        }
                        if let block = row.commandBlock { CommandBlockCard(block: block) }
                        else if !row.text.isEmpty { AgentMessageText(text: row.text) }
                    }
                }
                else if !row.text.isEmpty {
                    if let expansion {
                        VStack(alignment: .leading, spacing: 4) {
                            if let nativeReplyRevision {
                                NativeAgentMessageText(revision: nativeReplyRevision, onDraft: draftNativeReply)
                            } else { AgentMessageText(text: row.text) }
                            ExpandMessageButton(kind: expansion) { expanded = true }
                        }
                    } else { AgentMessageText(text: row.text) }
                }
            }.font(bubbleFont)
                .onGeometryChange(for: CGFloat?.self) { proxy in
                    proxy.size.height > messageLiftLimit ? proxy.size.width.rounded() : nil
                } action: { tallPreviewWidth = $0 }
                .modifier(ChatBubbleSurface(isUser: row.isUser))
                // Lift only the bubble; without this the preview is the row's wider frame.
                .contentShape(.contextMenuPreview, ChatBubbleSurface.shape)
                .modifier(MessageMenuPreview(tallWidth: tallPreviewWidth, isUser: row.isUser, font: bubbleFont,
                                             text: row.text) { messageMenu })
        }
            .accessibilityElement(children: expansion == nil && row.attachmentIds.isEmpty && !recoveredBeforeReconnect && row.commandBlock == nil ? .combine : .contain)
            .accessibilityLabel(hasNativeReply ? "\(row.author), interactive reply" : row.isUser ? (row.source?.fromLabel.map { "\($0): " } ?? "") + ChatPlainText.of(row.text, style: markdownStyle) : "\(row.author): \(ChatPlainText.of(row.text, style: markdownStyle))")
            .accessibilityIdentifier("message:" + row.id)
            .accessibilityAction(named: "Copy message") { if !row.text.isEmpty { UIPasteboard.general.string = row.text } }
            .sheet(isPresented: $selectingText) { MessageTextSelectionSheet(text: row.text) }
            .sheet(isPresented: $expanded) {
                if let expansion {
                    ExpandedMessageSheet(title: row.author, content: expansion == .page ? .page(row.text, fence: true)
                                            : .reply(nativeReplyRevision ?? NativeReplyRevision(row.text)),
                                         onDraft: draftNativeReply)
                }
            }
    }
    private var bubbleFont: Font { typography.font(.body) }
    private var markdownStyle: MessageMarkdown.Style { .init(code: typography.code) }
    @ViewBuilder private var messageMenu: some View {
        MessageTextActions(text: row.text, selectingText: $selectingText, offersCopy: !hasReplyFooter)
        if !row.text.isEmpty, !row.isUser, row.commandBlock == nil {
            ReadAloudMenuItem(key: SpeechReader.key(chat: chatID, message: row.id), text: row.text)
        }
        if let forkFromHere {
            Button("Fork from here", systemImage: "arrow.triangle.branch", action: forkFromHere)
                .accessibilityIdentifier("fork-from-here")
        }
        if let editFromHere {
            Button("Edit from here", systemImage: "arrow.uturn.backward", action: editFromHere)
                .accessibilityIdentifier("edit-from-here")
        }
    }
    /// Copy lives in the reply footer when it shows, not in the menu; Fork stays here.
    private var hasReplyFooter: Bool { finishedLabel != nil && !row.text.isEmpty }
    private var hasNativeReply: Bool { nativeReplyRevision != nil }
}

/// "9:41 AM" / "Yesterday 9:41 AM" / "Oct 27, 9:41 AM", formatted once when the row is built.
enum ReplyFinishedTime {
    static func label(_ iso: String, now: Date = Date(), calendar: Calendar = .current) -> String {
        let numeric = Double(iso).map { Date(timeIntervalSince1970: $0 > 100_000_000_000 ? $0 / 1000 : $0) }
        guard let date = numeric ?? (try? Date(iso, strategy: .iso8601.year().month().day().time(includingFractionalSeconds: true)))
                ?? (try? Date(iso, strategy: .iso8601)) else { return "" }
        let time = date.formatted(date: .omitted, time: .shortened)
        if calendar.isDate(date, inSameDayAs: now) { return time }
        if calendar.isDateInYesterday(date) { return "Yesterday \(time)" }
        return date.formatted(.dateTime.month(.abbreviated).day()) + ", " + time
    }
}

/// The quiet line under a finished reply: Copy leads, the finish time trails.
private struct ReplyFooter: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.dynamicTypeSize) private var typeSize
    let time: String
    let text: String
    @State private var copied = false
    var body: some View {
        let layout = typeSize.isAccessibilitySize
            ? AnyLayout(VStackLayout(alignment: .leading, spacing: 0))
            : AnyLayout(HStackLayout(spacing: 0))
        layout {
            actions
            if !typeSize.isAccessibilitySize { Spacer(minLength: 8) }
            if !time.isEmpty {
                Text(time).fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: typeSize.isAccessibilitySize ? .infinity : nil, alignment: .trailing)
                    .accessibilityIdentifier("reply-time")
            }
        }
        .font(.caption).foregroundStyle(.secondary)
        // The 44 pt targets stay in the existing footer, with the time below
        // them at accessibility sizes so the named action stays readable.
        .padding(.vertical, typeSize.isAccessibilitySize ? 0 : -12)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var actions: some View {
        HStack(spacing: 0) {
            Button {
                UIPasteboard.general.string = text
                withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.15)) { copied = true }
                Task {
                    try? await Task.sleep(for: .seconds(1.5))
                    withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.15)) { copied = false }
                }
            } label: {
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
                    .frame(width: 44, height: 44)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Copy reply")
            .accessibilityValue(copied ? "Copied" : "")
            .accessibilityIdentifier("copy-reply")
        }
    }
}

/// Copy and Select text, shared by replies and the agent's interim notes.
private struct MessageTextActions: View {
    let text: String
    @Binding var selectingText: Bool
    var offersCopy = true
    var body: some View {
        if !text.isEmpty {
            // The row holds the whole message, so Copy never depends on
            // what the bubble currently shows.
            if offersCopy {
                Button("Copy", systemImage: "doc.on.doc") { UIPasteboard.general.string = text }
                    .accessibilityIdentifier("copy-message")
            }
            Button("Select text", systemImage: "text.cursor") { selectingText = true }
                .accessibilityIdentifier("select-message-text-action")
        }
    }
}

/// Read aloud, or Stop reading while this reply is read. Only this item
/// observes the reader, so rows never re-render as reading starts and ends.
private struct ReadAloudMenuItem: View {
    let key: String
    let text: String
    @ObservedObject private var reader = SpeechReader.shared

    var body: some View {
        if reader.readingID == key {
            Button("Stop reading", systemImage: "stop.circle") { reader.stop() }
                .accessibilityIdentifier("stop-reading-message")
        } else {
            Button { reader.read(text, id: key) } label: {
                Label("Read aloud", systemImage: "speaker.wave.2")
                if let reason = reader.unavailableReason { Text(reason) }
            }
            .disabled(reader.unavailableReason != nil)
            .accessibilityIdentifier("read-message-aloud")
        }
    }
}

/// The agent's interim note between steps: smaller and secondary to its reply,
/// with the reply's Copy and Select text. Only the text shows here: the note's
/// files and pictures are their own timeline items, and Fork and Edit stay on
/// finished replies and the owner's messages.
struct CommentaryBubble: View {
    @Environment(\.wonderTypography) private var typography
    let text: String
    let author: String
    @State private var selectingText = false
    @State private var tallPreviewWidth: CGFloat?
    var body: some View {
        AgentMessageText(text: text, textStyle: .subheadline)
            .font(font)
            .foregroundStyle(.secondary)
            .onGeometryChange(for: CGFloat?.self) { proxy in
                proxy.size.height > messageLiftLimit ? proxy.size.width.rounded() : nil
            } action: { tallPreviewWidth = $0 }
            .modifier(ChatBubbleSurface())
            .contentShape(.contextMenuPreview, ChatBubbleSurface.shape)
            .modifier(MessageMenuPreview(tallWidth: tallPreviewWidth, isUser: false, font: font, textStyle: .subheadline,
                                         text: text) { MessageTextActions(text: text, selectingText: $selectingText) })
            .accessibilityElement(children: .combine)
            .accessibilityLabel(author + ": " + ChatPlainText.of(text, style: .init(code: typography.code)))
            .accessibilityAction(named: "Copy message") { if !text.isEmpty { UIPasteboard.general.string = text } }
            .accessibilityIdentifier("commentary-message")
            .sheet(isPresented: $selectingText) { MessageTextSelectionSheet(text: text) }
    }
    private var font: Font { typography.font(.subheadline) }
}

/// Parsing belongs to the worker actor, keyed by exact message text. A changed
/// revision never keeps the previous revision's controls active while preparing.
private struct NativeAgentMessageText: View {
    let revision: NativeReplyRevision
    let onDraft: ((String) -> String?)?
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    @State private var preparation = NativeReplyPreparation()

    var body: some View {
        NativeReplyBody(preparation: preparation, requestedRevision: revision, onDraft: onDraft, theme: theme.nativeUI,
                        fonts: typography.nativeUI) { source in
            AgentMessageText(text: source)
        }
        .task(id: revision) {
            preparation.begin(revision)
            let value = await NativeReplyCache.shared.projection(for: revision.source)
            guard !Task.isCancelled else { return }
            preparation.finish(value, for: revision)
        }
    }
}

enum ExpandedMessageKind { case interactive, page }

private struct ExpandMessageButton: View {
    let kind: ExpandedMessageKind
    let action: () -> Void
    var body: some View {
        Button(action: action) {
            Label(kind == .page ? "View page" : "Expand",
                  systemImage: kind == .page ? "doc.richtext" : "arrow.up.left.and.arrow.down.right")
                .font(.subheadline.weight(.medium))
                .frame(minHeight: 44)
        }
        .buttonStyle(.borderless)
        .accessibilityHint(kind == .page ? "Opens the page full screen as text, tables and charts."
                                         : "Opens this reply full screen.")
        .accessibilityIdentifier(kind == .page ? "html-page-open" : "genui-expand")
    }
}

/// One agent message, or one HTML file, full screen: scrollable at any text
/// size, closed with a swipe down. A draft from a reply's controls closes the
/// sheet so the composer it filled is visible.
struct ExpandedMessageSheet: View {
    enum Content {
        case reply(NativeReplyRevision)
        /// `fence` reads the first ```html block of a reply; otherwise the text is the page.
        case page(String, fence: Bool)
    }
    let title: String
    let content: Content
    var onDraft: ((String) -> String?)? = nil
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Group {
                switch content {
                case .reply(let revision):
                    ScrollView {
                        NativeAgentMessageText(revision: revision, onDraft: onDraft.map { draft in
                            { text in
                                let failure = draft(text)
                                if failure == nil { dismiss() }
                                return failure
                            }
                        })
                        .frame(maxWidth: 760, alignment: .leading)
                        .padding()
                        .frame(maxWidth: .infinity)
                    }
                case .page(let source, let fence):
                    HTMLPageView(source: source, fence: fence)
                }
            }
            .wonderPage()
            .accessibilityIdentifier("expanded-message")
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
        }
        .presentationDetents([.large])
        .presentationDragIndicator(.hidden)
        .presentationSizing(.page)
    }
}

/// An HTML page in its offline browser sandbox, or a structured JSON visual
/// drawn with native components. Preparation stays on the cache actor.
struct HTMLPageView: View {
    let source: String
    let fence: Bool
    @Environment(\.wonderTheme) private var theme
    @Environment(\.wonderTypography) private var typography
    @State private var prepared: HTMLVisualCache.Prepared?
    @State private var finished = false

    var body: some View {
        Group {
            if let prepared {
                if prepared.rendersHTML {
                    HTMLArtifactView(html: prepared.html)
                        .padding(.horizontal, 16)
                } else if prepared.page.isEmpty {
                    ContentUnavailableView("Nothing to show here", systemImage: "doc.richtext",
                        description: Text("This page has no text, tables or charts Cupola can show."))
                } else {
                    ScrollView {
                        HTMLVisualPageView(page: prepared.page, theme: theme.nativeUI, fonts: typography.nativeUI,
                                           drawing: { AnyView(HTMLDrawingView(drawing: $0)) }) { text in
                            AgentMessageText(text: text)
                        }
                        .frame(maxWidth: 760, alignment: .leading)
                        .padding()
                        .frame(maxWidth: .infinity)
                    }
                }
            } else if finished {
                ContentUnavailableView("Page not ready", systemImage: "doc.richtext",
                    description: Text("The page is still being written. Open it again when the reply finishes."))
            } else {
                ProgressView("Preparing page").frame(maxHeight: .infinity)
            }
        }
        .frame(maxHeight: .infinity)
        .task(id: NativeReplyRevision(source)) {
            finished = false
            let value = await HTMLVisualCache.shared.prepare(source, fence: fence)
            guard !Task.isCancelled else { return }
            prepared = value
            finished = true
        }
    }
}

/// Under a message the Mac did not deliver: that it was not sent, why, and a
/// Retry that sends the same message again.
struct UndeliveredNotice: View {
    let undelivered: UndeliveredMessage
    let canRetry: Bool
    let retry: (String) async throws -> Void
    @State private var retrying = false
    @State private var failure: String?
    var body: some View {
        VStack(alignment: .trailing, spacing: 4) {
            Label {
                Text(undelivered.explanation == nil ? "Not sent" : "Not sent. ").fontWeight(.semibold)
                    + Text(undelivered.explanation ?? "")
            } icon: {
                Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.red)
            }
            .font(.caption)
            .multilineTextAlignment(.trailing)
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("undelivered-reason")
            if let failure {
                Text(failure).font(.caption).foregroundStyle(.red)
                    .multilineTextAlignment(.trailing)
                    .accessibilityIdentifier("undelivered-retry-failure")
            }
            Button {
                retrying = true; failure = nil
                Task {
                    do { try await retry(undelivered.messageId) }
                    catch { failure = (error as? LocalizedError)?.errorDescription ?? "Couldn’t send it again." }
                    retrying = false
                }
            } label: {
                if retrying { ProgressView().controlSize(.small) } else { Text("Try again") }
            }
            .font(.caption.weight(.semibold))
            .buttonStyle(.bordered)
            .disabled(!canRetry || retrying)
            .accessibilityLabel(retrying ? "Sending message again" : "Try again")
            .accessibilityHint(canRetry ? "Sends the same message again without creating a duplicate." : "Reconnect to your computer to try again.")
            .accessibilityIdentifier("undelivered-retry")
        }
        .frame(maxWidth: 320, alignment: .trailing)
    }
}

/// The whole message in a selectable text view, so part of it can be selected and
/// copied now that a long-press on the bubble opens the context menu.
struct MessageTextSelectionSheet: View {
    let text: String
    @Environment(\.dismiss) private var dismiss
    @Environment(\.wonderTheme) private var theme
    /// Bounds the text view for a very long message; the full text stays on Copy.
    private static let limit = 60_000
    var body: some View {
        NavigationStack {
            ScrollView {
                Text(String(text.prefix(Self.limit)))
                    .font(.body)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding()
                    .accessibilityIdentifier("select-message-text")
                if text.count > Self.limit {
                    Text("Showing the first part of a long message. Copy has all of it.")
                        .font(.footnote).foregroundStyle(.secondary).padding([.horizontal, .bottom])
                }
            }
            .wonderPage()
            .navigationTitle("Select text")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() }.accessibilityIdentifier("select-message-done") } }
        }
        .presentationDetents([.medium, .large]).presentationDragIndicator(.hidden)
    }
}

/// The long-press menu of a message. iOS shrinks a lifted view that does not fit
/// above the menu (a pasted handoff became an unreadable sliver, and was squashed
/// on the Duo), so a taller bubble shows its opening at its own width instead.
private let messageLiftLimit: CGFloat = 360
/// The opening of a taller message, small enough for the iPad's preview box.
private let messageTallPreviewWidth: CGFloat = 320
private let messageTallPreviewHeight: CGFloat = 220

private struct MessageMenuPreview<Menu: View>: ViewModifier {
    let tallWidth: CGFloat?
    let isUser: Bool
    let font: Font
    var textStyle: Font.TextStyle = .body
    let text: String
    @ViewBuilder let menu: () -> Menu

    func body(content: Content) -> some View {
        if let tallWidth {
            content.contextMenu(menuItems: menu) {
                // An exact size the iPad does not resize: it narrows a wider preview
                // to about 340 pt but lays it out unscaled, cropping it around its middle.
                AgentMessageText(text: text, textStyle: textStyle)
                    .font(font)
                    .frame(width: min(tallWidth, messageTallPreviewWidth), alignment: .leading)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(width: min(tallWidth, messageTallPreviewWidth), height: messageTallPreviewHeight, alignment: .top)
                    .clipped()
                    .mask(LinearGradient(stops: [.init(color: .black, location: 0.85), .init(color: .clear, location: 1)],
                                         startPoint: .top, endPoint: .bottom))
                    .modifier(ChatBubbleSurface(isUser: isUser))
            }
        } else {
            content.contextMenu(menuItems: menu)
        }
    }
}

struct ChatBubbleSurface: ViewModifier {
    static let shape = RoundedRectangle(cornerRadius: 16, style: .continuous)
    var isUser = false
    @Environment(\.wonderTheme) private var theme
    func body(content: Content) -> some View {
        content.modifier(PreparedBubbleSurface(
            foreground: theme.isDefault ? Color.primary : theme.primaryText,
            background: theme.isDefault ? Color(uiColor: isUser ? .systemGray4 : .secondarySystemBackground) : theme.bubble(isUser: isUser)))
    }
}

/// A plan the agent proposed, shown as the answer to review rather than as
/// work. The newest finished plan in a plan-mode thread can be implemented.
struct PlanCard: View {
    @Environment(\.wonderTheme) private var theme
    let text: String?
    let isRunning: Bool
    let canImplement: Bool
    let isImplementing: Bool
    let implement: () -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label("Plan", systemImage: "list.bullet.clipboard")
                .font(.subheadline.weight(.semibold)).foregroundStyle(.secondary)
                .accessibilityAddTraits(.isHeader)
            if let text { AgentMessageText(text: text) }
            else if isRunning { ProgressView().accessibilityLabel("Writing the plan") }
            if canImplement || isImplementing {
                Button(action: implement) {
                    HStack(spacing: 8) {
                        if isImplementing { ProgressView() }
                        Text("Implement plan")
                    }.frame(maxWidth: .infinity).frame(minHeight: 32)
                }
                .buttonStyle(.wonderProminent)
                .disabled(isImplementing)
                .accessibilityIdentifier("implement-plan")
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(theme.surface, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("plan-card")
    }
}

/// What a message says in plain words, for VoiceOver and the other
/// accessibility consumers: the same preparation the bubble draws from, so a
/// message is parsed once however many readers it has.
@MainActor enum ChatPlainText {
    static func of(_ text: String) -> String { of(text, style: .standard) }
    static func of(_ text: String, style: MessageMarkdown.Style) -> String {
        MessageMarkdown.prepared(text, style: style).plain
    }
}

/// Replies kept for their next streamed update (spoken and rendered text).
/// Bounded by count and source size, newest kept first; dropped on a memory
/// warning and when a conversation closes.
enum GrowingReplies {
    static let maximumCount = 4
    static let sourceBudget = 256 * 1_024
    /// Newest first. The newest entry always stays; older ones while they fit.
    static func trim<Entry>(_ entries: inout [Entry], source: (Entry) -> String) {
        var bytes = 0, kept = 0
        for entry in entries {
            bytes += source(entry).utf8.count
            guard kept == 0 || (kept < maximumCount && bytes <= sourceBudget) else { break }
            kept += 1
        }
        entries.removeSubrange(kept...)
    }
    @MainActor static func removeAll() { MessageMarkdown.removeGrowing() }
}

struct AgentMessageText: View {
    let text: String
    @Environment(\.wonderTypography) private var typography
    @Environment(\.wonderTheme) private var theme
    /// A Markdown file's Preview keeps relative links (`docs/USAGE.md`); its
    /// viewer resolves them inside the project. Messages keep web links only.
    var keepsRelativeLinks = false
    /// The size of plain paragraphs; headings keep their own sizes.
    var textStyle: Font.TextStyle = .body
    private typealias TextStyle = MessageMarkdown.TextStyle
    private var prepared: MessageMarkdown.Prepared {
        MessageMarkdown.prepared(text, style: .init(code: typography.code, keepsRelativeLinks: keepsRelativeLinks))
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(prepared.blocks.enumerated()), id: \.offset) { _, block in
                switch block {
                case .text(let value, let style):
                    Text(value).font(font(style)).textSelection(.enabled)
                        .accessibilityAddTraits(style == .body ? [] : .isHeader)
                case .item(let marker, let depth, let value):
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(marker).monospacedDigit().foregroundStyle(.secondary)
                        Text(value).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .padding(.leading, CGFloat(depth) * 16)
                    .accessibilityElement(children: .combine)
                case .quote(let value):
                    HStack(spacing: 8) {
                        Capsule().fill(.secondary.opacity(0.4)).frame(width: 3)
                        Text(value).foregroundStyle(.secondary).textSelection(.enabled)
                    }
                    .fixedSize(horizontal: false, vertical: true)
                case .code(let language, let code):
                    CodeBlockView(code: code, language: language)
                case .table(let table):
                    NativeTableView(table: table, theme: theme.nativeUI, font: typography.font(.subheadline))
                case .rule:
                    Divider()
                }
            }
        }
    }
    private func font(_ style: TextStyle) -> Font {
        switch style {
        case .body: typography.font(textStyle)
        case .heading1: typography.font(.title3).weight(.semibold)
        case .heading2: typography.font(.headline)
        case .heading3: typography.font(.subheadline).weight(.semibold)
        }
    }
}


/// A message's Markdown, prepared once: each block with its inline text
/// attributed, and what the message says in plain words (the accessibility
/// label), derived from those same attributed strings. Preparation is a pure,
/// thread-safe function of the text, so it can run off the main thread; the
/// views only read the result.
enum MessageMarkdown {
    enum Block: Sendable {
        case text(AttributedString, style: TextStyle)
        case item(marker: String, depth: Int, AttributedString)
        case quote(AttributedString)
        case code(language: String?, text: String)
        case table(NativeTable)
        case rule
    }
    enum TextStyle: Sendable { case body, heading1, heading2, heading3 }

    struct Prepared: Sendable {
        let blocks: [Block]
        let plain: String
        fileprivate let progress: MarkdownBlock.Progress
        fileprivate let parts: [String?]
    }

    /// What the inline text depends on besides the message: the code font of
    /// inline code and whether relative links survive.
    struct Style: Hashable, Sendable {
        var code: CodeFont = .system
        var keepsRelativeLinks = false
        static let standard = Style()
        init(code: CodeFont = .system, keepsRelativeLinks: Bool = false) {
            self.code = code; self.keepsRelativeLinks = keepsRelativeLinks
        }
        fileprivate var key: String { code.rawValue + "|" + (keepsRelativeLinks ? "relative:" : "") }
    }

    private final class Box: Sendable { let value: Prepared; init(_ value: Prepared) { self.value = value } }
    /// Settled texts, bounded by bytes only. A scrolled-away row re-prepares
    /// on its way back without this; see the numbers in the unit report.
    nonisolated(unsafe) private static let settled: NSCache<NSString, Box> = {
        let cache = NSCache<NSString, Box>(); cache.totalCostLimit = 2_000_000; return cache
    }()
    /// Replies that grew while shown (streaming), newest first. Each update
    /// continues from the last finished block and keeps those blocks' prepared
    /// text; only a text seen whole enters `settled`.
    private static let growing = OSAllocatedUnfairLock(initialState: [(style: String, prepared: Prepared)]())

    static func prepared(_ text: String, style: Style = .standard) -> Prepared {
        let key = (style.key + text) as NSString
        if let hit = settled.object(forKey: key) { return hit.value }
        let previous: Prepared? = growing.withLock { entries in
            if let same = entries.first(where: { $0.style == style.key && $0.prepared.progress.source == text }) { return same.prepared }
            guard let earlier = entries.firstIndex(where: { $0.style == style.key && text.utf8.starts(with: $0.prepared.progress.source.utf8) }) else { return nil }
            return entries.remove(at: earlier).prepared
        }
        let value: Prepared
        if let previous, previous.progress.source == text {
            value = previous
        } else {
            let probe = MarkdownProbe.begin(previous == nil ? "cold" : "delta", bytes: text.utf8.count)
            value = prepare(text, style: style, continuing: previous)
            probe.end()
            if previous == nil { settled.setObject(Box(value), forKey: key, cost: key.length) }
        }
        growing.withLock { entries in
            entries.insert((style.key, value), at: 0)
            GrowingReplies.trim(&entries) { $0.prepared.progress.source }
        }
        return value
    }

    static func removeGrowing() { growing.withLock { $0 = [] } }
    static var growingCount: Int { growing.withLock { $0.count } }

    private static func prepare(_ text: String, style: Style, continuing previous: Prepared?) -> Prepared {
        let codeFont = style.code == .system ? nil : WonderTypography(message: .system, code: style.code).codeFont(.body)
        let progress = MarkdownBlock.parse(text, continuing: previous?.progress)
        var parts: [String?] = []
        let blocks = progress.blocks.enumerated().map { index, block -> Block in
            if let previous, index < previous.blocks.count, previous.progress.blocks[index] == block {
                parts.append(previous.parts[index])
                return previous.blocks[index]
            }
            func inline(_ source: String) -> AttributedString {
                Self.inline(source, codeFont: codeFont, keepsRelativeLinks: style.keepsRelativeLinks)
            }
            func said(_ value: AttributedString) -> String { String(value.characters) }
            switch block {
            case .paragraph(let text):
                let value = inline(text); parts.append(said(value)); return .text(value, style: .body)
            case .heading(let level, let text):
                let value = inline(text); parts.append(said(value))
                return .text(value, style: level == 1 ? .heading1 : level == 2 ? .heading2 : .heading3)
            case .listItem(let marker, let depth, let text):
                let value = inline(text); parts.append((marker == "•" ? "" : marker + " ") + said(value))
                return .item(marker: marker, depth: depth, value)
            case .quote(let text):
                let value = inline(text); parts.append(said(value)); return .quote(value)
            case .code(let language, let text):
                parts.append(text); return .code(language: language, text: text)
            case .table(let header, let rows):
                let cells = ([header] + rows).map { $0.map(inline) }
                parts.append(cells.map { $0.map(said).joined(separator: ", ") }.joined(separator: "\n"))
                return .table(NativeTable(header: cells[0], rows: Array(cells.dropFirst())))
            case .rule:
                parts.append(nil); return .rule
            }
        }
        return Prepared(blocks: blocks, plain: parts.compactMap { $0 }.joined(separator: "\n"), progress: progress, parts: parts)
    }

    private static func inline(_ source: String, codeFont: Font?, keepsRelativeLinks: Bool) -> AttributedString {
        var value = (try? AttributedString(markdown: source, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))) ?? AttributedString(source)
        // Model text cannot turn a chat link into a local-file or app-control link.
        for run in Array(value.runs) {
            if let url = run.link, !["https", "http"].contains(url.scheme?.lowercased() ?? ""),
               !(keepsRelativeLinks && url.scheme == nil) {
                value[run.range].link = nil
            } else if run.link != nil {
                // Wonder's tint is neutral, so a link is marked by its underline.
                value[run.range].underlineStyle = .single
            }
            if let codeFont, run.inlinePresentationIntent?.contains(.code) == true {
                value[run.range].font = codeFont
            }
        }
        return value
    }
}

/// Main-thread cost of preparing a message's Markdown, as a `MarkdownPrepare`
/// signpost interval and one log line per call (Diagnostics builds only).
struct MarkdownProbe {
    #if WONDER_DIAGNOSTICS
    private static let signposter = OSSignposter(subsystem: "com.swaymun.wonder", category: .pointsOfInterest)
    private static let logger = Logger(subsystem: "com.swaymun.wonder", category: "markdown")
    private let kind: String, bytes: Int, start: ContinuousClock.Instant, cpuStart: Int
    private let state: OSSignpostIntervalState
    static func begin(_ kind: String, bytes: Int) -> MarkdownProbe {
        MarkdownProbe(kind: kind, bytes: bytes, start: .now, cpuStart: cpuMicros(),
                      state: signposter.beginInterval("MarkdownPrepare", id: signposter.makeSignpostID(), "\(kind) bytes=\(bytes)"))
    }
    /// This thread's CPU time, which a busy host does not inflate the way it does wall time.
    private static func cpuMicros() -> Int {
        var time = timespec(); clock_gettime(CLOCK_THREAD_CPUTIME_ID, &time)
        return time.tv_sec * 1_000_000 + time.tv_nsec / 1_000
    }
    func end() {
        Self.signposter.endInterval("MarkdownPrepare", state)
        let micros = (ContinuousClock.now - start).components.attoseconds / 1_000_000_000_000
        Self.logger.notice("markdown-prepare kind=\(kind, privacy: .public) bytes=\(bytes) us=\(micros) cpu=\(Self.cpuMicros() - cpuStart) main=\(Thread.isMainThread)")
    }
    #else
    static func begin(_ kind: String, bytes: Int) -> MarkdownProbe { MarkdownProbe() }
    func end() {}
    #endif
}
