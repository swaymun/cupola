import CoreTransferable
import PhotosUI
import AVFoundation
import CoreGraphics
import ImageIO
import SwiftUI
import UIKit
import UniformTypeIdentifiers
import WonderPairing

enum CameraAttachmentResult: Sendable {
    case attached
    case cancelled
    case failed
}

enum CameraPhotoPreparationFailure: LocalizedError {
    case invalidImage
    case tooLarge
    case encodingFailed

    var errorDescription: String? {
        switch self {
        case .invalidImage: return "The captured photo could not be read."
        case .tooLarge: return "The captured photo is larger than 8 MB."
        case .encodingFailed: return "The captured photo could not be prepared."
        }
    }
}

struct PreparedCameraPhoto: Sendable {
    let data: Data
    let fileExtension: String
    let mimeType: String
}

/// Camera always re-encodes (orientation applied, at most 8 MB). Photos and
/// paste follow `AttachmentImagePolicy`: the original when it fits, otherwise
/// the same re-encode for a still photo, otherwise a refusal.
enum CameraPhotoPreparation {
    nonisolated static let maximumBytes = AttachmentImagePolicy.maximumBytes

    static func prepare(_ data: Data) async throws -> PreparedCameraPhoto {
        try await detached { try prepareSynchronously(data) }
    }

    /// A photo chosen in Photos or pasted: the original when it fits.
    static func prepareChosen(_ data: Data) async throws -> PreparedCameraPhoto {
        try await detached { try prepareChosenSynchronously(data) }
    }

    private static func detached(_ work: @escaping @Sendable () throws -> PreparedCameraPhoto) async throws -> PreparedCameraPhoto {
        let worker = Task.detached(priority: .userInitiated) {
            try Task.checkCancellation()
            return try work()
        }
        return try await withTaskCancellationHandler(operation: {
            try await worker.value
        }, onCancel: {
            worker.cancel()
        })
    }

    nonisolated static func prepareChosenSynchronously(_ data: Data) throws -> PreparedCameraPhoto {
        guard !data.isEmpty, let source = CGImageSourceCreateWithData(data as CFData, nil),
              CGImageSourceCopyPropertiesAtIndex(source, 0, nil) != nil,
              let identifier = CGImageSourceGetType(source),
              let type = UTType(identifier as String), type.conforms(to: .image),
              let mime = type.preferredMIMEType, let ext = type.preferredFilenameExtension else {
            throw CameraPhotoPreparationFailure.invalidImage
        }
        if data.count <= maximumBytes { return PreparedCameraPhoto(data: data, fileExtension: ext, mimeType: mime) }
        guard AttachmentImagePolicy.mayShrink(source) else { throw CameraPhotoPreparationFailure.tooLarge }
        return try prepareSynchronously(data)
    }

    private nonisolated static func prepareSynchronously(_ data: Data) throws -> PreparedCameraPhoto {
        guard !data.isEmpty else { throw CameraPhotoPreparationFailure.invalidImage }
        guard data.count <= 64 * 1024 * 1024 else { throw CameraPhotoPreparationFailure.tooLarge }
        guard let source = CGImageSourceCreateWithData(data as CFData, nil),
              let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any] else {
            throw CameraPhotoPreparationFailure.invalidImage
        }

        let width = (properties[kCGImagePropertyPixelWidth] as? NSNumber)?.intValue ?? 0
        let height = (properties[kCGImagePropertyPixelHeight] as? NSNumber)?.intValue ?? 0
        guard width > 0, height > 0, width <= 8192, height <= 8192,
              Int64(width) * Int64(height) <= 64 * 1024 * 1024 else {
            throw CameraPhotoPreparationFailure.tooLarge
        }
        let originalDimension = max(width, height)
        let dimensions = [min(originalDimension, 4096), 3072, 2048, 1536, 1024]
            .filter { $0 > 0 }
            .reduce(into: [Int]()) { values, value in
                if !values.contains(value) { values.append(value) }
            }

        for dimension in dimensions {
            try Task.checkCancellation()
            guard let resized = makeImage(source: source, properties: properties, maxDimension: dimension) else { continue }
            for quality in [0.84, 0.72, 0.58, 0.44] {
                try Task.checkCancellation()
                guard let encoded = encodeJPEG(resized, quality: quality) else { continue }
                if encoded.count <= maximumBytes {
                    return PreparedCameraPhoto(data: encoded, fileExtension: "jpg", mimeType: UTType.jpeg.preferredMIMEType ?? "image/jpeg")
                }
            }
        }
        throw CameraPhotoPreparationFailure.tooLarge
    }

    private nonisolated static func makeImage(source: CGImageSource, properties: [CFString: Any], maxDimension: Int? = nil) -> CGImage? {
        let dimension = maxDimension ?? max(
            (properties[kCGImagePropertyPixelWidth] as? NSNumber)?.intValue ?? 1,
            (properties[kCGImagePropertyPixelHeight] as? NSNumber)?.intValue ?? 1
        )
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            // This applies the source EXIF orientation to the pixels. The
            // encoded result below explicitly writes orientation .up.
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: max(1, dimension)
        ]
        return CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
    }

    private nonisolated static func encodeJPEG(_ image: CGImage, quality: Double) -> Data? {
        let output = NSMutableData()
        guard let destination = CGImageDestinationCreateWithData(output, UTType.jpeg.identifier as CFString, 1, nil) else { return nil }
        let properties: [CFString: Any] = [
            kCGImageDestinationLossyCompressionQuality: quality,
            kCGImagePropertyOrientation: 1
        ]
        CGImageDestinationAddImage(destination, image, properties as CFDictionary)
        guard CGImageDestinationFinalize(destination) else { return nil }
        return output as Data
    }
}

enum CameraPosition: Sendable {
    case back
    case front
}

enum CameraCaptureState: Equatable {
    case checking
    case ready
    case capturing
    case denied
    case restricted
    case unavailable
    case failed(String)
}

private enum CameraSessionEvent: Sendable {
    case ready(run: UUID, position: CameraPosition, flashAvailable: Bool)
    case unavailable(run: UUID)
    case failed(run: UUID, message: String)
    case photo(run: UUID, capture: UUID, data: Data?)
}

/// Owns all AVCaptureSession work on a dedicated queue. The unchecked
/// Sendable boundary is intentional: AVFoundation objects are thread-safe by
/// contract, and this type serializes every session mutation on sessionQueue.
private final class CameraSessionCoordinator: @unchecked Sendable {
    let session = AVCaptureSession()
    let photoOutput = AVCapturePhotoOutput()
    private let sessionQueue = DispatchQueue(label: "com.saimun.wonder.camera.session")
    private let delegateLock = NSLock()
    private var input: AVCaptureDeviceInput?
    private var position: CameraPosition = .back
    private var delegates: [UUID: CameraPhotoDelegate] = [:]
    private var event: (@Sendable (CameraSessionEvent) -> Void)?

    init() {}

    fileprivate func setEventHandler(_ handler: @escaping @Sendable (CameraSessionEvent) -> Void) {
        event = handler
    }

    fileprivate func emit(_ event: CameraSessionEvent) { self.event?(event) }

    func start(run: UUID) {
        sessionQueue.async { [weak self] in
            guard let self else { return }
            if self.session.isRunning {
                let device = self.input?.device
                self.emit(.ready(run: run, position: self.position, flashAvailable: device?.hasFlash == true))
                return
            }

            if self.input == nil {
                guard let device = Self.device(for: .back) ?? Self.device(for: .front) else {
                    self.emit(.unavailable(run: run))
                    return
                }
                do {
                    let input = try AVCaptureDeviceInput(device: device)
                    self.session.beginConfiguration()
                    guard self.session.canAddInput(input), self.session.canAddOutput(self.photoOutput) else {
                        self.session.commitConfiguration()
                        self.emit(.failed(run: run, message: "Camera hardware is unavailable."))
                        return
                    }
                    self.session.sessionPreset = .photo
                    self.session.addInput(input)
                    self.session.addOutput(self.photoOutput)
                    self.session.commitConfiguration()
                    self.input = input
                    self.position = device.position == .front ? .front : .back
                } catch {
                    self.emit(.failed(run: run, message: "Camera hardware is unavailable."))
                    return
                }
            }

            self.session.startRunning()
            guard self.session.isRunning, let device = self.input?.device else {
                self.emit(.failed(run: run, message: "Camera could not be started."))
                return
            }
            self.emit(.ready(run: run, position: self.position, flashAvailable: device.hasFlash))
        }
    }

    func stop() {
        // Keep the coordinator alive until this queued shutdown has stopped the
        // session and released capture delegates. View dismissal must not turn
        // cleanup into a best-effort weak callback.
        sessionQueue.async { [self] in
            self.session.stopRunning()
            self.delegateLock.lock()
            self.delegates.removeAll()
            self.delegateLock.unlock()
        }
    }

    func switchCamera(run: UUID) {
        sessionQueue.async { [weak self] in
            guard let self, self.session.isRunning, let current = self.input else { return }
            let nextPosition: CameraPosition = self.position == .back ? .front : .back
            guard let device = Self.device(for: nextPosition) else {
                self.emit(.failed(run: run, message: "The other camera is unavailable."))
                return
            }
            do {
                let next = try AVCaptureDeviceInput(device: device)
                self.session.beginConfiguration()
                self.session.removeInput(current)
                guard self.session.canAddInput(next) else {
                    self.session.addInput(current)
                    self.session.commitConfiguration()
                    self.emit(.failed(run: run, message: "The other camera is unavailable."))
                    return
                }
                self.session.addInput(next)
                self.session.commitConfiguration()
                self.input = next
                self.position = nextPosition
                self.emit(.ready(run: run, position: nextPosition, flashAvailable: device.hasFlash))
            } catch {
                self.emit(.failed(run: run, message: "The other camera is unavailable."))
            }
        }
    }

    func capture(run: UUID, capture: UUID, flashEnabled: Bool, rotationAngle: CGFloat?) {
        sessionQueue.async { [weak self] in
            guard let self, self.session.isRunning else {
                self?.emit(.photo(run: run, capture: capture, data: nil))
                return
            }
            if let rotationAngle,
               let connection = self.photoOutput.connection(with: .video),
               connection.isVideoRotationAngleSupported(rotationAngle) {
                connection.videoRotationAngle = rotationAngle
            }
            let settings = AVCapturePhotoSettings()
            if self.photoOutput.supportedFlashModes.contains(.on) {
                settings.flashMode = flashEnabled ? .on : .off
            }
            let delegate = CameraPhotoDelegate(run: run, capture: capture, owner: self)
            self.delegateLock.lock()
            self.delegates[capture] = delegate
            self.delegateLock.unlock()
            self.photoOutput.capturePhoto(with: settings, delegate: delegate)
        }
    }

    fileprivate func removeDelegate(_ capture: UUID) {
        delegateLock.lock()
        delegates.removeValue(forKey: capture)
        delegateLock.unlock()
    }

    private static func device(for position: CameraPosition) -> AVCaptureDevice? {
        let requested: AVCaptureDevice.Position = position == .front ? .front : .back
        let discovery = AVCaptureDevice.DiscoverySession(
            deviceTypes: [.builtInWideAngleCamera], mediaType: .video, position: requested
        )
        return discovery.devices.first
    }
}

private final class CameraPhotoDelegate: NSObject, AVCapturePhotoCaptureDelegate, @unchecked Sendable {
    let run: UUID
    let capture: UUID
    weak var owner: CameraSessionCoordinator?

    init(run: UUID, capture: UUID, owner: CameraSessionCoordinator) {
        self.run = run
        self.capture = capture
        self.owner = owner
    }

    func photoOutput(_ output: AVCapturePhotoOutput, didFinishProcessingPhoto photo: AVCapturePhoto, error: Error?) {
        let data = error == nil ? photo.fileDataRepresentation() : nil
        owner?.emit(.photo(run: run, capture: capture, data: data))
    }

    func photoOutput(_ output: AVCapturePhotoOutput, didFinishCaptureFor resolvedSettings: AVCaptureResolvedPhotoSettings, error: Error?) {
        if error != nil { owner?.emit(.photo(run: run, capture: capture, data: nil)) }
        owner?.removeDelegate(capture)
    }
}

@MainActor final class CameraCaptureController: ObservableObject {
    @Published private(set) var state: CameraCaptureState = .checking
    @Published private(set) var position: CameraPosition = .back
    @Published private(set) var flashAvailable = false
    @Published var flashEnabled = false
    @Published var errorMessage: String?
    private let coordinator: CameraSessionCoordinator
    private var startTask: Task<Void, Never>?
    private var lifecycle = UUID()
    private var activeCapture: UUID?
    private var photoHandler: ((Data) -> Void)?
    private var captureRotationAngle: CGFloat?

    #if WONDER_DIAGNOSTICS
    let fixture: CameraCaptureFixture?
    #endif

    var session: AVCaptureSession { coordinator.session }

    #if WONDER_DIAGNOSTICS
    init(fixture: CameraCaptureFixture? = nil) {
        self.fixture = fixture
        self.coordinator = CameraSessionCoordinator()
        self.coordinator.setEventHandler { [weak self] event in
            Task { @MainActor [weak self] in self?.handle(event) }
        }
    }
    #else
    init() {
        self.coordinator = CameraSessionCoordinator()
        self.coordinator.setEventHandler { [weak self] event in
            Task { @MainActor [weak self] in self?.handle(event) }
        }
    }
    #endif

    deinit {
        coordinator.stop()
    }

    func start(onPhoto: @escaping (Data) -> Void) {
        photoHandler = onPhoto
        guard startTask == nil else { return }
        let run = lifecycle
        state = .checking
        #if WONDER_DIAGNOSTICS
        if let fixture {
            state = fixture.state
            position = .back
            flashAvailable = fixture.state == .ready
            startTask = Task { @MainActor [weak self] in
                guard let self else { return }
                self.startTask = nil
            }
            return
        }
        #endif
        startTask = Task { @MainActor [weak self] in
            guard let self else { return }
            let authorization = AVCaptureDevice.authorizationStatus(for: .video)
            let granted: Bool
            switch authorization {
            case .authorized: granted = true
            case .notDetermined: granted = await Self.requestCameraAccess()
            case .denied: self.state = .denied; self.startTask = nil; return
            case .restricted: self.state = .restricted; self.startTask = nil; return
            @unknown default: granted = false
            }
            guard !Task.isCancelled, run == self.lifecycle else { return }
            guard granted else { self.state = .denied; self.startTask = nil; return }
            self.coordinator.start(run: run)
            self.startTask = nil
        }
    }

    func stop() {
        lifecycle = UUID()
        startTask?.cancel()
        startTask = nil
        activeCapture = nil
        photoHandler = nil
        state = .checking
        coordinator.stop()
    }

    func capture() {
        guard state == .ready, activeCapture == nil else { return }
        let run = lifecycle
        let capture = UUID()
        activeCapture = capture
        state = .capturing
        #if WONDER_DIAGNOSTICS
        if let fixture {
            Task { @MainActor [weak self] in
                guard let self, self.activeCapture == capture, self.lifecycle == run else { return }
                self.activeCapture = nil
                self.state = .ready
                self.photoHandler?(fixture.data)
            }
            return
        }
        #endif
        coordinator.capture(run: run, capture: capture, flashEnabled: flashEnabled, rotationAngle: captureRotationAngle)
    }

    func setCaptureRotationAngle(_ angle: CGFloat) {
        captureRotationAngle = angle
    }

    func switchCamera() {
        guard state == .ready else { return }
        #if WONDER_DIAGNOSTICS
        if fixture != nil {
            position = position == .back ? .front : .back
            flashAvailable = position == .back
            if !flashAvailable { flashEnabled = false }
            return
        }
        #endif
        state = .checking
        captureRotationAngle = nil
        coordinator.switchCamera(run: lifecycle)
    }

    func toggleFlash() {
        guard flashAvailable else { return }
        flashEnabled.toggle()
    }

    func runtimeError() {
        guard photoHandler != nil else { return }
        lifecycle = UUID()
        activeCapture = nil
        coordinator.stop()
        state = .failed("Camera is temporarily unavailable.")
    }

    func retry() {
        guard photoHandler != nil, case .failed = state else { return }
        state = .checking
        coordinator.start(run: lifecycle)
    }

    private func handle(_ event: CameraSessionEvent) {
        switch event {
        case .ready(let run, let position, let flashAvailable):
            guard run == lifecycle else { return }
            self.position = position
            self.flashAvailable = flashAvailable
            if !flashAvailable { flashEnabled = false }
            state = .ready
        case .unavailable(let run):
            guard run == lifecycle else { return }
            state = .unavailable
        case .failed(let run, let message):
            guard run == lifecycle else { return }
            state = .failed(message)
        case .photo(let run, let capture, let data):
            guard run == lifecycle, activeCapture == capture else { return }
            activeCapture = nil
            state = .ready
            guard let data else {
                errorMessage = "The photo could not be captured. Try again."
                return
            }
            photoHandler?(data)
        }
    }

    private static func requestCameraAccess() async -> Bool {
        await withCheckedContinuation { continuation in
            AVCaptureDevice.requestAccess(for: .video) { continuation.resume(returning: $0) }
        }
    }
}

struct CameraPreview: UIViewRepresentable {
    let session: AVCaptureSession
    let position: CameraPosition
    let onRuntimeError: () -> Void
    let onInterrupted: () -> Void
    let onInterruptionEnded: () -> Void
    let onCaptureRotationAngle: (CGFloat) -> Void

    func makeUIView(context: Context) -> CameraPreviewView {
        let view = CameraPreviewView()
        view.onRuntimeError = onRuntimeError
        view.onInterrupted = onInterrupted
        view.onInterruptionEnded = onInterruptionEnded
        view.onCaptureRotationAngle = onCaptureRotationAngle
        view.session = session
        return view
    }

    func updateUIView(_ view: CameraPreviewView, context: Context) {
        view.onRuntimeError = onRuntimeError
        view.onInterrupted = onInterrupted
        view.onInterruptionEnded = onInterruptionEnded
        view.onCaptureRotationAngle = onCaptureRotationAngle
        if view.session !== session { view.session = session }
        view.configureRotation()
    }
}

final class CameraPreviewView: UIView {
    override class var layerClass: AnyClass { AVCaptureVideoPreviewLayer.self }

    private var previewLayer: AVCaptureVideoPreviewLayer { layer as! AVCaptureVideoPreviewLayer }
    private var rotationCoordinator: AVCaptureDevice.RotationCoordinator?
    private var previewRotationObservation: NSKeyValueObservation?
    private var captureRotationObservation: NSKeyValueObservation?
    nonisolated(unsafe) private var sessionObservers: [NSObjectProtocol] = []
    var onRuntimeError: (() -> Void)?
    var onInterrupted: (() -> Void)?
    var onInterruptionEnded: (() -> Void)?
    var onCaptureRotationAngle: ((CGFloat) -> Void)?
    var session: AVCaptureSession? {
        didSet {
            guard oldValue !== session else { return }
            removeSessionObservers()
            rotationCoordinator = nil
            previewRotationObservation = nil
            captureRotationObservation = nil
            previewLayer.session = session
            if let session { observe(session) }
            configureRotation()
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        previewLayer.frame = bounds
        previewLayer.videoGravity = .resizeAspectFill
        configureRotation()
    }

    deinit {
        removeSessionObservers()
    }

    func configureRotation() {
        guard let session, let input = session.inputs.compactMap({ $0 as? AVCaptureDeviceInput }).first else { return }
        if rotationCoordinator?.device?.uniqueID != input.device.uniqueID {
            previewRotationObservation = nil
            captureRotationObservation = nil
            let coordinator = AVCaptureDevice.RotationCoordinator(device: input.device, previewLayer: previewLayer)
            rotationCoordinator = coordinator
            previewRotationObservation = coordinator.observe(\.videoRotationAngleForHorizonLevelPreview, options: [.initial, .new]) { [weak self] _, _ in
                Task { @MainActor [weak self] in self?.applyRotation() }
            }
            captureRotationObservation = coordinator.observe(\.videoRotationAngleForHorizonLevelCapture, options: [.initial, .new]) { [weak self] _, _ in
                Task { @MainActor [weak self] in self?.applyRotation() }
            }
        }
        applyRotation()
    }

    private func applyRotation() {
        guard let coordinator = rotationCoordinator else { return }
        let previewAngle = coordinator.videoRotationAngleForHorizonLevelPreview
        if let connection = previewLayer.connection, connection.isVideoRotationAngleSupported(previewAngle) {
            connection.videoRotationAngle = previewAngle
        }
        let captureAngle = coordinator.videoRotationAngleForHorizonLevelCapture
        onCaptureRotationAngle?(captureAngle)
    }

    private func observe(_ session: AVCaptureSession) {
        let center = NotificationCenter.default
        sessionObservers = [
            center.addObserver(forName: AVCaptureSession.runtimeErrorNotification, object: session, queue: .main) { [weak self] _ in
                Task { @MainActor [weak self] in self?.onRuntimeError?() }
            },
            center.addObserver(forName: AVCaptureSession.wasInterruptedNotification, object: session, queue: .main) { [weak self] _ in
                Task { @MainActor [weak self] in self?.onInterrupted?() }
            },
            center.addObserver(forName: AVCaptureSession.interruptionEndedNotification, object: session, queue: .main) { [weak self] _ in
                Task { @MainActor [weak self] in self?.onInterruptionEnded?() }
            }
        ]
    }

    nonisolated private func removeSessionObservers() {
        let center = NotificationCenter.default
        for observer in sessionObservers { center.removeObserver(observer) }
        sessionObservers.removeAll()
    }
}

#if WONDER_DIAGNOSTICS
enum CameraCaptureFixture {
    case ready(data: Data, preview: UIImage)
    case denied
    case unavailable

    var state: CameraCaptureState {
        switch self {
        case .ready: return .ready
        case .denied: return .denied
        case .unavailable: return .unavailable
        }
    }

    var data: Data {
        switch self {
        case .ready(let data, _): return data
        case .denied, .unavailable: return Data()
        }
    }

    var preview: UIImage? {
        if case .ready(_, let preview) = self { return preview }
        return nil
    }
}
#endif

struct CameraCaptureView: View {
    let chatID: String
    let chatTitle: String
    let originatingScope: String
    let currentContextToken: String
    let attachVideo: (@MainActor (URL) async -> CameraAttachmentResult)?
    let attachPhoto: @MainActor (Data) async -> CameraAttachmentResult

    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.dynamicTypeSize) private var typeSize
    @StateObject private var controller: CameraCaptureController
    @State private var attachTask: Task<Void, Never>?
    @State private var recordingVideo = false
    @State private var attaching = false
    @State private var attachmentError = false
    @State private var originContextToken: String?
    @State private var finished = false
    @State private var detent: PresentationDetent = .medium

    #if WONDER_DIAGNOSTICS
    init(chatID: String, chatTitle: String, originatingScope: String, currentContextToken: String,
         fixture: CameraCaptureFixture? = nil,
         attachVideo: (@MainActor (URL) async -> CameraAttachmentResult)? = nil,
         attachPhoto: @escaping @MainActor (Data) async -> CameraAttachmentResult) {
        self.chatID = chatID
        self.chatTitle = chatTitle
        self.originatingScope = originatingScope
        self.currentContextToken = currentContextToken
        self.attachVideo = attachVideo
        self.attachPhoto = attachPhoto
        _controller = StateObject(wrappedValue: CameraCaptureController(fixture: fixture))
    }
    #else
    init(chatID: String, chatTitle: String, originatingScope: String, currentContextToken: String,
         attachVideo: (@MainActor (URL) async -> CameraAttachmentResult)? = nil,
         attachPhoto: @escaping @MainActor (Data) async -> CameraAttachmentResult) {
        self.chatID = chatID
        self.chatTitle = chatTitle
        self.originatingScope = originatingScope
        self.currentContextToken = currentContextToken
        self.attachVideo = attachVideo
        self.attachPhoto = attachPhoto
        _controller = StateObject(wrappedValue: CameraCaptureController())
    }
    #endif

    var body: some View {
        VStack(spacing: 10) {
            HStack {
                Button { cancelAndDismiss() } label: {
                    Image(systemName: "chevron.left")
                        .font(.title3.weight(.medium))
                        .frame(width: 44, height: 44)
                        .background(Color.white.opacity(0.10), in: Circle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Dismiss camera")
                .accessibilityIdentifier("camera-dismiss")
                Spacer()
            }
            Group {
                switch controller.state {
                case .checking:
                    ProgressView("Starting camera…")
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                case .denied:
                    unavailableView(title: "Camera access is off", message: "Allow camera access in Settings to take photos and record video in Cupola.", settings: true)
                case .restricted:
                    unavailableView(title: "Camera access is restricted", message: "This device does not allow Cupola to use the camera.", settings: false)
                case .unavailable:
                    unavailableView(title: "No camera available", message: "This device does not have a camera Cupola can use.", settings: false)
                case .failed(let message):
                    unavailableView(title: "Camera couldn’t start", message: message, settings: false, retry: true)
                case .ready, .capturing:
                    captureSurface
                }
            }
        }
        .padding(.horizontal, 16)
        .padding(.top, 16)
        .padding(.bottom, 12)
        .frame(maxWidth: 680, maxHeight: .infinity)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black)
        .foregroundStyle(.white)
        .tint(.white)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("camera-sheet")
        .presentationDetents([.medium, .large], selection: $detent)
        .presentationDragIndicator(.hidden)
        .presentationCornerRadius(28)
        .presentationBackground(.black)
        .presentationCompactAdaptation(.sheet)
        .preferredColorScheme(.dark)
        .fullScreenCover(isPresented: $recordingVideo, onDismiss: {
            if !finished, !attaching { controller.start { handleCapturedPhoto($0) } }
        }) {
            VideoRecorder { url in
                recordingVideo = false
                guard let url, let attachVideo else { return }
                attaching = true
                attachTask = Task { @MainActor in
                    defer { try? FileManager.default.removeItem(at: url) }
                    let result = await attachVideo(url)
                    guard !Task.isCancelled, originContextToken == currentContextToken else { return }
                    attaching = false
                    if case .failed = result {
                        attachmentError = true
                        controller.start { handleCapturedPhoto($0) }
                    } else { cancelAndDismiss() }
                }
            }
        }
        .onAppear {
            guard !finished, originContextToken == nil else { return }
            // Store and compare the same token. Prefixing it only here caused
            // every shutter result to be dropped before production staging.
            originContextToken = currentContextToken
            controller.start { data in handleCapturedPhoto(data) }
        }
        .onDisappear {
            guard !recordingVideo else { return }
            stopCapture()
            finished = true
        }
        .onChange(of: scenePhase) { _, phase in
            guard !finished, originContextToken == currentContextToken else { return }
            // Control Center, a banner or a call only pause the camera. A photo
            // already taken keeps attaching and dismisses the sheet when saved.
            if phase == .active { if !attaching && !recordingVideo { controller.start { data in handleCapturedPhoto(data) } } }
            else { controller.stop() }
        }
        .onChange(of: currentContextToken) { _, _ in
            cancelAndDismiss()
        }
        .onChange(of: typeSize, initial: true) { _, size in
            if size.isAccessibilitySize { detent = .large }
        }
        .alert("Couldn’t attach media", isPresented: $attachmentError) {
            Button("OK", role: .cancel) { attachmentError = false }
        } message: {
            Text("Your draft is unchanged.")
        }
        .alert("Camera problem", isPresented: Binding(get: { controller.errorMessage != nil }, set: { if !$0 { controller.errorMessage = nil } })) {
            Button("OK", role: .cancel) { controller.errorMessage = nil }
        } message: {
            Text(controller.errorMessage ?? "The camera could not complete that action.")
        }
    }

    private var captureSurface: some View {
        VStack(spacing: 14) {
            // Use the available sheet height, including landscape/iPad windows.
            // The preview layer fills this bounded canvas; no image work occurs
            // in body or the representable's update path.
            GeometryReader { geometry in
                preview
                    .frame(width: geometry.size.width, height: geometry.size.height)
                    .clipped()
                    .clipShape(RoundedRectangle(cornerRadius: 22, style: .continuous))
                    .allowsHitTesting(false)
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Camera preview")
            .accessibilityValue(controller.position == .back ? "Back camera" : "Front camera")
            .accessibilityIdentifier("camera-preview")
            HStack(alignment: .center) {
                if attachVideo != nil, UIImagePickerController.isSourceTypeAvailable(.camera) {
                    Button {
                        controller.stop()
                        recordingVideo = true
                    } label: { Image(systemName: "video.fill").frame(width: 44, height: 44) }
                    .accessibilityLabel("Record video")
                    .accessibilityIdentifier("camera-record-video")
                    .disabled(attaching)
                } else { Color.clear.frame(width: 44, height: 44) }
                Spacer()
                Button { controller.capture() } label: {
                    Circle()
                        .fill(Color.white)
                        .frame(width: 60, height: 60)
                        .padding(4)
                        .overlay(Circle().stroke(Color.white.opacity(0.8), lineWidth: 2))
                        .overlay {
                            if attaching { ProgressView().tint(.black) }
                        }
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Take photo")
                .accessibilityIdentifier("camera-shutter")
                .disabled(controller.state != .ready || attaching)
                .opacity(controller.state == .ready && !attaching ? 1 : 0.55)
                Spacer()
                optionsMenu
            }
        }

    }


    @ViewBuilder private var preview: some View {
        #if WONDER_DIAGNOSTICS
        if let image = controller.fixture?.preview {
            Image(uiImage: image)
                .resizable()
                .scaledToFill()
                .accessibilityHidden(true)
        } else {
            CameraPreview(
                session: controller.session,
                position: controller.position,
                onRuntimeError: { controller.runtimeError() },
                onInterrupted: { controller.runtimeError() },
                onInterruptionEnded: { controller.retry() },
                onCaptureRotationAngle: { controller.setCaptureRotationAngle($0) }
            ).accessibilityHidden(true)
        }
        #else
        CameraPreview(
            session: controller.session,
            position: controller.position,
            onRuntimeError: { controller.runtimeError() },
            onInterrupted: { controller.runtimeError() },
            onInterruptionEnded: { controller.retry() },
            onCaptureRotationAngle: { controller.setCaptureRotationAngle($0) }
        ).accessibilityHidden(true)
        #endif
    }

    private var optionsMenu: some View {
        Menu {
            Button("Switch camera", systemImage: "camera.rotate") { controller.switchCamera() }
                .disabled(controller.state != .ready)
            Button(controller.flashEnabled ? "Turn flash off" : "Turn flash on", systemImage: controller.flashEnabled ? "bolt.slash" : "bolt.fill") {
                controller.toggleFlash()
            }
            .disabled(!controller.flashAvailable || controller.state != .ready)
        } label: {
            Image(systemName: "ellipsis")
                .font(.title2.weight(.semibold))
                .frame(width: 44, height: 44)
                .background(Color.white.opacity(0.08), in: Circle())
                .overlay(Circle().stroke(Color.white.opacity(0.14), lineWidth: 1))
        }
        .accessibilityLabel("Camera options")
        .accessibilityIdentifier("camera-options")
        .disabled(controller.state != .ready || attaching)
    }

    private func unavailableView(title: String, message: String, settings: Bool, retry: Bool = false) -> some View {
        ScrollView {
        VStack(spacing: 18) {
            Image(systemName: "camera.fill").font(.system(size: 34)).foregroundStyle(.white.opacity(0.75))
            Text(title)
                .font(.title2.weight(.semibold))
                .multilineTextAlignment(.center)
                .accessibilityIdentifier("camera-permission-state")
            Text(message)
                .font(.body)
                .foregroundStyle(.white.opacity(0.72))
                .multilineTextAlignment(.center)
                .frame(maxWidth: 340)
            if settings {
                Button("Open Settings") {
                    guard let url = URL(string: UIApplication.openSettingsURLString) else { return }
                    UIApplication.shared.open(url)
                }
                .buttonStyle(.bordered)
            }
            if retry {
                Button("Try again") { controller.retry() }
                    .buttonStyle(.bordered)
                    .accessibilityIdentifier("camera-error-retry")
            }
            Button("Dismiss") { cancelAndDismiss() }
                .buttonStyle(.bordered)
                .accessibilityIdentifier("camera-error-dismiss")
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 16)
        }
    }

    private func handleCapturedPhoto(_ data: Data) {
        guard !finished, !attaching, originContextToken == currentContextToken else { return }
        attaching = true
        attachTask?.cancel()
        attachTask = Task { @MainActor in
            let result = await attachPhoto(data)
            guard !Task.isCancelled, originContextToken == currentContextToken else { return }
            attaching = false
            switch result {
            case .attached:
                cancelAndDismiss()
            case .cancelled:
                cancelAndDismiss()
            case .failed:
                attachmentError = true
                // The camera paused while this photo attached; offer it again.
                if scenePhase == .active { controller.start { data in handleCapturedPhoto(data) } }
            }
            attachTask = nil
        }
    }

    private func cancelAndDismiss() {
        finished = true
        stopCapture()
        dismiss()
    }

    private func stopCapture() {
        attachTask?.cancel()
        attachTask = nil
        attaching = false
        controller.stop()
    }
}

/// Copies the provider's ephemeral file before returning from its transfer callback.
struct PickedVideo: Transferable, Sendable {
    let file: StagedFile
    static var transferRepresentation: some TransferRepresentation {
        FileRepresentation(importedContentType: .movie) { received in
            let source = received.file
            return try await Task.detached(priority: .userInitiated) {
                let type = UTType(filenameExtension: source.pathExtension)
                let mime = type.flatMap { $0.conforms(to: .movie) ? $0.preferredMIMEType : nil } ?? "video/quicktime"
                return Self(file: try StagedFile.importingVideo(source,
                    name: "Video-\(UUID().uuidString.prefix(8)).\(source.pathExtension)", mimeType: mime))
            }.value
        }
    }
}

enum MediaPreparation {
    static func chosen(_ item: PhotosPickerItem) async throws -> StagedFile {
        if item.supportedContentTypes.contains(where: { $0.conforms(to: .movie) }) {
            guard let movie = try await item.loadTransferable(type: PickedVideo.self) else { throw FileFailure.integrity }
            return movie.file
        }
        guard let data = try await item.loadTransferable(type: Data.self) else { throw FileFailure.integrity }
        return try await ConnectionModel.prepareImageAttachment(data)
    }
    static func video(_ url: URL, suggestedMIME: String? = nil) async throws -> StagedFile {
        let worker = Task.detached(priority: .userInitiated) {
            try Task.checkCancellation()
            let type = UTType(filenameExtension: url.pathExtension)
            let mime = type.flatMap { $0.conforms(to: .movie) ? $0.preferredMIMEType : nil } ?? suggestedMIME ?? "video/quicktime"
            let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0
            try AttachmentLimits.validate(byteSize: size, mimeType: mime)
            let ext = type?.conforms(to: .movie) == true ? url.pathExtension : UTType(mimeType: mime)?.preferredFilenameExtension ?? "mov"
            return try StagedFile.importingVideo(url, name: "Video-\(UUID().uuidString.prefix(8)).\(ext)", mimeType: mime)
        }
        return try await withTaskCancellationHandler(operation: { try await worker.value }, onCancel: { worker.cancel() })
    }
}

/// Native recording controls handle camera switching, audio capture and retakes.
private struct VideoRecorder: UIViewControllerRepresentable {
    let finished: (URL?) -> Void
    func makeCoordinator() -> Coordinator { Coordinator(finished) }
    func makeUIViewController(context: Context) -> UIImagePickerController {
        let picker = UIImagePickerController()
        picker.sourceType = .camera
        picker.mediaTypes = [UTType.movie.identifier]
        picker.cameraCaptureMode = .video
        picker.videoQuality = .typeHigh
        picker.delegate = context.coordinator
        return picker
    }
    func updateUIViewController(_ controller: UIImagePickerController, context: Context) {}
    final class Coordinator: NSObject, UIImagePickerControllerDelegate, UINavigationControllerDelegate {
        let finished: (URL?) -> Void
        init(_ finished: @escaping (URL?) -> Void) { self.finished = finished }
        func imagePickerControllerDidCancel(_ picker: UIImagePickerController) { finished(nil) }
        func imagePickerController(_ picker: UIImagePickerController, didFinishPickingMediaWithInfo info: [UIImagePickerController.InfoKey: Any]) {
            finished(info[.mediaURL] as? URL)
        }
    }
}
