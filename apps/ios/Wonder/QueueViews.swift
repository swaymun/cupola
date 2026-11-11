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

struct QueueDock: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let attachmentMetadata: [String: ConversationFile]
    let imagePreviews: ToolImagePreviews
    let imagePreviewScope: UUID
    let loadRemoteData: @Sendable (ConversationFile) async throws -> Data
    let openImage: (MessageAttachmentPresentation) -> Void
    let openDocument: ([String]) -> Void
    @State private var editing: QueuedMessage?
    @State private var text = ""
    @State private var busy: String?
    @State private var steering: String?
    @FocusState private var focused: Bool

    private var queue: [QueuedMessage] { model.queues[chat.id] ?? [] }

    var body: some View {
        VStack(spacing: 0) {
            ForEach(model.unlistedQueuedSends(chat.id), id: \.id) { item in
                // Keep the first placement while the computer returns its row.
                HStack(spacing: 8) {
                    Text(item.body).lineLimit(2).frame(maxWidth: .infinity, alignment: .leading)
                        .accessibilityLabel("Queued message: " + item.body)
                        .accessibilityIdentifier("queued-message:\(item.id)")
                    Text("Queued").font(.caption).foregroundStyle(.secondary)
                    ProgressView().frame(width: 44, height: 44).accessibilityLabel("Saving queued message")
                }
            }
            ForEach(queue) { item in
                HStack(spacing: 8) {
                    VStack(alignment: .leading, spacing: 4) {
                        if !item.attachmentIds.isEmpty {
                            MessageAttachmentStrip(
                                attachments: item.attachmentIds.map { MessageAttachmentPresentation(id: $0, file: attachmentMetadata[$0]) },
                                imagePreviews: imagePreviews, imagePreviewScope: imagePreviewScope, chatID: chat.id,
                                loadRemoteData: loadRemoteData, openImage: openImage,
                                openDocument: { openDocument(item.attachmentIds) })
                        }
                        Text(item.body).lineLimit(2)
                            .accessibilityLabel("Queued message: " + item.body)
                            .accessibilityIdentifier("queued-message:\(item.id)")
                    }.frame(maxWidth: .infinity, alignment: .leading)
                    Text(steering == item.id ? "Sending…" : "Queued").font(.caption).foregroundStyle(.secondary)
                    Menu { actions(item) } label: {
                        if busy == item.id { ProgressView().frame(width: 44, height: 44) }
                        else { Image(systemName: "ellipsis").frame(width: 44, height: 44).contentShape(Rectangle()) }
                    }
                    .menuOrder(.fixed)
                    .accessibilityLabel("Queued message actions")
                    .accessibilityIdentifier("queued-message-menu:\(item.id)")
                    .disabled(busy != nil)
                }
            }
        }
        .font(.subheadline)
        .frame(maxWidth: .infinity, alignment: .leading)
        .sheet(item: $editing) { item in
            NavigationStack {
                WonderForm {
                    if let error = model.controlErrors[chat.id] {
                        ComposerIssue(message: error, id: "queued-edit-issue") { model.dismissControlError(chat.id) }
                    }
                    TextField("Message", text: $text, axis: .vertical)
                        .lineLimit(2...8).focused($focused)
                        .accessibilityIdentifier("queued-message-draft")
                        .onChange(of: text) { _, value in model.typeAnswerDraft(["body": value], id: "queue-" + item.id) }
                }
                .navigationTitle("Edit queued message").navigationBarTitleDisplayMode(.inline)
                .wonderPage()
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Close") { model.flushAnswerDraft("queue-" + item.id); editing = nil; focused = false }
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Save") {
                            perform(item) {
                                try await model.changeQueue(chat, item: item, body: text)
                                model.saveAnswerDraft([:], id: "queue-" + item.id)
                                editing = nil; focused = false
                            }
                        }.disabled(text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || text.utf8.count > 65536
                                   || busy != nil || model.previewMode || model.accessEnded)
                    }
                }
            }
        }
        .onAppear {
            if model.previewMode, ProcessInfo.processInfo.arguments.contains("-queue-edit-preview"), let item = queue.first { edit(item); focused = false }
        }
    }

    @ViewBuilder private func actions(_ item: QueuedMessage) -> some View {
        let changesBlocked = model.previewMode || model.accessEnded
        Button("Send now", systemImage: "arrow.turn.up.right") {
            guard let turn = model.sendNowTurn(chat, item: item) else { return }
            steering = item.id
            perform(item) { try await model.changeQueue(chat, item: item, turn: turn) }
        }.disabled(changesBlocked || model.sendNowTurn(chat, item: item) == nil)
        Button("Edit", systemImage: "pencil") { edit(item) }.disabled(changesBlocked)
        Button("Remove", systemImage: "trash", role: .destructive) {
            perform(item) {
                try await model.changeQueue(chat, item: item, cancel: true)
                model.saveAnswerDraft([:], id: "queue-" + item.id)
            }
        }.disabled(changesBlocked)
    }
    private func edit(_ item: QueuedMessage) {
        text = model.answerDraft("queue-" + item.id)["body"] ?? item.body
        editing = item; focused = true
    }
    private func perform(_ item: QueuedMessage, action: @escaping () async throws -> Void) {
        guard busy == nil else { return }
        busy = item.id
        model.controlErrors[chat.id] = nil
        Task {
            defer { busy = nil; steering = nil }
            do { try await action() }
            catch {
                model.controlErrors[chat.id] = "The message may have changed or started. Reopen Edit to use the latest version. Your edit stays saved."
                try? await model.loadQueue(chat)
            }
        }
    }
}

/// Messages written while the Mac was unreachable, below the conversation:
/// never shown as sent. Waiting ones can be edited or cancelled from their
/// menu; a refused one says why and offers Retry, Edit and Cancel; one a
/// previous pairing wrote can only be copied or discarded.
struct OutboxDock: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary

    var body: some View {
        let items = model.composers[chat.id]?.outbox ?? []
        if !items.isEmpty {
            let device = model.connection?.credential.deviceId
            VStack(alignment: .trailing, spacing: 8) {
                ForEach(items) { item in
                    OutboxMessageRow(id: item.id, text: item.body, attachmentCount: item.attachmentCount,
                                     refusal: item.refusal, previousPairing: item.deviceId != device,
                                     sending: model.outboxInFlight.contains(item.id),
                                     edit: { model.editOutboxMessage(item.id, chat: chat) },
                                     cancel: { model.cancelOutboxMessage(item.id, chat: chat) },
                                     retry: { model.retryOutboxMessage(item.id, chat: chat) })
                }
            }
            .frame(maxWidth: .infinity, alignment: .trailing)
        }
    }
}

/// One message waiting for the Mac, in a thread or in New chat.
struct OutboxMessageRow: View {
    let id: String
    let text: String
    let attachmentCount: Int
    let refusal: String?
    let previousPairing: Bool
    let sending: Bool
    /// Where a waiting New chat goes (its Project).
    var destination: String? = nil
    var canEdit = true
    var canRetry = true
    let edit: () -> Void
    let cancel: () -> Void
    let retry: () -> Void

    private var status: String {
        let state = previousPairing ? "Belongs to a previous pairing"
            : refusal != nil ? "Not sent" : sending ? "Sending…" : "Waiting for your computer"
        return destination.map { state + " · " + $0 } ?? state
    }

    var body: some View {
        VStack(alignment: .trailing, spacing: 4) {
            Label(status, systemImage: previousPairing || refusal != nil ? "exclamationmark.circle" : "clock")
                .font(.caption).foregroundStyle(.secondary)
                .accessibilityIdentifier("outbox-status:\(id)")
            HStack(alignment: .bottom) {
                Spacer(minLength: 40)
                VStack(alignment: .leading, spacing: 4) {
                    if attachmentCount > 0 {
                        Label(attachmentCount == 1 ? "1 attachment" : "\(attachmentCount) attachments", systemImage: "paperclip")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    if !text.isEmpty { Text(text).lineLimit(6) }
                }
                .padding(12)
                .background(Color.secondary.opacity(0.10), in: RoundedRectangle(cornerRadius: 10))
                .contentShape(.contextMenuPreview, RoundedRectangle(cornerRadius: 10))
                .contextMenu { if !sending { actions } }
                .accessibilityElement(children: .combine)
                .accessibilityLabel(status + ": " + (text.isEmpty ? "message with attachments" : text))
                .accessibilityIdentifier("outbox-message:\(id)")
            }
            if let refusal, !previousPairing {
                Text(refusal).font(.caption).foregroundStyle(.secondary)
                    .multilineTextAlignment(.trailing)
                    .accessibilityIdentifier("outbox-refusal:\(id)")
            }
            if (previousPairing || refusal != nil) && !sending {
                HStack(spacing: 12) { actions }
                    .labelStyle(.titleOnly).font(.caption.weight(.semibold)).buttonStyle(.bordered)
            }
        }
        .frame(maxWidth: 640, alignment: .trailing)
    }

    @ViewBuilder private var actions: some View {
        if previousPairing {
            Button("Copy text", systemImage: "doc.on.doc") { UIPasteboard.general.string = text }
                .accessibilityIdentifier("outbox-copy:\(id)")
            Button("Discard", systemImage: "trash", role: .destructive, action: cancel)
                .accessibilityIdentifier("outbox-discard:\(id)")
        } else {
            if refusal != nil, canRetry {
                Button("Try again", systemImage: "arrow.clockwise", action: retry)
                    .accessibilityIdentifier("outbox-retry:\(id)")
            }
            if canEdit {
                Button("Edit message", systemImage: "pencil", action: edit)
                    .accessibilityIdentifier("outbox-edit:\(id)")
            }
            Button("Cancel message", systemImage: "trash", role: .destructive, action: cancel)
                .accessibilityIdentifier("outbox-cancel:\(id)")
        }
    }
}
