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

enum PhotoViewerRouting {
    nonisolated static func isImage(_ file: ConversationFile) -> Bool {
        isImage(mimeType: file.mimeType)
    }

    /// A raster image the photo viewer decodes. SVG draws in its sandbox instead.
    nonisolated static func isImage(mimeType: String?) -> Bool {
        guard let mime = mimeType?.lowercased() else { return false }
        return mime.hasPrefix("image/") && mime != "image/svg+xml"
    }
}

/// The verified original bytes, offered in their own image type so a share
/// target (Save Image, Messages, Files) receives the file unchanged.
struct PhotoShareItem: Transferable {
    let name: String
    let type: UTType
    let data: Data

    init?(name: String, mimeType: String, data: Data) {
        guard let type = UTType(mimeType: mimeType), type.conforms(to: .image) else { return nil }
        self.name = name; self.type = type; self.data = data
    }

    static var transferRepresentation: some TransferRepresentation {
        DataRepresentation(exportedContentType: .png) { $0.data }
            .exportingCondition { $0.type.conforms(to: .png) }.suggestedFileName { $0.name }
        DataRepresentation(exportedContentType: .jpeg) { $0.data }
            .exportingCondition { $0.type.conforms(to: .jpeg) }.suggestedFileName { $0.name }
        DataRepresentation(exportedContentType: .heic) { $0.data }
            .exportingCondition { $0.type.conforms(to: .heic) }.suggestedFileName { $0.name }
        DataRepresentation(exportedContentType: .gif) { $0.data }
            .exportingCondition { $0.type.conforms(to: .gif) }.suggestedFileName { $0.name }
        DataRepresentation(exportedContentType: .webP) { $0.data }
            .exportingCondition { $0.type.conforms(to: .webP) }.suggestedFileName { $0.name }
        DataRepresentation(exportedContentType: .image) { $0.data }.suggestedFileName { $0.name }
    }
}

private struct PhotoViewerActionStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .frame(minWidth: 44, minHeight: 44)
            .contentShape(Rectangle())
            .opacity(isEnabled ? (configuration.isPressed ? 0.5 : 1) : 0.4)
    }
}

private struct PhotoViewerRequest: Hashable, Sendable {
    let scope: UUID
    let sourceID: String
    let name: String
    let mimeType: String
    let state: String
    let revision: String
}

private struct PhotoViewerItem: Identifiable {
    let id: String
    let name: String
    let mimeType: String
    let sourceFile: ConversationFile?
    let localData: Data?
    var localRevision: String? = nil
}

private struct PhotoViewerPage: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    let item: PhotoViewerItem
    let onVerifiedData: (String, Data?) -> Void
    /// "Image 2 of 3" when the viewer pages through several pictures.
    var position: String? = nil
    /// Moves to the next (+1) or previous (-1) picture, for VoiceOver.
    var step: ((Int) -> Void)? = nil
    /// Paging waits until a zoomed picture is back at its full view.
    var onZoomed: (Bool) -> Void = { _ in }
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var image: UIImage?
    @State private var failure: String?
    @State private var zoomScale: CGFloat = 1
    @State private var loadedRequest: PhotoViewerRequest?

    private var request: PhotoViewerRequest {
        PhotoViewerRequest(
            scope: model.imagePreviewScope,
            sourceID: item.id,
            name: item.name,
            mimeType: item.mimeType,
            state: item.sourceFile?.state ?? "local",
            revision: item.localRevision ?? item.sourceFile?.updatedAt ?? "local"
        )
    }

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            if let image {
                PhotoZoomView(
                    image: image,
                    imageID: request.sourceID + ":" + (loadedRequest?.revision ?? request.revision),
                    zoomScale: $zoomScale,
                    reduceMotion: reduceMotion,
                    preservePosition: item.localRevision != nil
                )
                .accessibilityElement(children: .ignore)
                .accessibilityIdentifier("photo-viewer-image:\(item.id)")
                .accessibilityLabel(item.name)
                .accessibilityValue([position, "Zoom \(Int((zoomScale * 100).rounded()))%"].compactMap { $0 }.joined(separator: ", "))
                .accessibilityHint(step == nil
                    ? "Pinch to zoom. Double-tap to zoom or reset. Touch and hold the subject to lift it out, or use Share, Copy and Save below."
                    : "Swipe up or down with one finger for the next or previous image. Pinch to zoom. Double-tap to zoom or reset.")
                .accessibilityAdjustableAction { direction in
                    switch direction {
                    case .increment: step?(1)
                    case .decrement: step?(-1)
                    @unknown default: break
                    }
                }
                .accessibilityActions {
                    if let step {
                        Button("Next image") { step(1) }
                        Button("Previous image") { step(-1) }
                    }
                }
            } else if let failure {
                VStack(spacing: 12) {
                    Image(systemName: "photo")
                        .font(.largeTitle)
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                    Text("Photo unavailable")
                        .font(.headline)
                    Text(failure)
                        .font(.body)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(24)
                .foregroundStyle(.white)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("photo-viewer-error")
            } else {
                ProgressView("Loading photo…")
                    .tint(.white)
                    .foregroundStyle(.white)
                .accessibilityIdentifier("photo-viewer-loading")
            }
        }
        .task(id: request) { await load(request) }
        .onChange(of: zoomScale > 1.01) { _, zoomed in onZoomed(zoomed) }
        .onChange(of: request) { previous, next in
            if previous.sourceID != next.sourceID || previous.scope != next.scope {
                zoomScale = 1
                image = nil
            }
            failure = nil
        }
        .onDisappear {
            image = nil
            onVerifiedData(item.id, nil)
        }
    }

    private func load(_ request: PhotoViewerRequest) async {
        if loadedRequest?.sourceID != request.sourceID || loadedRequest?.scope != request.scope { image = nil }
        failure = nil
        guard PhotoViewerRouting.isImage(mimeType: request.mimeType) else {
            failure = "This file is not an image."
            return
        }
        if let sourceFile = item.sourceFile, sourceFile.state != "available" {
            failure = "This image is no longer available."
            return
        }
        do {
            let data: Data
            if let localData = item.localData {
                guard localData.count <= 8 * 1024 * 1024, !localData.isEmpty else { throw FileFailure.tooLarge }
                try ConversationFile.validateContent(localData, mime: request.mimeType)
                data = localData
            } else if let sourceFile = item.sourceFile {
                data = try await model.download(sourceFile, chat: chat)
            } else {
                throw FileFailure.integrity
            }
            try Task.checkCancellation()
            guard model.imagePreviewScope == request.scope else { return }
            let decodeTask: Task<UIImage?, Never> = Task.detached(priority: .userInitiated) {
                guard !Task.isCancelled else { return nil }
                return PhotoViewerImage.decode(data)
            }
            let decoded = await withTaskCancellationHandler(operation: {
                await decodeTask.value
            }, onCancel: {
                decodeTask.cancel()
            })
            try Task.checkCancellation()
            guard model.imagePreviewScope == request.scope else { return }
            guard let decoded else { throw FileFailure.unsupported }
            image = decoded
            loadedRequest = request
            onVerifiedData(item.id, data)
        } catch is CancellationError {
            // The viewer was dismissed or its conversation scope changed.
        } catch FileFailure.gone {
            guard !Task.isCancelled, model.imagePreviewScope == request.scope else { return }
            image = nil
            failure = "This picture is no longer on your computer. It was moved, changed or cleaned up there."
        } catch {
            guard !Task.isCancelled, model.imagePreviewScope == request.scope else { return }
            image = nil
            failure = "This image could not be verified or decoded. Close and try again."
            onVerifiedData(item.id, nil)
        }
    }
}

struct PhotoViewer: View {
    @ObservedObject var model: ConnectionModel
    let chat: ChatSummary
    private let items: [PhotoViewerItem]
    /// The picture in view; nil only while the pager settles between pages.
    @State private var position: Int?
    private let initialIndex: Int
    /// A zoomed picture keeps the pager still until it is zoomed back out.
    @State private var zoomedID: String?
    @Environment(\.dismiss) private var dismiss
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var onClose: (() -> Void)? = nil
    var showsHeader = true
    /// A control the host adds beside Share, Copy and Save, such as Comment.
    var accessory: AnyView? = nil
    @State private var verifiedData: [String: Data] = [:]
    @State private var copyStatus: String?
    @State private var saving = false

    init(model: ConnectionModel, chat: ChatSummary, file: ConversationFile, galleryFiles: [ConversationFile] = [], initialFileID: String? = nil,
         onClose: (() -> Void)? = nil, showsHeader: Bool = true) {
        self.model = model
        self.chat = chat
        self.onClose = onClose
        self.showsHeader = showsHeader
        let sources = galleryFiles.isEmpty ? [file] : ConversationAttachmentGallery.imageFiles(galleryFiles)
        let bounded = Self.boundedItems(sources.map { PhotoViewerItem(id: $0.id, name: $0.name, mimeType: $0.mimeType ?? "", sourceFile: $0, localData: nil) }, initialID: initialFileID ?? file.id)
        self.items = bounded.items
        initialIndex = bounded.index
        _position = State(initialValue: bounded.index)
    }

    init(model: ConnectionModel, chat: ChatSummary, galleryFiles: [ConversationFile], initialFileID: String) {
        self.model = model
        self.chat = chat
        let sources = ConversationAttachmentGallery.imageFiles(galleryFiles)
        let fallback = sources.first
        let bounded = Self.boundedItems(sources.map { PhotoViewerItem(id: $0.id, name: $0.name, mimeType: $0.mimeType ?? "", sourceFile: $0, localData: nil) }, initialID: initialFileID)
        self.items = bounded.items.isEmpty && fallback == nil ? [] : bounded.items
        initialIndex = bounded.index
        _position = State(initialValue: bounded.index)
    }

    init(model: ConnectionModel, chat: ChatSummary, name: String, mimeType: String, sourceID: String, localData: Data?, file: ConversationFile?, revision: String? = nil,
         onClose: (() -> Void)? = nil, showsHeader: Bool = true, accessory: AnyView? = nil) {
        self.model = model
        self.chat = chat
        self.onClose = onClose
        self.showsHeader = showsHeader
        self.accessory = accessory
        self.items = [PhotoViewerItem(id: sourceID, name: name, mimeType: mimeType, sourceFile: file, localData: localData, localRevision: revision)]
        initialIndex = 0
        _position = State(initialValue: 0)
    }

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            if items.isEmpty {
                ContentUnavailableView("Photo unavailable", systemImage: "photo", description: Text("This image is no longer available."))
                    .foregroundStyle(.white)
            } else {
                // Pictures shown together page sideways; each page loads its
                // picture when it comes near view and lets it go when it leaves.
                // Pages take the space between the header and the actions; the
                // scroll view itself reaches under them.
                GeometryReader { space in
                ScrollView(.horizontal) {
                    LazyHStack(spacing: 0) {
                        // The viewer's pictures never change while it is open, so a
                        // page's index is its identity and the pager's position.
                        ForEach(items.indices, id: \.self) { index in
                            let item = items[index]
                            PhotoViewerPage(model: model, chat: chat, item: item, onVerifiedData: { id, data in
                                if let data { verifiedData[id] = data } else { verifiedData.removeValue(forKey: id) }
                            },
                            position: items.count > 1 ? "Image \(index + 1) of \(items.count)" : nil,
                            step: items.count > 1 ? { move(by: $0, from: index) } : nil,
                            onZoomed: { zoomed in
                                if zoomed { zoomedID = item.id } else if zoomedID == item.id { zoomedID = nil }
                            })
                            .frame(width: space.size.width, height: space.size.height)
                        }
                    }
                    .scrollTargetLayout()
                }
                .scrollTargetBehavior(.paging)
                .scrollPosition(id: $position)
                .scrollIndicators(.hidden)
                .scrollDisabled(items.count < 2 || zoomedID != nil)
                .accessibilityIdentifier("photo-viewer-pager")
                }
            }
        }
        .overlay(alignment: .bottom) {
            // Feedback sits over the photo, leaving its viewport and controls fixed.
            if let copyStatus {
                Text(copyStatus)
                    .font(.footnote)
                    .foregroundStyle(.white)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 6)
                    .background(.black.opacity(0.6), in: Capsule())
                    .padding(6)
                    .accessibilityIdentifier("photo-viewer-copy-status")
            }
        }
        .safeAreaInset(edge: .top, spacing: 0) {
            if showsHeader {
                HStack(alignment: .top, spacing: 12) {
                if let item = currentItem {
                    Text(item.name)
                        .font(.headline)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)
                        .foregroundStyle(.white)
                        .accessibilityIdentifier("photo-viewer-name")
                }
                Spacer(minLength: 0)
                if items.count > 1 {
                    Text("\(currentIndex + 1) of \(items.count)")
                        .font(.subheadline.weight(.medium))
                        .foregroundStyle(.white)
                        .accessibilityIdentifier("photo-viewer-position")
                }
                Button { if let onClose { onClose() } else { dismiss() } } label: {
                    Image(systemName: "xmark")
                        .font(.body.weight(.semibold))
                        .frame(width: 44, height: 44)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .foregroundStyle(.white)
                .accessibilityLabel("Close photo viewer")
                .accessibilityIdentifier("photo-viewer-close")
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 4)
                .background(.black.opacity(0.78))
            }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if !items.isEmpty { actionBar }
        }
        .onChange(of: currentIndex) { _, _ in copyStatus = nil }
        .onDisappear {
            verifiedData.removeAll()
        }
    }

    private var currentIndex: Int { position ?? initialIndex }

    private var currentItem: PhotoViewerItem? {
        guard items.indices.contains(currentIndex) else { return nil }
        return items[currentIndex]
    }

    private func move(by delta: Int, from index: Int) {
        let next = index + delta
        guard items.indices.contains(next), zoomedID == nil else { return }
        withAnimation(reduceMotion ? nil : .default) { position = next }
    }

    /// Share, Copy and Save act on the verified original bytes, never on a
    /// re-encoded preview. Touch and hold the subject to lift it out.
    private var actionBar: some View {
        let shareable = currentItem.flatMap { item in
            verifiedData[item.id].flatMap { PhotoShareItem(name: item.name, mimeType: item.mimeType, data: $0) }
        }
        return HStack(spacing: 4) {
            if let shareable {
                ShareLink(item: shareable, preview: SharePreview(shareable.name)) {
                    Label("Share", systemImage: "square.and.arrow.up")
                }
                .accessibilityIdentifier("photo-viewer-share")
            } else {
                Label("Share", systemImage: "square.and.arrow.up")
                    .foregroundStyle(.white.opacity(0.4))
                    .accessibilityLabel("Share, available once the image loads")
                    .accessibilityIdentifier("photo-viewer-share")
            }
            Button("Copy", systemImage: "doc.on.doc") {
                if let id = currentItem?.id { copyImage(for: id) }
            }
            .disabled(shareable == nil)
            .accessibilityIdentifier("photo-viewer-copy")
            Button("Save to Photos", systemImage: saving ? "ellipsis" : "square.and.arrow.down") {
                if let id = currentItem?.id { saveImage(for: id) }
            }
            .disabled(shareable == nil || saving)
            .accessibilityIdentifier("photo-viewer-save")
            Spacer(minLength: 8)
            if let accessory { accessory }
        }
        .labelStyle(.iconOnly)
        .buttonStyle(PhotoViewerActionStyle())
        .font(.title3)
        .foregroundStyle(.white)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .frame(maxWidth: .infinity)
        .background(.black.opacity(0.78))
    }

    private func saveImage(for id: String) {
        guard let data = verifiedData[id], let item = items.first(where: { $0.id == id }) else {
            copyStatus = "Save unavailable"
            return
        }
        saving = true
        copyStatus = nil
        Task { @MainActor in
            defer { saving = false }
            let access = await PHPhotoLibrary.requestAuthorization(for: .addOnly)
            guard access == .authorized || access == .limited else {
                copyStatus = "To save images, allow Cupola to add photos in Settings."
                return
            }
            do {
                try await PHPhotoLibrary.shared().performChanges {
                    let options = PHAssetResourceCreationOptions()
                    options.originalFilename = item.name
                    PHAssetCreationRequest.forAsset().addResource(with: .photo, data: data, options: options)
                }
                copyStatus = "Saved to Photos"
            } catch {
                copyStatus = "The image could not be saved to Photos."
            }
        }
    }

    private func copyImage(for id: String) {
        guard let data = verifiedData[id], let item = items.first(where: { $0.id == id }),
              let type = UTType(mimeType: item.mimeType), type.conforms(to: .image) else {
            copyStatus = "Copy unavailable"
            return
        }
        UIPasteboard.general.setItems([[type.identifier: data]], options: [.expirationDate: Date().addingTimeInterval(300)])
        copyStatus = "Image copied"
    }

    private static func boundedItems(_ items: [PhotoViewerItem], initialID: String) -> (items: [PhotoViewerItem], index: Int) {
        guard items.count > 24 else {
            return (items, max(0, items.firstIndex(where: { $0.id == initialID }) ?? 0))
        }
        var bounded = Array(items.prefix(24))
        if let initial = items.first(where: { $0.id == initialID }), !bounded.contains(where: { $0.id == initialID }) {
            bounded[bounded.count - 1] = initial
        }
        return (bounded, bounded.firstIndex(where: { $0.id == initialID }) ?? 0)
    }

}

enum PhotoViewerImage {
    /// Full-screen viewing is bounded independently from the 1080px thumbnail.
    /// A 4096px maximum dimension keeps the decoded image near the 64 MiB budget;
    /// the original verified bytes remain available separately for Copy image.
    nonisolated static func decode(_ data: Data) -> UIImage? {
        autoreleasepool {
            guard data.count <= 8 * 1024 * 1024,
                  let source = CGImageSourceCreateWithData(data as CFData, [kCGImageSourceShouldCache: false] as CFDictionary),
                  let image = CGImageSourceCreateThumbnailAtIndex(source, 0, [
                    kCGImageSourceCreateThumbnailFromImageAlways: true,
                    kCGImageSourceCreateThumbnailWithTransform: true,
                    kCGImageSourceThumbnailMaxPixelSize: 4096,
                    kCGImageSourceShouldCacheImmediately: true
                  ] as CFDictionary),
                  Int64(image.width) * Int64(image.height) <= 16_777_216,
                  Int64(image.bytesPerRow) * Int64(image.height) <= 64 * 1024 * 1024 else { return nil }
            return UIImage(cgImage: image)
        }
    }
}

private struct PhotoZoomView: UIViewRepresentable {
    let image: UIImage
    let imageID: String
    @Binding var zoomScale: CGFloat
    let reduceMotion: Bool
    var preservePosition = false

    func makeCoordinator() -> Coordinator {
        Coordinator(zoomScale: $zoomScale, reduceMotion: reduceMotion)
    }

    func makeUIView(context: Context) -> PhotoZoomScrollView {
        let view = PhotoZoomScrollView()
        view.backgroundColor = .clear
        view.delegate = context.coordinator
        view.minimumZoomScale = 1
        view.maximumZoomScale = 4
        view.showsHorizontalScrollIndicator = false
        view.showsVerticalScrollIndicator = false
        // A sideways drag at full view belongs to the pager, not a bounce.
        view.alwaysBounceHorizontal = false
        view.alwaysBounceVertical = true
        let imageView = UIImageView(image: image)
        imageView.contentMode = .scaleAspectFit
        imageView.isUserInteractionEnabled = true
        imageView.isAccessibilityElement = false
        view.photoImageView = imageView
        view.addSubview(imageView)
        let doubleTap = UITapGestureRecognizer(target: context.coordinator, action: #selector(Coordinator.doubleTap(_:)))
        doubleTap.numberOfTapsRequired = 2
        view.addGestureRecognizer(doubleTap)
        context.coordinator.install(image: image, id: imageID, in: view)
        context.coordinator.attachAnalysis(to: imageView)
        return view
    }

    static func dismantleUIView(_ view: PhotoZoomScrollView, coordinator: Coordinator) {
        coordinator.cancelAnalysis()
    }

    func updateUIView(_ view: PhotoZoomScrollView, context: Context) {
        // Do not touch zoomScale or contentOffset for an ordinary SwiftUI update.
        // The source ID is the only event that starts a new viewing session.
        if view.imageID != imageID {
            context.coordinator.install(image: image, id: imageID, in: view, preservePosition: preservePosition)
        }
    }

    final class Coordinator: NSObject, UIScrollViewDelegate {
        private let zoomScale: Binding<CGFloat>
        private let reduceMotion: Bool
        private var isInstallingImage = false
        /// Subject lifting and Live Text run on VisionKit's own analysis; one
        /// request per shown image, cancelled when the image or view goes away.
        private var interaction: ImageAnalysisInteraction?
        private var analysis: Task<Void, Never>?

        func attachAnalysis(to imageView: UIImageView) {
            guard ImageAnalyzer.isSupported else { return }
            let interaction = ImageAnalysisInteraction()
            interaction.preferredInteractionTypes = []
            imageView.addInteraction(interaction)
            self.interaction = interaction
            if let image = imageView.image { analyze(image) }
        }

        func cancelAnalysis() {
            analysis?.cancel()
            analysis = nil
            interaction?.analysis = nil
        }

        private func analyze(_ image: UIImage) {
            guard let interaction else { return }
            analysis?.cancel()
            interaction.analysis = nil
            interaction.preferredInteractionTypes = []
            analysis = Task { @MainActor [weak interaction] in
                // ImageAnalyzer works on its own queue; the image is already
                // bounded to 4096 px by PhotoViewerImage.decode.
                let configuration = ImageAnalyzer.Configuration([.text, .visualLookUp])
                guard let result = try? await PhotoSubjectAnalyzer.shared.analyze(image, configuration: configuration),
                      !Task.isCancelled, let interaction else { return }
                interaction.analysis = result
                interaction.preferredInteractionTypes = [.imageSubject, .textSelection]
            }
        }

        init(zoomScale: Binding<CGFloat>, reduceMotion: Bool) {
            self.zoomScale = zoomScale
            self.reduceMotion = reduceMotion
        }

        func install(image: UIImage, id: String, in scrollView: PhotoZoomScrollView,
                     preservePosition: Bool = false) {
            isInstallingImage = true
            defer { isInstallingImage = false }
            let oldSize = scrollView.contentSize
            let oldCenter = CGPoint(x: (scrollView.contentOffset.x + scrollView.bounds.midX) / max(oldSize.width, 1),
                                    y: (scrollView.contentOffset.y + scrollView.bounds.midY) / max(oldSize.height, 1))
            let oldZoom = scrollView.zoomScale
            let changedImage = scrollView.photoImageView?.image !== image
            scrollView.imageID = id
            scrollView.photoImageView?.image = image
            if changedImage, interaction != nil { analyze(image) }
            scrollView.minimumZoomScale = 1
            scrollView.maximumZoomScale = 4
            scrollView.photoImageView?.frame = scrollView.bounds
            scrollView.contentSize = scrollView.bounds.size
            scrollView.setZoomScale(preservePosition ? min(4, max(1, oldZoom)) : 1, animated: false)
            if preservePosition {
                let width = max(0, scrollView.contentSize.width - scrollView.bounds.width)
                let height = max(0, scrollView.contentSize.height - scrollView.bounds.height)
                scrollView.contentOffset = CGPoint(
                    x: min(width, max(0, oldCenter.x * scrollView.contentSize.width - scrollView.bounds.midX)),
                    y: min(height, max(0, oldCenter.y * scrollView.contentSize.height - scrollView.bounds.midY)))
            }
        }

        func viewForZooming(in scrollView: UIScrollView) -> UIView? {
            (scrollView as? PhotoZoomScrollView)?.photoImageView
        }

        func scrollViewDidZoom(_ scrollView: UIScrollView) {
            guard !isInstallingImage, zoomScale.wrappedValue != scrollView.zoomScale else { return }
            zoomScale.wrappedValue = scrollView.zoomScale
        }

        @objc func doubleTap(_ recognizer: UITapGestureRecognizer) {
            guard let scrollView = recognizer.view as? PhotoZoomScrollView else { return }
            let target = scrollView.zoomScale > scrollView.minimumZoomScale + 0.01 ? scrollView.minimumZoomScale : min(2, scrollView.maximumZoomScale)
            scrollView.setZoomScale(target, animated: !reduceMotion)
        }
    }
}

/// One analyzer serves every viewer; VisionKit recommends reusing it.
@MainActor enum PhotoSubjectAnalyzer {
    static let shared = ImageAnalyzer()
}

private final class PhotoZoomScrollView: UIScrollView {
    var photoImageView: UIImageView?
    var imageID: String?

    override func layoutSubviews() {
        super.layoutSubviews()
        guard zoomScale <= minimumZoomScale + 0.01 else { return }
        photoImageView?.frame = bounds
        contentSize = bounds.size
    }
}
