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

struct ComposerAttachment: Identifiable, Sendable {
    let id: String
    let name: String
    let mimeType: String
    let data: Data?
    let remoteFile: ConversationFile?
    let sha256: String?
    let byteSize: Int?
    let state: String
    let updatedAt: String
    var source: AttachmentFileSource? = nil

    var isVideo: Bool { mimeType.hasPrefix("video/") }
    var showsImage: Bool { mimeType.hasPrefix("image/") }
    var isAnnotation: Bool { mimeType == ArtifactAnnotation.mimeType }
    var iconName: String {
        if isAnnotation { return "text.bubble" }
        if mimeType.hasPrefix("image/") { return "photo" }
        if isVideo { return "video" }
        if mimeType == "application/pdf" { return "doc.richtext" }
        if mimeType.hasPrefix("text/") { return "doc.text" }
        return "paperclip"
    }

    static let fallbackName = "Saved attachment"
}

struct ComposerAttachmentStrip: View {
    let attachments: [ComposerAttachment]
    let imagePreviews: ToolImagePreviews
    let imagePreviewScope: UUID
    let chatID: String
    var staleAnnotationIDs: Set<String> = []
    let removalDisabled: Bool
    let loadRemoteData: @Sendable (ConversationFile) async throws -> Data
    let openPhoto: (ComposerAttachment) -> Void
    let remove: (String) -> Void
    var editAnnotation: ((ComposerAttachment) -> Void)? = nil

    private var containsImage: Bool { attachments.contains { $0.showsImage } }

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(alignment: .center, spacing: 8) {
                ForEach(attachments) { attachment in
                    ComposerAttachmentItem(
                        attachment: attachment,
                        imagePreviews: imagePreviews,
                        imagePreviewScope: imagePreviewScope,
                        chatID: chatID,
                        isStale: staleAnnotationIDs.contains(attachment.id),
                        removalDisabled: removalDisabled,
                        loadRemoteData: loadRemoteData,
                        openPhoto: openPhoto,
                        remove: remove,
                        editAnnotation: editAnnotation
                    )
                }
            }
            .padding(.horizontal, 4)
            .padding(.vertical, 4)
        }
        .fixedSize(horizontal: false, vertical: true)
        .frame(minHeight: containsImage ? 88 : 52)
        .accessibilityIdentifier("composer-attachments")
    }
}

private struct ComposerAttachmentItem: View {
    @Environment(\.wonderTheme) private var theme
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    let attachment: ComposerAttachment
    let imagePreviews: ToolImagePreviews
    let imagePreviewScope: UUID
    let chatID: String
    let isStale: Bool
    let removalDisabled: Bool
    let loadRemoteData: @Sendable (ConversationFile) async throws -> Data
    let openPhoto: (ComposerAttachment) -> Void
    let remove: (String) -> Void
    let editAnnotation: ((ComposerAttachment) -> Void)?
    @State private var thumbnail: UIImage?
    @State private var failed = false
    @State private var videoDuration: String?

    private var thumbnailRequest: ToolImageRequest? {
        guard attachment.showsImage else { return nil }
        return ToolImageRequest(
            scope: imagePreviewScope,
            chatID: chatID,
            fileID: attachment.id,
            sha256: attachment.sha256,
            mimeType: attachment.mimeType,
            byteSize: attachment.byteSize,
            state: attachment.state,
            updatedAt: attachment.updatedAt,
            size: .chip
        )
    }
    private var removalLabel: String {
        "Remove \(attachment.isAnnotation ? "comment" : attachment.isVideo ? "video" : attachment.showsImage ? "photo" : "file") \(attachment.name)"
    }

    var body: some View {
        Group {
            if attachment.showsImage {
                imageTile
            } else {
                fileChip
            }
        }
        .accessibilityIdentifier("composer-attachment:\(attachment.id)")
        .accessibilityValue(attachment.showsImage ? (thumbnail != nil ? "Loaded" : failed ? "Unavailable" : "Loading") : "")
        .task(id: thumbnailRequest) { await loadThumbnail(thumbnailRequest) }
        .task(id: attachment.sha256) {
            guard attachment.isVideo, let source = attachment.source else { return }
            do {
                let poster = try await AttachmentVideoPosters.shared.poster(key: attachment.sha256 ?? attachment.id, source: source, mimeType: attachment.mimeType)
                try Task.checkCancellation()
                thumbnail = UIImage(cgImage: poster.image)
                videoDuration = poster.duration
            } catch { failed = true }
        }
        .onDisappear { thumbnail = nil }
    }

    private var imageTile: some View {
        ZStack(alignment: .topTrailing) {
            Button { openPhoto(attachment) } label: {
                ZStack {
                    theme.surface
                    if let thumbnail {
                        Image(uiImage: thumbnail)
                            .resizable()
                            .scaledToFill()
                            .frame(width: 72, height: 72)
                            .accessibilityHidden(true)
                    } else if failed {
                        Image(systemName: "photo")
                            .font(.title3)
                            .foregroundStyle(.secondary)
                            .accessibilityHidden(true)
                    } else {
                        ProgressView()
                            .accessibilityHidden(true)
                    }
                }
                .frame(width: 72, height: 72)
                .clipped()
            }
            .buttonStyle(.plain)
            .frame(width: 72, height: 72)
            .contentShape(Rectangle())
            .accessibilityLabel("Open photo \(attachment.name)")
            .accessibilityIdentifier("composer-attachment-open:\(attachment.id)")
            .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .strokeBorder(Color.primary.opacity(0.12), lineWidth: 1)
            }
            removeButton
                .offset(x: 8, y: -8)
        }
        .padding(.top, 8)
        .padding(.trailing, 8)
        .frame(width: 88, height: 88, alignment: .bottomLeading)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Photo \(attachment.name)")
        .accessibilityValue(thumbnail != nil ? "Loaded" : failed ? "Unavailable" : "Loading")
    }

    private var fileChip: some View {
        ZStack(alignment: .topTrailing) {
            let label = HStack(spacing: 6) {
                if attachment.isVideo, let thumbnail {
                    Image(uiImage: thumbnail).resizable().scaledToFill()
                        .frame(width: 48, height: 48).clipped().accessibilityHidden(true)
                } else {
                    Image(systemName: attachment.iconName).foregroundStyle(.secondary)
                }
                if attachment.isVideo {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(videoDuration ?? (failed ? "Preview unavailable" : "Preparing video…"))
                        Text(ByteCountFormatter.string(fromByteCount: Int64(attachment.byteSize ?? 0), countStyle: .file))
                    }.font(.caption).foregroundStyle(.secondary)
                }
                if isStale { Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange) }
                Text(attachment.isAnnotation ? "\(isStale ? "File changed · " : "Comment · ")\(attachment.name.replacingOccurrences(of: ".annotation.json", with: ""))" : attachment.name)
                    .font(.subheadline)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .frame(maxWidth: typeSize.isAccessibilitySize ? (horizontalSizeClass == .compact ? 180 : 320) : attachment.isVideo ? 160 : nil,
                           alignment: .leading)
                    .accessibilityLabel(attachment.name)
            }
            .padding(.leading, 10)
            .padding(.trailing, 24)
            .frame(minHeight: typeSize.isAccessibilitySize ? 64 : 44)
            .background(theme.chip, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .strokeBorder(Color.primary.opacity(0.12), lineWidth: 1)
            }
            if attachment.isAnnotation, let editAnnotation {
                Button { editAnnotation(attachment) } label: { label }
                    .buttonStyle(.plain)
                    .accessibilityLabel("\(isStale ? "File changed. Update comment" : "Edit comment") on \(attachment.name)")
                    .accessibilityIdentifier("composer-annotation-edit")
            } else { label }
            removeButton
                .offset(x: 8, y: -8)
        }
        .padding(.top, 8)
        .padding(.trailing, 8)
        .frame(minHeight: 52, alignment: .bottomLeading)
        .frame(maxWidth: typeSize.isAccessibilitySize ? (horizontalSizeClass == .compact ? 280 : 420) : nil)
        .accessibilityElement(children: .contain)
    }

    private var removeButton: some View {
        Button { remove(attachment.id) } label: {
            Image(systemName: "xmark.circle.fill")
                .font(.system(size: 17, weight: .semibold))
                .foregroundStyle(.primary)
                .frame(width: 24, height: 24)
                .background(.ultraThinMaterial, in: Circle())
                .frame(width: 44, height: 44)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(removalDisabled)
        .accessibilityLabel(removalLabel)
        .accessibilityIdentifier("composer-attachment-remove:\(attachment.id)")
    }

    private func loadThumbnail(_ request: ToolImageRequest?) async {
        thumbnail = nil
        failed = false
        guard let request else { return }
        let localData = attachment.data
        let remoteFile = attachment.remoteFile
        let remoteLoader = loadRemoteData
        do {
            let image = try await imagePreviews.image(for: request) {
                if let localData { return localData }
                guard let remoteFile else { throw FileFailure.unsupported }
                return try await remoteLoader(remoteFile)
            }
            try Task.checkCancellation()
            guard request == thumbnailRequest else { return }
            thumbnail = image
        } catch {
            guard !Task.isCancelled, request == thumbnailRequest else { return }
            failed = true
        }
    }
}
