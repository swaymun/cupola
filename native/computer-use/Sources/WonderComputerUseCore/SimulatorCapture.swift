import CoreImage
import CoreMedia
import Foundation
import WonderSimulatorCapture

/// When a still Simulator's last picture is sent again: every 0.2 s for a few
/// seconds after a start or a new viewer link, then once a second.
public struct SimulatorRefreshCadence: Sendable, Equatable {
    public static let quickInterval = 0.2
    public static let idleInterval = 1.0
    public static let quickWindow = 3.0
    /// A tick can fire a little early; this much slack still counts as due.
    public static let slack = 0.05

    public private(set) var quickUntil = 0.0

    public init() {}

    public mutating func startQuickPhase(at now: Double) { quickUntil = now + Self.quickWindow }

    public func resendIsDue(now: Double, lastDelivery: Double) -> Bool {
        let wait = now < quickUntil ? Self.quickInterval : Self.idleInterval
        return now - lastDelivery >= wait - Self.slack
    }
}

/// CoreSimulator calls and framebuffer copies stay off the app run loop. The
/// helper process supplies the outer startup deadline if Apple's service hangs.
final class SimulatorCapture: @unchecked Sendable {
    private let queue = DispatchQueue(label: "com.wonder.simulator", qos: .userInitiated)
    private let native = WonderSimulatorCapture()
    private let context = CIContext(options: [.cacheIntermediates: false])
    private let lock = NSLock()
    private var active = true
    private var configuration: CaptureConfiguration
    private var latestFrame: CVPixelBuffer?
    private var latestSource: CaptureSourceDescriptor?
    private var running = false
    private var sendFrame: (@Sendable (CVPixelBuffer, CaptureSourceDescriptor) -> Void)?
    /// A Simulator that is not repainting sends no frames, so the encoder would
    /// have nothing to give a viewer who connects after the last one (or lost
    /// the one sent while connecting). While running, the last picture is sent
    /// again: quickly for a few seconds after a start or a new link, then
    /// once a second as a keepalive.
    private static let refreshTick: DispatchTimeInterval = .milliseconds(200)
    private var refreshTimer: DispatchSourceTimer?
    private var lastDelivery = 0.0
    private var cadence = SimulatorRefreshCadence()

    init(configuration: CaptureConfiguration) { self.configuration = configuration }

    static func developerDirectory() -> String? {
        let process = Process()
        let pipe = Pipe()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/xcode-select")
        process.arguments = ["--print-path"]
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        do { try process.run() } catch { return nil }
        // This system command has a small, fixed output. Termination is bounded
        // even if xcode-select itself gets stuck; no shell or user command runs.
        let deadline = Date().addingTimeInterval(2)
        while process.isRunning && Date() < deadline { Thread.sleep(forTimeInterval: 0.01) }
        if process.isRunning { kill(process.processIdentifier, SIGKILL) }
        process.waitUntilExit()
        guard process.terminationStatus == 0,
              let value = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) else { return nil }
        let path = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return path.hasPrefix("/") && path.count < 4096 ? path : nil
    }

    static func bootedDevices() -> [[String: String]]? {
        guard let developer = developerDirectory() else { return nil }
        return try? WonderSimulatorCapture.bootedDevices(developer)
    }

    /// A device that has just booted can be listed before its display exists.
    /// Attaching looks again for this long, inside the helper's own deadline.
    private static let surfaceWait: TimeInterval = 8
    private static let surfaceRetry: DispatchTimeInterval = .milliseconds(400)

    func attach(sourceID: String,
                selected: @escaping @Sendable (CaptureSourceDescriptor?) -> Void,
                frame: @escaping @Sendable (CVPixelBuffer, CaptureSourceDescriptor) -> Void,
                removed: @escaping @Sendable () -> Void) {
        let deadline = Date().addingTimeInterval(Self.surfaceWait)
        queue.async { [self] in attempt(sourceID: sourceID, deadline: deadline, selected: selected, frame: frame, removed: removed) }
    }

    /// One try on the capture queue. Without a renderable surface yet it
    /// schedules another instead of blocking the queue; `stop()` ends the
    /// retries because every try first checks `active`.
    private func attempt(sourceID: String, deadline: Date,
                         selected: @escaping @Sendable (CaptureSourceDescriptor?) -> Void,
                         frame: @escaping @Sendable (CVPixelBuffer, CaptureSourceDescriptor) -> Void,
                         removed: @escaping @Sendable () -> Void) {
        guard lock.withLock({ active }) else { return }
        guard sourceID.hasPrefix("simulator:"),
              let uuid = UUID(uuidString: String(sourceID.dropFirst("simulator:".count))),
              let developer = Self.developerDirectory(),
              let devices = try? WonderSimulatorCapture.bootedDevices(developer),
              let device = devices.first(where: { $0["id"] == "simulator:\(uuid.uuidString)" }) else {
            selected(nil); return
        }
        let title = device["name"] ?? "Simulator"
        var announced = false
        var attached = false
        do {
            try native.attach(uuid.uuidString, developer: developer, queue: queue, frame: { [weak self] pixel in
                guard let self, self.lock.withLock({ self.active }) else { return }
                let width = CVPixelBufferGetWidth(pixel), height = CVPixelBufferGetHeight(pixel)
                let source = CaptureSourceDescriptor(id: sourceID, kind: .simulator, title: title,
                    width: width, height: height, scale: 1,
                    contentRect: CaptureRect(x: 0, y: 0, width: Double(width), height: Double(height)))
                self.latestFrame = pixel
                self.latestSource = source
                if !announced {
                    announced = true
                    self.native.enabled = false
                    selected(source)
                } else if self.running { self.deliver(pixel, source: source, frame: frame) }
            }, removed: {
                guard self.lock.withLock({ self.active }) else { return }
                removed()
            })
            attached = true
        } catch {}
        if attached && announced {
            sendFrame = { [weak self] pixel, source in self?.deliver(pixel, source: source, frame: frame) }
            return
        }
        // A booted device without a renderable surface cannot be selected yet.
        native.detach()
        guard Date() < deadline else { selected(nil); return }
        queue.asyncAfter(deadline: .now() + Self.surfaceRetry) { [self] in
            attempt(sourceID: sourceID, deadline: deadline, selected: selected, frame: frame, removed: removed)
        }
    }

    func start(source: CaptureSourceDescriptor) {
        queue.async { [self] in
            guard lock.withLock({ active }) else { return }
            running = true
            native.enabled = true
            if let pixel = latestFrame { sendFrame?(pixel, latestSource ?? source) }
            cadence.startQuickPhase(at: Self.now())
            startRefresh()
        }
    }

    /// A viewer's link just came up: resend the picture now and keep resending quickly.
    func redraw() {
        queue.async { [self] in
            guard running, lock.withLock({ active }) else { return }
            cadence.startQuickPhase(at: Self.now())
            resendLatest()
        }
    }

    private static func now() -> Double { ProcessInfo.processInfo.systemUptime }

    private func startRefresh() {
        guard refreshTimer == nil else { return }
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + Self.refreshTick, repeating: Self.refreshTick, leeway: .milliseconds(50))
        timer.setEventHandler { [weak self] in
            guard let self, self.running else { return }
            if self.cadence.resendIsDue(now: Self.now(), lastDelivery: self.lastDelivery) { self.resendLatest() }
        }
        refreshTimer = timer
        timer.resume()
    }

    private func stopRefresh() { refreshTimer?.cancel(); refreshTimer = nil }

    private func resendLatest() {
        if let pixel = latestFrame, let source = latestSource { sendFrame?(pixel, source) }
    }

    func pause() { queue.async { [self] in running = false; native.enabled = false; stopRefresh() } }
    func configure(_ value: CaptureConfiguration) {
        queue.async { [self] in
            configuration = value
            // A static Simulator has no new frame callback. Redraw its last
            // immutable image when quality changes, without polling pixels.
            if running, let pixel = latestFrame, let source = latestSource { sendFrame?(pixel, source) }
        }
    }
    func stop() {
        lock.withLock { active = false }
        queue.async { [self] in stopRefresh(); native.detach(); latestFrame = nil; latestSource = nil; sendFrame = nil; running = false }
    }

    private func deliver(_ pixel: CVPixelBuffer, source: CaptureSourceDescriptor,
                         frame: @Sendable (CVPixelBuffer, CaptureSourceDescriptor) -> Void) {
        guard lock.withLock({ active }) else { return }
        lastDelivery = Self.now()
        var output: CVPixelBuffer?
        guard CVPixelBufferCreate(nil, configuration.width, configuration.height, kCVPixelFormatType_32BGRA,
              [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &output) == kCVReturnSuccess,
              let output else { return }
        let destination = configuration.centeredDestinationRect(for: source)
        let rect = CGRect(x: destination.x, y: destination.y, width: destination.width, height: destination.height)
        let input = CIImage(cvPixelBuffer: pixel)
            .transformed(by: CGAffineTransform(scaleX: rect.width / Double(source.width), y: rect.height / Double(source.height)))
            .transformed(by: CGAffineTransform(translationX: rect.minX, y: rect.minY))
        let canvas = CGRect(x: 0, y: 0, width: configuration.width, height: configuration.height)
        context.render(input.composited(over: CIImage(color: .black)).cropped(to: canvas), to: output)
        guard lock.withLock({ active }) else { return }
        frame(output, source)
    }
}
