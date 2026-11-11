import Accelerate
import CoreVideo
import Foundation
@preconcurrency import WebRTC

/// The display-link rate the computer view asks for. It follows the stream
/// (rounded up to a rate the display can hold evenly) and never exceeds the
/// screen, so a 30 fps stream does not keep a ProMotion panel at 120 Hz.
enum ComputerDisplayRate {
    /// Rates a ProMotion panel runs evenly (120 / n), lowest first.
    private static let steps = [30, 40, 60, 120]

    static func preferred(inboundFps: Double?, screenMax: Int) -> Int {
        let ceiling = max(screenMax, 1)
        guard let inboundFps, inboundFps > 0 else { return ceiling }
        // A 60 fps stream measures 59-61 over a second, so allow 5% before stepping up.
        let step = steps.first { Double($0) >= inboundFps * 0.95 } ?? ceiling
        return min(step, ceiling)
    }
}

/// A decoded picture ready for the display layer: upright NV12 (or the decoder's own buffer).
struct ComputerDecodedPicture: @unchecked Sendable {
    var buffer: CVPixelBuffer
    var timestampNs: Int64
}

/// Turns WebRTC frames into pixel buffers the display layer can show. Hardware
/// decoders already hand over a pixel buffer; software decoders, cropped frames
/// and rotated frames are converted here, on the caller's (decode) thread, so
/// the main thread only enqueues.
final class ComputerPictureConverter: @unchecked Sendable {
    private let lock = NSLock()
    private var pools: [Int: CVPixelBufferPool] = [:]

    func picture(for frame: RTCVideoFrame) -> ComputerDecodedPicture? {
        if let native = frame.buffer as? RTCCVPixelBuffer, !native.requiresCropping(), frame.rotation == ._0 {
            Self.tagColorIfMissing(native.pixelBuffer)
            return ComputerDecodedPicture(buffer: native.pixelBuffer, timestampNs: frame.timeStampNs)
        }
        let i420 = frame.buffer.toI420()
        guard var buffer = makeBuffer(width: Int(i420.width), height: Int(i420.height)) else { return nil }
        Self.fillNV12(buffer, from: i420)
        if frame.rotation != ._0 {
            guard let rotated = rotate(buffer, by: frame.rotation) else { return nil }
            buffer = rotated
        }
        Self.tagColorIfMissing(buffer)
        return ComputerDecodedPicture(buffer: buffer, timestampNs: frame.timeStampNs)
    }

    private func makeBuffer(width: Int, height: Int) -> CVPixelBuffer? {
        let size = width << 32 | height
        let pool: CVPixelBufferPool? = lock.withLock {
            if let pool = pools[size] { return pool }
            if pools.count >= 4 { pools.removeAll() }
            var created: CVPixelBufferPool?
            CVPixelBufferPoolCreate(nil, nil, [
                kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                kCVPixelBufferWidthKey: width,
                kCVPixelBufferHeightKey: height,
                kCVPixelBufferIOSurfacePropertiesKey: [String: Any](),
            ] as CFDictionary, &created)
            pools[size] = created
            return created
        }
        guard let pool else { return nil }
        var buffer: CVPixelBuffer?
        guard CVPixelBufferPoolCreatePixelBuffer(nil, pool, &buffer) == kCVReturnSuccess else { return nil }
        return buffer
    }

    /// Rotates clockwise by the frame's rotation, as `RTCMTLVideoView` did. Luma is one
    /// byte per pixel; chroma is one Cb/Cr byte pair, rotated as a 16-bit pixel.
    private func rotate(_ source: CVPixelBuffer, by rotation: RTCVideoRotation) -> CVPixelBuffer? {
        let width = CVPixelBufferGetWidth(source), height = CVPixelBufferGetHeight(source)
        let turnsQuarter = rotation == ._90 || rotation == ._270
        guard let target = makeBuffer(width: turnsQuarter ? height : width, height: turnsQuarter ? width : height) else { return nil }
        let constant: Int
        switch rotation {
        case ._90: constant = kRotate90DegreesClockwise
        case ._180: constant = kRotate180DegreesCounterClockwise
        default: constant = kRotate270DegreesClockwise
        }
        CVPixelBufferLockBaseAddress(source, .readOnly)
        CVPixelBufferLockBaseAddress(target, [])
        defer {
            CVPixelBufferUnlockBaseAddress(target, [])
            CVPixelBufferUnlockBaseAddress(source, .readOnly)
        }
        for plane in 0..<2 {
            guard let from = CVPixelBufferGetBaseAddressOfPlane(source, plane),
                  let to = CVPixelBufferGetBaseAddressOfPlane(target, plane) else { return nil }
            var input = vImage_Buffer(
                data: from, height: vImagePixelCount(CVPixelBufferGetHeightOfPlane(source, plane)),
                width: vImagePixelCount(CVPixelBufferGetWidthOfPlane(source, plane)),
                rowBytes: CVPixelBufferGetBytesPerRowOfPlane(source, plane))
            var output = vImage_Buffer(
                data: to, height: vImagePixelCount(CVPixelBufferGetHeightOfPlane(target, plane)),
                width: vImagePixelCount(CVPixelBufferGetWidthOfPlane(target, plane)),
                rowBytes: CVPixelBufferGetBytesPerRowOfPlane(target, plane))
            let error = plane == 0
                ? vImageRotate90_Planar8(&input, &output, UInt8(constant), 0, vImage_Flags(kvImageNoFlags))
                : vImageRotate90_Planar16U(&input, &output, UInt8(constant), 0, vImage_Flags(kvImageNoFlags))
            if error != kvImageNoError { return nil }
        }
        return target
    }

    /// libyuv, which encodes the host's capture, uses BT.601 limited range. A
    /// decoder that does not tag its output would otherwise be read as BT.709.
    private static func tagColorIfMissing(_ buffer: CVPixelBuffer) {
        guard CVBufferCopyAttachment(buffer, kCVImageBufferYCbCrMatrixKey, nil) == nil else { return }
        CVBufferSetAttachment(buffer, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_601_4, .shouldPropagate)
        CVBufferSetAttachment(buffer, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, .shouldPropagate)
        CVBufferSetAttachment(buffer, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2, .shouldPropagate)
    }

    static func fillNV12(_ buffer: CVPixelBuffer, from i420: RTCI420BufferProtocol) {
        CVPixelBufferLockBaseAddress(buffer, [])
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        let width = Int(i420.width), height = Int(i420.height)
        if let luma = CVPixelBufferGetBaseAddressOfPlane(buffer, 0) {
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
            for row in 0..<height {
                memcpy(luma + row * stride, i420.dataY + row * Int(i420.strideY), width)
            }
        }
        if let chroma = CVPixelBufferGetBaseAddressOfPlane(buffer, 1) {
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1)
            for row in 0..<Int(i420.chromaHeight) {
                let target = (chroma + row * stride).assumingMemoryBound(to: UInt8.self)
                let u = i420.dataU + row * Int(i420.strideU)
                let v = i420.dataV + row * Int(i420.strideV)
                for column in 0..<Int(i420.chromaWidth) {
                    target[column * 2] = u[column]
                    target[column * 2 + 1] = v[column]
                }
            }
        }
    }
}

/// Decoded frames arrive on WebRTC's decode thread, where they are converted;
/// the display link draws the newest one on the main thread. Older frames are
/// replaced, never queued.
final class ComputerFrameMailbox: NSObject, RTCVideoRenderer, @unchecked Sendable {
    struct Delivery {
        var picture: ComputerDecodedPicture?
        var clear: Bool
    }

    private let lock = NSLock()
    private let converter = ComputerPictureConverter()
    private var pending: ComputerDecodedPicture?
    private var clearPending = false
    private var arrivedFrames = 0
    private var wakeArmed = false
    private let wake: @Sendable () -> Void

    /// `wake` runs on the main queue when a frame arrives after `armWake()`.
    init(wake: @escaping @Sendable () -> Void) { self.wake = wake }

    func setSize(_ size: CGSize) {}

    func renderFrame(_ frame: RTCVideoFrame?) {
        // Convert before taking the lock: software frames cost real time.
        let converted = frame.flatMap { converter.picture(for: $0) }
        let shouldWake: Bool = lock.withLock {
            if frame != nil {
                arrivedFrames += 1
                guard let converted else { return false }
                pending = converted
                clearPending = false
            } else {
                pending = nil
                clearPending = true
            }
            defer { wakeArmed = false }
            return wakeArmed
        }
        if shouldWake { DispatchQueue.main.async(execute: wake) }
    }

    func take() -> Delivery {
        lock.withLock {
            defer { pending = nil; clearPending = false }
            return Delivery(picture: pending, clear: clearPending)
        }
    }

    var framesArrived: Int { lock.withLock { arrivedFrames } }

    /// False when something is already waiting, so the caller keeps drawing.
    func armWake() -> Bool {
        lock.withLock {
            if pending != nil || clearPending { return false }
            wakeArmed = true
            return true
        }
    }
}

#if canImport(UIKit)
import AVFoundation
import CoreMedia
import CoreVideo
import SwiftUI
import UIKit

extension ComputerDisplayRate {
    static func range(inboundFps: Double?, screenMax: Int) -> CAFrameRateRange {
        let preferred = Float(preferred(inboundFps: inboundFps, screenMax: screenMax))
        return CAFrameRateRange(minimum: min(24, preferred), maximum: preferred, preferred: preferred)
    }
}

/// Draws the picture with an `AVSampleBufferDisplayLayer` on its own display
/// link, so the rate follows the stream up to the screen's maximum instead of
/// the 60 fps default of WebRTC's `RTCMTLVideoView`. The link runs only while
/// frames arrive and sleeps when the picture is still.
public final class ComputerDisplayView: UIView {
    public override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }

    /// Attach this to the picture; it holds the frames this view draws.
    private(set) lazy var mailbox = ComputerFrameMailbox { [weak self] in MainActor.assumeIsolated { self?.resumeLink() } }
    /// Called on the main thread with the timestamp of each frame drawn.
    var onDraw: ((Int64) -> Void)?
    /// Frames drawn since creation; main thread.
    public private(set) var framesDrawn = 0

    /// Hands a frame to this view directly, for tests that exercise the draw path.
    public func renderFrame(_ frame: RTCVideoFrame?) { mailbox.renderFrame(frame) }

    private var displayLayer: AVSampleBufferDisplayLayer { layer as! AVSampleBufferDisplayLayer }
    private var link: CADisplayLink?
    private var formatDescription: CMVideoFormatDescription?
    private var lastActivity: CFTimeInterval = 0
    private var rateWindowStart: CFTimeInterval = 0
    private var rateWindowArrivals = 0
    /// Start and arrival count of the window before the open one, if this stretch of activity has one.
    private var previousWindow: (start: CFTimeInterval, arrivals: Int)?
    /// The display-link rate asked for, in Hz; follows the stream's measured rate.
    public private(set) var requestedFrameRate = 0
    /// True while the link is paused because no frames are arriving.
    public var isLinkSleeping: Bool { link?.isPaused ?? true }

    public override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        displayLayer.videoGravity = .resizeAspect
    }

    public required init?(coder: NSCoder) { nil }

    private var screenMax: Int { window?.windowScene?.screen.maximumFramesPerSecond ?? 60 }

    public override func didMoveToWindow() {
        super.didMoveToWindow()
        if window == nil { stopLink() } else { resumeLink() }
    }

    func stop() {
        stopLink()
        displayLayer.sampleBufferRenderer.flush(removingDisplayedImage: true, completionHandler: nil)
    }

    private func resumeLink() {
        guard window != nil else { return }
        if link == nil {
            let value = CADisplayLink(target: LinkTarget(self), selector: #selector(LinkTarget.tick(_:)))
            value.preferredFrameRateRange = ComputerDisplayRate.range(inboundFps: nil, screenMax: screenMax)
            value.add(to: .main, forMode: .common)
            link = value
            requestedFrameRate = ComputerDisplayRate.preferred(inboundFps: nil, screenMax: screenMax)
        }
        // A woken link keeps the rate it measured before sleeping; the window below
        // raises it again if the stream has sped up.
        lastActivity = CACurrentMediaTime()
        rateWindowStart = 0
        rateWindowArrivals = mailbox.framesArrived
        previousWindow = nil
        link?.isPaused = false
    }

    private func stopLink() {
        link?.invalidate()
        link = nil
    }

    private func setRate(inboundFps: Double?) {
        let preferred = ComputerDisplayRate.preferred(inboundFps: inboundFps, screenMax: screenMax)
        guard preferred != requestedFrameRate else { return }
        requestedFrameRate = preferred
        link?.preferredFrameRateRange = ComputerDisplayRate.range(inboundFps: inboundFps, screenMax: screenMax)
    }

    fileprivate func tick(_ sender: CADisplayLink) {
        let now = sender.timestamp
        let delivery = mailbox.take()
        let renderer = displayLayer.sampleBufferRenderer
        if delivery.clear { renderer.flush(removingDisplayedImage: true, completionHandler: nil) }
        if let picture = delivery.picture {
            enqueue(picture.buffer, on: renderer)
            framesDrawn += 1
            onDraw?(picture.timestampNs)
            lastActivity = now
        }
        // The request follows frames that arrived, not frames drawn: drawing is capped by
        // the rate already requested, so measuring it would never let the rate rise.
        if rateWindowStart == 0 { rateWindowStart = now }
        if now - rateWindowStart >= 1 {
            let arrived = mailbox.framesArrived
            setRate(inboundFps: Double(arrived - rateWindowArrivals) / (now - rateWindowStart))
            previousWindow = (rateWindowStart, rateWindowArrivals)
            rateWindowStart = now
            rateWindowArrivals = arrived
        }
        if now - lastActivity > 0.5, mailbox.armWake() {
            // The sleep comes half a second after the last frame, so the open window holds under a
            // second of a busy stream, and the one before it may end in silence. Measure from the
            // previous window's start to the last frame when there is one.
            // Sparse frames never fill a window, so they settle on what this stretch saw; the frame
            // that woke the link counted before the window began, hence the floor of one frame; the
            // half-second floor on the span keeps a lone frame from reading as a fast stream.
            let start = previousWindow ?? (rateWindowStart, rateWindowArrivals)
            let span = max(lastActivity - start.start, 0.5)
            setRate(inboundFps: Double(max(mailbox.framesArrived - start.arrivals, 1)) / span)
            sender.isPaused = true
        }
    }

    private func enqueue(_ buffer: CVPixelBuffer, on renderer: AVSampleBufferVideoRenderer) {
        if renderer.status == .failed || renderer.requiresFlushToResumeDecoding { renderer.flush() }
        if formatDescription == nil
            || CMVideoFormatDescriptionMatchesImageBuffer(formatDescription!, imageBuffer: buffer) == false {
            var created: CMVideoFormatDescription?
            CMVideoFormatDescriptionCreateForImageBuffer(
                allocator: nil, imageBuffer: buffer, formatDescriptionOut: &created)
            formatDescription = created
        }
        guard let formatDescription else { return }
        var timing = CMSampleTimingInfo(
            duration: .invalid,
            presentationTimeStamp: CMClockGetTime(CMClockGetHostTimeClock()),
            decodeTimeStamp: .invalid)
        var sample: CMSampleBuffer?
        CMSampleBufferCreateReadyWithImageBuffer(
            allocator: nil, imageBuffer: buffer, formatDescription: formatDescription,
            sampleTiming: &timing, sampleBufferOut: &sample)
        guard let sample,
              let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true),
              CFArrayGetCount(attachments) > 0 else { return }
        let dictionary = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
        CFDictionarySetValue(
            dictionary,
            Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
            Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
        renderer.enqueue(sample)
    }

    /// The link holds its target strongly; this keeps the view free to deallocate.
    @MainActor private final class LinkTarget: NSObject {
        weak var view: ComputerDisplayView?
        init(_ view: ComputerDisplayView) { self.view = view }
        @objc func tick(_ sender: CADisplayLink) {
            guard let view else { sender.invalidate(); return }
            view.tick(sender)
        }
    }
}

public struct ComputerVideoView: UIViewRepresentable {
    private let receiver: ComputerReceiver

    public init(receiver: ComputerReceiver) {
        self.receiver = receiver
    }

    public func makeCoordinator() -> Coordinator {
        Coordinator(receiver: receiver)
    }

    /// updateUIView only runs when SwiftUI state changes and the receiver is not
    /// observed, so a view made before the first frame stayed hidden until a
    /// layout change (unfold). The listener shows it when the frame arrives.
    private static func showOnFirstFrame(_ view: ComputerDisplayView, receiver: ComputerReceiver, coordinator: Coordinator) {
        let picture = receiver.picture
        let listener = FirstFrameListener { [weak view, weak picture] in
            MainActor.assumeIsolated {
                guard let view, let picture, picture.hasFrame, view.isHidden else { return }
                view.isHidden = false
                // A folded layout can leave the size unset until now, and the
                // hidden view's link may have idled past the frame.
                view.setNeedsLayout()
                view.layoutIfNeeded()
                picture.replay(to: view.mailbox)
            }
        }
        coordinator.listener = listener
        picture.attach(listener)
    }

    public func makeUIView(context: Context) -> ComputerDisplayView {
        let view = ComputerDisplayView(frame: .zero)
        view.isHidden = !receiver.picture.hasFrame
        view.onDraw = { [weak receiver] timestamp in receiver?.noteFrameDrawn(timestampNs: timestamp) }
        receiver.attach(renderer: view.mailbox)
        Self.showOnFirstFrame(view, receiver: receiver, coordinator: context.coordinator)
        return view
    }

    public func updateUIView(_ view: ComputerDisplayView, context: Context) {
        // Hiding an empty picture really clears an explicit close, including
        // before dismissal finishes.
        view.isHidden = !receiver.picture.hasFrame
        receiver.attach(renderer: view.mailbox)
    }

    public static func dismantleUIView(_ view: ComputerDisplayView, coordinator: Coordinator) {
        if let listener = coordinator.listener { coordinator.receiver.picture.detach(listener) }
        coordinator.listener = nil
        coordinator.receiver.detach(renderer: view.mailbox)
        view.onDraw = nil
        view.stop()
    }

    public final class Coordinator {
        fileprivate let receiver: ComputerReceiver
        fileprivate var listener: FirstFrameListener?

        fileprivate init(receiver: ComputerReceiver) {
            self.receiver = receiver
        }
    }
}
#endif
