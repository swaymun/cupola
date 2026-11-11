import Combine
import ImageIO
import PencilKit
import PhotosUI
import SwiftUI
import UniformTypeIdentifiers
import WonderPairing

enum SketchAttachmentResult: Sendable {
    case attached, cancelled
    case failed(String)
}

enum SketchFailure: LocalizedError {
    case invalidImage, invalidCrop, tooLarge, encoding
    var errorDescription: String? {
        switch self {
        case .invalidImage: "This image could not be opened. Choose another image."
        case .invalidCrop: "Select a larger crop area."
        case .tooLarge: "This sketch is larger than 8 MB. Crop it or use a smaller background."
        case .encoding: "The sketch could not be saved. Try attaching it again."
        }
    }
}

/// One coordinate space for the live canvas, image layer, crop and PNG. Sheet
/// resizing only scales the viewport; it never rewrites strokes or source pixels.
enum SketchExport {
    static let pageSize = CGSize(width: 1024, height: 768)
    static let wholePage = CGRect(x: 0, y: 0, width: 1, height: 1)

    nonisolated static func fit(_ size: CGSize, in bounds: CGRect) -> CGRect {
        guard size.width > 0, size.height > 0 else { return .zero }
        let scale = min(bounds.width / size.width, bounds.height / size.height)
        let width = size.width * scale, height = size.height * scale
        return CGRect(x: bounds.midX - width / 2, y: bounds.midY - height / 2, width: width, height: height)
    }

    nonisolated static func prepareBackground(_ data: Data) throws -> Data {
        guard !data.isEmpty, data.count <= 64 * 1024 * 1024,
              let source = CGImageSourceCreateWithData(data as CFData, nil),
              let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let width = properties[kCGImagePropertyPixelWidth] as? NSNumber,
              let height = properties[kCGImagePropertyPixelHeight] as? NSNumber,
              width.intValue > 0, height.intValue > 0,
              width.intValue <= 8192, height.intValue <= 8192,
              width.int64Value * height.int64Value <= 64 * 1024 * 1024,
              let image = CGImageSourceCreateThumbnailAtIndex(source, 0, [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceShouldCacheImmediately: true,
                kCGImageSourceThumbnailMaxPixelSize: 1024
              ] as CFDictionary),
              let png = UIImage(cgImage: image).pngData() else { throw SketchFailure.invalidImage }
        return png
    }

    static func png(drawing: Data, background: Data?, crop: CGRect) async throws -> Data {
        let worker = Task.detached(priority: .userInitiated) {
            try Task.checkCancellation()
            return try render(drawing: drawing, background: background, crop: crop)
        }
        return try await withTaskCancellationHandler(operation: { try await worker.value }, onCancel: { worker.cancel() })
    }

    nonisolated static func render(drawing: Data, background: Data?, crop: CGRect) throws -> Data {
        guard [crop.minX, crop.minY, crop.width, crop.height].allSatisfy(\.isFinite),
              crop.minX >= 0, crop.minY >= 0, crop.maxX <= 1, crop.maxY <= 1,
              crop.width >= 0.02, crop.height >= 0.02 else { throw SketchFailure.invalidCrop }
        let drawing = try PKDrawing(data: drawing)
        let backgroundImage: UIImage?
        if let background {
            guard let image = UIImage(data: background) else { throw SketchFailure.invalidImage }
            backgroundImage = image
        } else { backgroundImage = nil }
        let page = CGRect(origin: .zero, size: pageSize)
        let area = CGRect(x: crop.minX * page.width, y: crop.minY * page.height,
                          width: crop.width * page.width, height: crop.height * page.height).integral.intersection(page)
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        format.opaque = true
        // PencilKit resolves ink against the current appearance on this worker.
        // Match the canvas's white paper even when the conversation is dark.
        let image = UIGraphicsImageRenderer(size: area.size, format: format).image { context in
            UITraitCollection(userInterfaceStyle: .light).performAsCurrent {
                UIColor.white.setFill()
                context.fill(CGRect(origin: .zero, size: area.size))
                context.cgContext.translateBy(x: -area.minX, y: -area.minY)
                if let backgroundImage { backgroundImage.draw(in: fit(backgroundImage.size, in: page)) }
                drawing.image(from: page, scale: 1).draw(in: page)
            }
        }
        try Task.checkCancellation()
        guard let png = image.pngData() else { throw SketchFailure.encoding }
        guard png.count <= 8 * 1024 * 1024 else { throw SketchFailure.tooLarge }
        return png
    }

    /// Keep the exact PNG, its declared MIME and the standard integrity checks.
    static func attachment(_ data: Data) async throws -> StagedFile {
        try await Task.detached(priority: .userInitiated) {
            guard !data.isEmpty, data.count <= 8 * 1024 * 1024 else { throw SketchFailure.tooLarge }
            try ConversationFile.validateContent(data, mime: "image/png")
            guard let source = CGImageSourceCreateWithData(data as CFData, nil),
                  CGImageSourceGetType(source) as String? == UTType.png.identifier,
                  let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
                  let width = properties[kCGImagePropertyPixelWidth] as? NSNumber,
                  let height = properties[kCGImagePropertyPixelHeight] as? NSNumber,
                  width.intValue > 0, height.intValue > 0, width.intValue <= 2048, height.intValue <= 2048,
                  CGImageSourceCreateImageAtIndex(source, 0, nil) != nil else { throw SketchFailure.invalidImage }
            return try StagedFile(name: "Sketch-\(UUID().uuidString.prefix(8)).png", mimeType: "image/png", data: data)
        }.value
    }
}

fileprivate final class SketchPKCanvas: PKCanvasView {
    let history: UndoManager
    init(history: UndoManager) {
        self.history = history
        super.init(frame: .zero)
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }
    override var undoManager: UndoManager? { history }
}

@MainActor final class SketchDocument: NSObject, ObservableObject, PKCanvasViewDelegate {
    enum Tool: String, CaseIterable {
        case pen, marker, eraser, lasso
        var symbol: String {
            switch self {
            case .pen: "pencil.tip"
            case .marker: "highlighter"
            case .eraser: "eraser"
            case .lasso: "lasso"
            }
        }
    }
    /// Owned here so a background or crop set before the canvas exists undoes
    /// in the same history as the strokes drawn after it.
    let history = UndoManager()
    /// Built by `prepareCanvas()` once the sheet is on screen. Creating PencilKit's
    /// first canvas takes long enough to hold up the sheet's presentation.
    @Published fileprivate private(set) var canvas: SketchPKCanvas?
    @Published var tool = Tool.pen { didSet { updateTool() } }
    @Published var color = Color.black { didSet { updateTool() } }
    @Published var ruler = false { didSet { canvas?.isRulerActive = ruler } }
    @Published private(set) var background: Data?
    @Published private(set) var backgroundImage: UIImage?
    @Published private(set) var crop = SketchExport.wholePage
    @Published private(set) var canUndo = false
    @Published private(set) var canRedo = false
    @Published private(set) var strokeCount = 0
    private var historyObserver: AnyCancellable?

    override init() {
        super.init()
        history.levelsOfUndo = 10
        historyObserver = NotificationCenter.default.publisher(for: .NSUndoManagerDidCloseUndoGroup, object: history)
            .sink { [weak self] _ in Task { @MainActor [weak self] in self?.refreshHistory() } }
    }

    var isCanvasReady: Bool { canvas != nil }

    func prepareCanvas() {
        guard canvas == nil else { return }
        let canvas = SketchPKCanvas(history: history)
        canvas.delegate = self
        canvas.drawingPolicy = .anyInput
        canvas.isScrollEnabled = false
        canvas.isOpaque = false
        canvas.backgroundColor = .clear
        canvas.overrideUserInterfaceStyle = .light
        canvas.contentSize = SketchExport.pageSize
        canvas.isRulerActive = ruler
        canvas.accessibilityIdentifier = "sketch-canvas"
        canvas.accessibilityLabel = "Sketch canvas"
        canvas.accessibilityHint = "Draw with your finger or Apple Pencil."
        self.canvas = canvas
        updateTool()
        refreshCanvasValue()
    }

    private func updateTool() {
        guard let canvas else { return }
        let ink = UIColor(color).resolvedColor(with: UITraitCollection(userInterfaceStyle: .light))
        switch tool {
        case .pen: canvas.tool = PKInkingTool(.pen, color: ink, width: 5)
        case .marker: canvas.tool = PKInkingTool(.marker, color: ink, width: 22)
        case .eraser: canvas.tool = PKEraserTool(.vector)
        case .lasso: canvas.tool = PKLassoTool()
        }
    }

    func canvasViewDrawingDidChange(_ canvasView: PKCanvasView) {
        let count = canvasView.drawing.strokes.count
        if count != strokeCount { strokeCount = count; refreshCanvasValue() }
        refreshHistory()
    }

    private func refreshCanvasValue() {
        canvas?.accessibilityValue = "\(strokeCount) strokes" + (background == nil ? "" : "; Image background")
    }

    func refreshHistory() {
        let undo = history.canUndo, redo = history.canRedo
        if canUndo != undo { canUndo = undo }
        if canRedo != redo { canRedo = redo }
    }

    func undo() { history.undo(); refreshHistory() }
    func redo() { history.redo(); refreshHistory() }

    func setBackground(_ data: Data?, preview: UIImage? = nil) {
        let previous = background, previousImage = backgroundImage
        history.registerUndo(withTarget: self) { $0.setBackground(previous, preview: previousImage) }
        history.setActionName("Background")
        background = data
        backgroundImage = preview ?? data.flatMap { UIImage(data: $0) }
        refreshCanvasValue()
        refreshHistory()
    }

    func setCrop(_ rect: CGRect) {
        guard rect != crop else { return }
        let previous = crop
        history.registerUndo(withTarget: self) { $0.setCrop(previous) }
        history.setActionName("Crop")
        crop = rect
        refreshHistory()
    }

    var hasContent: Bool { background != nil || strokeCount > 0 }
    var drawingData: Data { (canvas?.drawing ?? PKDrawing()).dataRepresentation() }
}

struct SketchSheet: View {
    var backgroundData: Data? = nil
    /// Changes when the presenting conversation, draft or pairing is replaced.
    let contextToken: String
    let attach: (Data) async -> SketchAttachmentResult
    @StateObject private var document = SketchDocument()
    @Environment(\.dismiss) private var dismiss
    @Environment(\.wonderTheme) private var theme
    @State private var selection: PhotosPickerItem?
    @State private var picking = false
    @State private var loading = false
    @State private var attaching = false
    @State private var cropping = false
    @State private var failure: String?
    @State private var discard = false
    @State private var detent = PresentationDetent.medium
    @State private var work: Task<Void, Never>?
    @State private var generation = UUID()

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                ZStack {
                    if let canvas = document.canvas {
                        SketchCanvas(canvas: canvas, backgroundImage: document.backgroundImage)
                        SketchCropOverlay(crop: document.crop, editing: cropping, commit: document.setCrop)
                    } else {
                        ProgressView("Preparing canvas…").accessibilityIdentifier("sketch-preparing")
                    }
                }
                .allowsHitTesting(!loading && !attaching)
                .padding(12)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(theme.surface)
                tools
                if let failure {
                    Text(failure).font(.footnote).foregroundStyle(.red)
                        .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12).padding(.bottom, 8)
                        .accessibilityIdentifier("sketch-error")
                }
            }
            .wonderPage()
            .navigationTitle("Sketch").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { if document.hasContent { discard = true } else { dismiss() } }
                        .disabled(attaching).accessibilityIdentifier("sketch-cancel")
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button { export() } label: {
                        if attaching { ProgressView() } else { Text("Attach").fontWeight(.semibold) }
                    }
                    .disabled(!document.hasContent || loading || attaching)
                    .accessibilityLabel("Attach sketch").accessibilityIdentifier("sketch-attach")
                }
            }
            .overlay { if loading, document.isCanvasReady { ProgressView("Opening image…").padding().background(.regularMaterial) } }
        }
        .presentationDetents([.medium, .large], selection: $detent)
        .presentationDragIndicator(.hidden)
        .presentationContentInteraction(.scrolls)
        .interactiveDismissDisabled(document.hasContent || loading || attaching)
        .photosPicker(isPresented: $picking, selection: $selection, matching: .images, preferredItemEncoding: .compatible)
        .task(id: selection) {
            guard let selection else { return }
            loading = true
            defer { loading = false; self.selection = nil }
            do {
                guard let data = try await selection.loadTransferable(type: Data.self) else { throw SketchFailure.invalidImage }
                try await importImage(data)
            } catch { if !Task.isCancelled { failure = error.localizedDescription } }
        }
        .task {
            // Let the sheet reach the screen before PencilKit holds the main thread.
            await SketchFrames.wait(2)
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-diagnostics-sketch-canvas-held") {
                // Holds "Preparing canvas…" until `wonderctl signal sketch-canvas`.
                while !Task.isCancelled, !DiagnosticSubagentFixture.signalled("sketch-canvas") {
                    try? await Task.sleep(for: .milliseconds(100))
                }
            }
            #endif
            guard !Task.isCancelled else { return }
            document.prepareCanvas()
        }
        .task {
            guard let backgroundData else { return }
            loading = true
            defer { loading = false }
            do {
                try await importImage(backgroundData)
                document.history.removeAllActions()
                document.refreshHistory()
            } catch { if !Task.isCancelled { failure = error.localizedDescription } }
        }
        .onChange(of: contextToken) { _, _ in generation = UUID(); work?.cancel(); dismiss() }
        .onDisappear { generation = UUID(); work?.cancel() }
        .confirmationDialog("Discard this sketch?", isPresented: $discard, titleVisibility: .visible) {
            Button("Discard sketch", role: .destructive) { dismiss() }
        }
    }

    private var tools: some View {
        VStack(spacing: 0) {
            HStack(spacing: 0) {
                ForEach(SketchDocument.Tool.allCases, id: \.self) { tool in
                    toolButton(tool.rawValue.capitalized, symbol: tool.symbol, selected: document.tool == tool) {
                        cropping = false; document.tool = tool
                    }.accessibilityIdentifier("sketch-tool-\(tool.rawValue)")
                }
                toolButton("Ruler", symbol: "ruler", selected: document.ruler) { document.ruler.toggle() }
                    .accessibilityIdentifier("sketch-ruler")
                ColorPicker("Ink color", selection: $document.color, supportsOpacity: false)
                    .labelsHidden().frame(minWidth: 44, minHeight: 44)
                    .accessibilityLabel("Ink color").accessibilityIdentifier("sketch-color")
                Spacer(minLength: 0)
                toolButton("Undo", symbol: "arrow.uturn.backward") { document.undo() }
                    .disabled(!document.canUndo).keyboardShortcut("z", modifiers: .command)
                    .accessibilityIdentifier("sketch-undo")
                toolButton("Redo", symbol: "arrow.uturn.forward") { document.redo() }
                    .disabled(!document.canRedo).keyboardShortcut("z", modifiers: [.command, .shift])
                    .accessibilityIdentifier("sketch-redo")
            }
            .padding(.horizontal, 6)
            HStack(spacing: 2) {
                Menu {
                    Button("Choose photo", systemImage: "photo") { picking = true }
                    if document.background != nil {
                        Button("Remove background", systemImage: "trash", role: .destructive) { document.setBackground(nil) }
                    }
                } label: { Label("Image", systemImage: "photo.badge.plus").frame(minWidth: 44, minHeight: 44) }
                    .labelStyle(.iconOnly)
                    .accessibilityIdentifier("sketch-image")
                PasteButton(supportedContentTypes: [.image]) { providers in
                    guard let provider = providers.first,
                          let type = provider.registeredTypeIdentifiers.first(where: { UTType($0)?.conforms(to: .image) == true }) else { return }
                    work?.cancel()
                    work = Task { @MainActor in
                        loading = true
                        defer { loading = false }
                        do {
                            let data: Data = try await withCheckedThrowingContinuation { continuation in
                                provider.loadDataRepresentation(forTypeIdentifier: type) { data, error in
                                    if let error { continuation.resume(throwing: error) }
                                    else if let data { continuation.resume(returning: data) }
                                    else { continuation.resume(throwing: SketchFailure.invalidImage) }
                                }
                            }
                            try Task.checkCancellation()
                            try await importImage(data)
                        } catch { if !Task.isCancelled { failure = error.localizedDescription } }
                    }
                }
                .labelStyle(.iconOnly)
                .frame(minWidth: 44, minHeight: 44)
                .accessibilityLabel("Paste image").accessibilityIdentifier("sketch-paste")
                toolButton(cropping ? "Done cropping" : "Crop", symbol: cropping ? "checkmark" : "crop", selected: cropping) {
                    cropping.toggle()
                    if cropping { document.ruler = false }
                }.accessibilityIdentifier("sketch-crop")
                if cropping {
                    toolButton("Center crop", symbol: "rectangle.center.inset.filled") {
                        document.setCrop(CGRect(x: 0.1, y: 0.1, width: 0.8, height: 0.8))
                    }.accessibilityIdentifier("sketch-crop-center")
                }
                if document.crop != SketchExport.wholePage {
                    toolButton("Reset crop", symbol: "arrow.counterclockwise") { document.setCrop(SketchExport.wholePage) }
                        .accessibilityIdentifier("sketch-crop-reset")
                }
                Spacer(minLength: 0)
                toolButton(detent == .medium ? "Expand sketch" : "Collapse sketch",
                           symbol: detent == .medium ? "arrow.up.left.and.arrow.down.right" : "arrow.down.right.and.arrow.up.left") {
                    detent = detent == .medium ? .large : .medium
                }.accessibilityIdentifier("sketch-expand")
            }
            .padding(.horizontal, 12)
        }
        .font(.body).disabled(loading || attaching || !document.isCanvasReady)
    }

    private func toolButton(_ title: String, symbol: String, selected: Bool = false, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol).frame(minWidth: 44, minHeight: 44)
                .background(selected ? theme.text.opacity(0.12) : .clear, in: RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(.plain).accessibilityLabel(title)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    private func importImage(_ data: Data) async throws {
        let current = generation
        let worker = Task.detached(priority: .userInitiated) { () throws -> (Data, UIImage) in
            let png = try SketchExport.prepareBackground(data)
            guard let source = CGImageSourceCreateWithData(png as CFData, nil),
                  let image = CGImageSourceCreateImageAtIndex(source, 0, [kCGImageSourceShouldCacheImmediately: true] as CFDictionary)
            else { throw SketchFailure.invalidImage }
            return (png, UIImage(cgImage: image))
        }
        let prepared = try await withTaskCancellationHandler(operation: { try await worker.value }, onCancel: { worker.cancel() })
        try Task.checkCancellation()
        guard generation == current else { return }
        document.setBackground(prepared.0, preview: prepared.1)
        failure = nil
    }

    private func export() {
        let drawing = document.drawingData, background = document.background, crop = document.crop
        let current = generation
        attaching = true; failure = nil
        work = Task { @MainActor in
            defer { attaching = false }
            do {
                let png = try await SketchExport.png(drawing: drawing, background: background, crop: crop)
                try Task.checkCancellation()
                guard generation == current else { return }
                let result = await attach(png)
                guard !Task.isCancelled, generation == current else { return }
                switch result {
                case .attached: dismiss()
                case .cancelled: failure = "The destination changed. Close this sketch and reopen it in the intended chat."
                case .failed(let reason): failure = reason
                }
            } catch { if !Task.isCancelled { failure = error.localizedDescription } }
        }
    }
}

/// Resumes after the screen has drawn `count` more frames.
@MainActor private enum SketchFrames {
    static func wait(_ count: Int) async {
        let counter = Counter(remaining: count)
        await withTaskCancellationHandler {
            await withCheckedContinuation { counter.start($0) }
        } onCancel: {
            Task { @MainActor in counter.finish() }
        }
    }

    @MainActor private final class Counter: NSObject {
        private var remaining: Int
        private var link: CADisplayLink?
        private var continuation: CheckedContinuation<Void, Never>?
        init(remaining: Int) { self.remaining = remaining }
        func start(_ continuation: CheckedContinuation<Void, Never>) {
            self.continuation = continuation
            let link = CADisplayLink(target: self, selector: #selector(frame))
            link.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 60, preferred: 60)
            link.add(to: .main, forMode: .common)
            self.link = link
        }
        @objc private func frame() {
            remaining -= 1
            if remaining <= 0 { finish() }
        }
        func finish() {
            link?.invalidate(); link = nil
            continuation?.resume(); continuation = nil
        }
    }
}

private struct SketchCanvas: UIViewRepresentable {
    let canvas: SketchPKCanvas
    let backgroundImage: UIImage?
    func makeUIView(context: Context) -> SketchCanvasHost { SketchCanvasHost(canvas: canvas) }
    func updateUIView(_ view: SketchCanvasHost, context: Context) { view.imageView.image = backgroundImage }
    static func dismantleUIView(_ view: SketchCanvasHost, coordinator: ()) { view.canvas.resignFirstResponder() }
}

private final class SketchCanvasHost: UIView {
    let canvas: PKCanvasView
    let imageView = UIImageView()
    private let page = UIView()
    init(canvas: PKCanvasView) {
        self.canvas = canvas
        super.init(frame: .zero)
        page.backgroundColor = .white
        page.clipsToBounds = true
        page.layer.cornerCurve = .continuous
        imageView.contentMode = .scaleAspectFit
        imageView.isAccessibilityElement = false
        addSubview(page); page.addSubview(imageView); page.addSubview(canvas)
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }
    override func layoutSubviews() {
        super.layoutSubviews()
        let fit = SketchExport.fit(SketchExport.pageSize, in: bounds)
        page.bounds = CGRect(origin: .zero, size: SketchExport.pageSize)
        page.center = CGPoint(x: fit.midX, y: fit.midY)
        page.transform = CGAffineTransform(scaleX: fit.width / SketchExport.pageSize.width, y: fit.height / SketchExport.pageSize.height)
        // Soft corners on screen; the page is scaled, so the radius is set in its own units.
        page.layer.cornerRadius = 12 * SketchExport.pageSize.width / max(1, fit.width)
        imageView.frame = page.bounds
        canvas.frame = page.bounds
    }
}

private struct SketchCropOverlay: View {
    let crop: CGRect
    let editing: Bool
    let commit: (CGRect) -> Void
    @State private var live: CGRect?
    var body: some View {
        GeometryReader { geometry in
            let fit = SketchExport.fit(SketchExport.pageSize, in: CGRect(origin: .zero, size: geometry.size))
            let shown = live ?? crop
            let area = CGRect(x: fit.minX + shown.minX * fit.width, y: fit.minY + shown.minY * fit.height,
                              width: shown.width * fit.width, height: shown.height * fit.height)
            ZStack(alignment: .topLeading) {
                Path { path in path.addRect(fit); path.addRect(area) }
                    .fill(.black.opacity(0.5), style: FillStyle(eoFill: true))
                if editing || crop != SketchExport.wholePage {
                    Rectangle().strokeBorder(.white, style: StrokeStyle(lineWidth: 2, dash: editing ? [6, 4] : []))
                        .frame(width: area.width, height: area.height).offset(x: area.minX, y: area.minY)
                }
                if editing {
                    Text("Drag to crop").font(.footnote.weight(.medium)).foregroundStyle(.white)
                        .padding(6).background(.black.opacity(0.7)).position(x: fit.midX, y: fit.minY + 18)
                }
            }
            .frame(width: geometry.size.width, height: geometry.size.height)
            .contentShape(Rectangle())
            .gesture(DragGesture(minimumDistance: 2).onChanged { value in
                func point(_ p: CGPoint) -> CGPoint {
                    CGPoint(x: min(1, max(0, (p.x - fit.minX) / max(1, fit.width))),
                            y: min(1, max(0, (p.y - fit.minY) / max(1, fit.height))))
                }
                let start = point(value.startLocation), end = point(value.location)
                let rect = CGRect(x: min(start.x, end.x), y: min(start.y, end.y), width: abs(end.x - start.x), height: abs(end.y - start.y))
                if rect.width >= 0.02, rect.height >= 0.02 { live = rect }
            }.onEnded { _ in if let live { commit(live) }; live = nil })
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Crop area")
            .accessibilityValue("\(Int(shown.width * 100))% wide, \(Int(shown.height * 100))% high")
            .accessibilityActions {
                Button("Crop center") { commit(CGRect(x: 0.1, y: 0.1, width: 0.8, height: 0.8)) }
                Button("Reset crop") { commit(SketchExport.wholePage) }
            }
            .accessibilityIdentifier("sketch-crop-area")
        }
        .allowsHitTesting(editing)
        .accessibilityHidden(!editing)
    }
}
