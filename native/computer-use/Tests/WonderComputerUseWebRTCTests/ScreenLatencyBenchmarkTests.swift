import CoreVideo
import Foundation
import Darwin
import XCTest
@preconcurrency import WebRTC
@testable import WonderComputerUse
import WonderComputerUseCore

/// Synthetic source -> encoder -> loopback RTP -> decoder -> callback latency.
/// This does not measure ScreenCaptureKit or physical display presentation.
/// See Tests/Benchmarks/README.md for the matrix, metric definitions and commands.
final class ScreenLatencyBenchmarkTests: XCTestCase {
    private struct Configuration {
        let width: Int
        let height: Int
        let framesPerSecond: Int
        let maxFramerate: Int?
        let level52: Bool
        let pixelFormat: BenchPixelFormat
        let maxBitrateBps: Int?
        let pacing: FramePump.Pacing
        /// The production policy: ceiling, bitrate, level and limits from the
        /// publisher's own code instead of the matrix's explicit knobs.
        var publisher = false

        var name: String {
            if publisher { return "\(height)p-\(framesPerSecond)fps-publisher-\(pixelFormat.rawValue)-\(pacing)" }
            return "\(height)p-\(framesPerSecond)fps-\(maxFramerate == nil ? "unset" : "set")-l\(level52 ? "52" : "51")-\(pixelFormat.rawValue)-\(pacing)"
        }
    }

    func testScreenShareLoopbackLatency() throws {
        let environment = ProcessInfo.processInfo.environment
        guard environment["WONDER_LATENCY_BENCH"] == "1" else {
            throw XCTSkip("Set WONDER_LATENCY_BENCH=1 to run the latency benchmark")
        }
        let duration = try setting("WONDER_LATENCY_BENCH_SECONDS", fallback: 12, range: 8...60)
        let warmup = try setting("WONDER_LATENCY_BENCH_WARMUP", fallback: 4, range: 3...30)
        let bitrateScale = try setting("WONDER_LATENCY_BENCH_BITRATE_SCALE", fallback: 1, range: 0...8)
        let pacing: FramePump.Pacing = environment["WONDER_LATENCY_BENCH_PACING"] == "onChange" ? .onChange : .continuous
        var configurations: [Configuration] = []
        if environment["WONDER_LATENCY_BENCH_PUBLISHER"] == "1" {
            for (width, height) in [(1_280, 720), (1_920, 1_080), (2_560, 1_440)] {
                for fps in [15, 30, 60, 120] {
                    let capture = CaptureConfiguration(width: width, height: height, framesPerSecond: fps)
                    configurations.append(Configuration(
                        width: width, height: height, framesPerSecond: fps, maxFramerate: fps,
                        level52: VideoEncodingPolicy.h264Level(forSessionCeiling: fps) == .level5_2,
                        pixelFormat: .nv12,
                        maxBitrateBps: VideoEncodingPolicy.maxBitrateBps(
                            width: capture.width, height: capture.height, framesPerSecond: fps),
                        pacing: pacing, publisher: true))
                }
            }
        } else {
        for (width, height, baseBitrate) in [(1_280, 720, 1_000_000), (1_920, 1_080, 2_000_000), (2_560, 1_440, 4_000_000)] {
            for fps in [15, 30, 60, 120] {
                for explicit in [false, true] {
                    for level52 in [false, true] {
                        for format in BenchPixelFormat.allCases {
                            configurations.append(Configuration(
                                width: width, height: height, framesPerSecond: fps,
                                maxFramerate: explicit ? fps : nil, level52: level52, pixelFormat: format,
                                maxBitrateBps: bitrateScale == 0 ? nil : Int(Double(baseBitrate * fps / 15) * bitrateScale),
                                pacing: pacing
                            ))
                        }
                    }
                }
            }
        }
        }
        if let filter = environment["WONDER_LATENCY_BENCH_FILTER"], !filter.isEmpty {
            // Comma-separated OR substrings allow a selected matrix in one process.
            let filters = filter.split(separator: ",").map(String.init)
            configurations = configurations.filter { config in filters.contains { config.name.contains($0) } }
        }
        XCTAssertFalse(configurations.isEmpty, "Benchmark filter matched no configurations")
        let logger = RTCCallbackLogger()
        if environment["WONDER_LATENCY_BENCH_LOG"] == "1" {
            logger.severity = .info
            logger.start { message in fputs(message, stderr) }
        }
        defer { logger.stop() }
        RTCInitializeSSL()
        if let output = environment["WONDER_LATENCY_BENCH_OUTPUT"] {
            try Data().write(to: URL(fileURLWithPath: output), options: .withoutOverwriting)
        }
        try emit("latency-bench-meta os=\(ProcessInfo.processInfo.operatingSystemVersionString) cores=\(ProcessInfo.processInfo.processorCount) pid=\(getpid()) warmup=\(warmup)s window=\(duration)s cases=\(configurations.count) cpuScope=process-encode-decode-fixture cpu100=one-core")
        for configuration in configurations {
            let encoderFactory = configuration.publisher
                ? BenchmarkEncoderFactory(productionCeiling: configuration.framesPerSecond)
                : BenchmarkEncoderFactory(level52: configuration.level52)
            let factory = RTCPeerConnectionFactory(encoderFactory: encoderFactory, decoderFactory: RTCDefaultVideoDecoderFactory())
            try run(configuration, factory: factory, encoderEvidence: encoderFactory.evidence, warmup: warmup, duration: duration)
        }
    }

    private func emit(_ line: String) throws {
        print(line)
        fflush(stdout)
        if let output = ProcessInfo.processInfo.environment["WONDER_LATENCY_BENCH_OUTPUT"] {
            let handle = try FileHandle(forWritingTo: URL(fileURLWithPath: output))
            defer { try? handle.close() }
            try handle.seekToEnd()
            try handle.write(contentsOf: Data((line + "\n").utf8))
        }
    }

    private func setting(_ name: String, fallback: Double, range: ClosedRange<Double>) throws -> Double {
        guard let raw = ProcessInfo.processInfo.environment[name] else { return fallback }
        let value = try XCTUnwrap(Double(raw), "Invalid \(name)")
        guard range.contains(value) else { throw NSError(domain: "latency-bench: \(name) outside \(range)", code: 1) }
        return value
    }

    private func run(_ benchmark: Configuration, factory: RTCPeerConnectionFactory, encoderEvidence: BenchmarkEncoderEvidence, warmup: Double, duration: Double) throws {
        let configuration = RTCConfiguration()
        configuration.iceServers = []
        configuration.sdpSemantics = .unifiedPlan
        configuration.continualGatheringPolicy = .gatherContinually
        let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
        let left = try XCTUnwrap(factory.peerConnection(with: configuration, constraints: constraints, delegate: nil))
        let right = try XCTUnwrap(factory.peerConnection(with: configuration, constraints: constraints, delegate: nil))
        defer { left.close(); right.close() }
        let leftDelegate = BenchPeerDelegate()
        let rightDelegate = BenchPeerDelegate()
        left.delegate = leftDelegate
        right.delegate = rightDelegate
        let router = BenchCandidateRouter(left: left, right: right)
        leftDelegate.onCandidate = { router.route($0, fromLeft: true) }
        rightDelegate.onCandidate = { router.route($0, fromLeft: false) }

        let source = factory.videoSource(forScreenCast: true)
        source.adaptOutputFormat(
            toWidth: Int32(benchmark.width), height: Int32(benchmark.height), fps: Int32(benchmark.framesPerSecond)
        )
        let capturer = RTCVideoCapturer(delegate: source)
        let track = factory.videoTrack(with: source, trackId: "latency-bench-video")
        let sender = try XCTUnwrap(left.add(track, streamIds: ["latency-bench"]))
        let parameters = sender.parameters
        if benchmark.publisher {
            WebRTCPublisher.applyEncodingLimits(to: parameters, configuration: CaptureConfiguration(
                width: benchmark.width, height: benchmark.height, framesPerSecond: benchmark.framesPerSecond))
        } else {
            if let maxFramerate = benchmark.maxFramerate {
                parameters.encodings.forEach { $0.maxFramerate = NSNumber(value: maxFramerate) }
            }
            if let maxBitrateBps = benchmark.maxBitrateBps {
                parameters.encodings.forEach { $0.maxBitrateBps = NSNumber(value: maxBitrateBps) }
            }
            parameters.degradationPreference = NSNumber(value: RTCDegradationPreference.maintainResolution.rawValue)
        }
        sender.parameters = parameters

        let probe = LatencyProbe()
        let renderer = LumaRenderer(probe: probe)
        let negotiated = expectation(description: "offer and answer applied")
        let offerConstraints = RTCMediaConstraints(
            mandatoryConstraints: [kRTCMediaConstraintsOfferToReceiveVideo: kRTCMediaConstraintsValueFalse],
            optionalConstraints: nil
        )
        left.offer(for: offerConstraints) { offer, error in
            XCTAssertNil(error)
            guard let offer else { return }
            left.setLocalDescription(offer) { error in
                XCTAssertNil(error)
                right.setRemoteDescription(offer) { error in
                    XCTAssertNil(error)
                    router.markRightRemoteDescriptionSet()
                    right.answer(for: offerConstraints) { answer, error in
                        XCTAssertNil(error)
                        guard let answer else { return }
                        right.setLocalDescription(answer) { error in
                            XCTAssertNil(error)
                            left.setRemoteDescription(answer) { error in
                                XCTAssertNil(error)
                                router.markLeftRemoteDescriptionSet()
                                negotiated.fulfill()
                            }
                        }
                    }
                }
            }
        }
        wait(for: [negotiated], timeout: 15)
        let remoteTrack = try XCTUnwrap(right.receivers.lazy.compactMap { $0.track as? RTCVideoTrack }.first)
        remoteTrack.add(renderer)

        let pump = FramePump(
            capturer: capturer, width: benchmark.width, height: benchmark.height,
            framesPerSecond: benchmark.framesPerSecond, pacing: benchmark.pacing,
            pixelFormat: benchmark.pixelFormat, probe: probe
        )
        defer { pump.stop(); remoteTrack.remove(renderer) }
        pump.start()
        delay(warmup)
        let before = try statistics(left)
        let inboundBefore = try statistics(right)
        let pumpBefore = pump.snapshot()
        let cpuBefore = processCPUSeconds()
        let began = nowNanoseconds()
        probe.beginRecording()
        delay(duration)
        let after = try statistics(left)
        let inboundAfter = try statistics(right)
        let elapsed = Double(nowNanoseconds() - began) / 1_000_000_000
        let cpu = (processCPUSeconds() - cpuBefore) / elapsed * 100
        let pumpAfter = pump.snapshot()
        pump.stop()
        let latency = probe.snapshot()
        let samples = latency.milliseconds.sorted()
        let sourceFPS = Double(pumpAfter.frames - pumpBefore.frames) / elapsed
        let outbound = BenchStats(report: after, type: "outbound-rtp")
        let oldOutbound = BenchStats(report: before, type: "outbound-rtp")
        let inbound = BenchStats(report: inboundAfter, type: "inbound-rtp")
        let oldInbound = BenchStats(report: inboundBefore, type: "inbound-rtp")
        let seconds = (outbound.timestamp - oldOutbound.timestamp) / 1_000_000
        let sent = outbound.number("framesSent") - oldOutbound.number("framesSent")
        let encoded = outbound.number("framesEncoded") - oldOutbound.number("framesEncoded")
        let achieved = sent / seconds
        let encodeMS = (outbound.number("totalEncodeTime") - oldOutbound.number("totalEncodeTime")) * 1_000 / encoded
        let decoded = inbound.number("framesDecoded") - oldInbound.number("framesDecoded")
        let decodeMS = (inbound.number("totalDecodeTime") - oldInbound.number("totalDecodeTime")) * 1_000 / decoded
        let emitted = inbound.number("jitterBufferEmittedCount") - oldInbound.number("jitterBufferEmittedCount")
        let jitterMS = (inbound.number("jitterBufferDelay") - oldInbound.number("jitterBufferDelay")) * 1_000 / emitted
        let passed = achieved >= Double(benchmark.framesPerSecond) * 0.95
        // Explanations are hypotheses from measured counters, never an assertion
        // that a deliberately capped/over-level diagnostic case must hit target.
        let reason: String
        if passed { reason = "none" }
        else if benchmark.pacing == .onChange { reason = "sparse-source" }
        else if sourceFPS < Double(benchmark.framesPerSecond) * 0.95 { reason = "source-pacing" }
        else if outbound.string("qualityLimitationReason") != "none" { reason = "webrtc-" + outbound.string("qualityLimitationReason") }
        else if encodeMS > 1_000 / Double(benchmark.framesPerSecond) { reason = "encode-over-frame-interval-investigate" }
        else { reason = "sender-or-codec-cap-investigate" }
        try emit(String(
            format: "latency-bench config=%@ target=%d maxFramerate=%@ maxBitrate=%d sourceFPS=%.2f outboundFPS=%.2f statsFPS=%.2f encodedFPS=%.2f receivedFPS=%.2f encoder=%@ codec=%@ sps=%@ sent=%.0fx%.0f avgEncode=%.2fms avgDecode=%.2fms jitter=%.2fms samples=%d pendingChanges=%d median=%.2fms p90=%.2fms cpu=%.2f%% fixture=%.2fms bitrate=%.3fMbps qualityLimitation=%@ pass95=%@ reasonInferred=%@ load1=%.2f thermal=%d window=%.3fs",
            benchmark.name, benchmark.framesPerSecond, benchmark.maxFramerate.map(String.init) ?? "unset",
            benchmark.maxBitrateBps ?? 0, sourceFPS, achieved, outbound.number("framesPerSecond"), encoded / seconds,
            decoded / ((inbound.timestamp - oldInbound.timestamp) / 1_000_000),
            outbound.string("encoderImplementation"), outbound.codec, encoderEvidence.summary(),
            outbound.number("frameWidth"), outbound.number("frameHeight"), encodeMS, decodeMS, jitterMS,
            samples.count, latency.pending, percentile(samples, 0.5), percentile(samples, 0.9), cpu,
            Double(pumpAfter.generationNS - pumpBefore.generationNS) / Double(pumpAfter.frames - pumpBefore.frames) / 1_000_000,
            (outbound.number("bytesSent") - oldOutbound.number("bytesSent")) * 8 / seconds / 1_000_000,
            outbound.string("qualityLimitationReason"), passed ? "yes" : "no", reason, loadAverage(),
            ProcessInfo.processInfo.thermalState.rawValue, seconds
        ))
        XCTAssertNotEqual(encoderEvidence.summary(), "unobserved", "No H.264 SPS observed")
        XCTAssertNil(pumpAfter.error)
        XCTAssertGreaterThan(seconds, duration * 0.9)
        XCTAssertGreaterThanOrEqual(samples.count, Int(duration * 1.5), "Too few observed changes: \(benchmark.name)")
        XCTAssertGreaterThan(sent, 0, "No outbound video")
        XCTAssertEqual(outbound.number("frameWidth"), Double(benchmark.width), "Resolution must be held")
        XCTAssertEqual(outbound.number("frameHeight"), Double(benchmark.height), "Resolution must be held")
        XCTAssertEqual(outbound.codec.lowercased(), "video/h264", "Unexpected codec fallback")
    }

    private func delay(_ seconds: Double) {
        let elapsed = expectation(description: "bounded benchmark window")
        DispatchQueue.global().asyncAfter(deadline: .now() + seconds) { elapsed.fulfill() }
        wait(for: [elapsed], timeout: seconds + 5)
    }

    private func statistics(_ peer: RTCPeerConnection) throws -> RTCStatisticsReport {
        let collected = expectation(description: "benchmark stats")
        let collector = BenchStatsCollector()
        peer.statistics { collector.save($0); collected.fulfill() }
        wait(for: [collected], timeout: 5)
        return try XCTUnwrap(collector.load(), "WebRTC statistics callback timed out")
    }

    private func processCPUSeconds() -> Double {
        var usage = rusage()
        guard getrusage(RUSAGE_SELF, &usage) == 0 else { return .nan }
        return Double(usage.ru_utime.tv_sec + usage.ru_stime.tv_sec)
            + Double(usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) / 1_000_000
    }

    private func percentile(_ sorted: [Double], _ fraction: Double) -> Double {
        guard !sorted.isEmpty else { return .nan }
        let index = Int((Double(sorted.count - 1) * fraction).rounded())
        return sorted[index]
    }

    private func loadAverage() -> Double {
        var load = [Double](repeating: 0, count: 3)
        return getloadavg(&load, 3) > 0 ? load[0] : .nan
    }
}

private func nowNanoseconds() -> UInt64 { DispatchTime.now().uptimeNanoseconds }

/// Matches a unique 16-bit pixel marker, so a delayed frame cannot be
/// mistaken for a newer black/white flip and under-report latency.
fileprivate final class LatencyProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var changes: [Int: UInt64] = [:]
    private var latencies: [Double] = []
    private var recording = false

    func beginRecording() {
        lock.lock()
        recording = true
        lock.unlock()
    }

    func contentChanged(at time: UInt64, marker: Int) {
        lock.lock()
        if recording { changes[marker] = time }
        lock.unlock()
    }

    func rendered(marker: Int, at time: UInt64) {
        lock.lock()
        defer { lock.unlock() }
        guard let changedAt = changes.removeValue(forKey: marker), time >= changedAt else { return }
        latencies.append(Double(time - changedAt) / 1_000_000)
    }

    func snapshot() -> (milliseconds: [Double], pending: Int) {
        lock.lock()
        defer { lock.unlock() }
        return (latencies, changes.count)
    }
}

/// Paces frames like ScreenCaptureKit with minimumFrameInterval = 1/fps: the
/// content changes at a random phase, but a frame only leaves on the next tick.
final class FramePump: @unchecked Sendable {
    enum Pacing: String, CustomStringConvertible {
        /// Something on screen always moves, so every tick carries a frame.
        case continuous
        /// ScreenCaptureKit drops idle frames: a frame leaves on the first
        /// 60 Hz refresh after a change, at least 1/fps after the previous one.
        case onChange

        var description: String { rawValue }
    }

    private static let refreshNanoseconds: UInt64 = 1_000_000_000 / 60
    private let pacing: Pacing
    private var contentChangedSinceDelivery = false
    private var lastDelivery: UInt64 = 0
    private let capturer: RTCVideoCapturer
    private let framesPerSecond: Int
    private let probe: LatencyProbe
    private let width: Int
    private let height: Int
    private let pixelFormat: BenchPixelFormat
    private var frames = 0
    private var generationNS: UInt64 = 0
    private var failure: String?
    private let queue = DispatchQueue(label: "latency-bench.pump", qos: .userInteractive)
    private var timer: DispatchSourceTimer?
    private var marker = 0
    private var nextFlip: UInt64 = 0
    private var startedAt: UInt64 = 0

    fileprivate init(
        capturer: RTCVideoCapturer, width: Int, height: Int, framesPerSecond: Int, pacing: Pacing, pixelFormat: BenchPixelFormat, probe: LatencyProbe
    ) {
        self.capturer = capturer
        self.pacing = pacing
        self.framesPerSecond = framesPerSecond
        self.probe = probe
        self.width = width
        self.height = height
        self.pixelFormat = pixelFormat
    }

    func start() {
        queue.async { [self] in
            startedAt = nowNanoseconds()
            nextFlip = startedAt + Self.flipInterval()
            let timer = DispatchSource.makeTimerSource(flags: .strict, queue: queue)
            let interval = pacing == .continuous ? frameInterval : Self.refreshNanoseconds
            timer.schedule(deadline: .now(), repeating: .nanoseconds(Int(interval)), leeway: .nanoseconds(0))
            timer.setEventHandler { [weak self] in self?.tick() }
            self.timer = timer
            timer.resume()
        }
    }

    func stop() {
        queue.sync {
            timer?.cancel()
            timer = nil
        }
    }

    func snapshot() -> (frames: Int, generationNS: UInt64, error: String?) {
        queue.sync { (frames, generationNS, failure) }
    }

    private var frameInterval: UInt64 { 1_000_000_000 / UInt64(framesPerSecond) }

    private func tick() {
        let now = nowNanoseconds()
        if now >= nextFlip {
            contentChangedSinceDelivery = true
            // The change happened at nextFlip (a random phase between ticks);
            // this tick is the first capture that can include it.
            marker += 1
            probe.contentChanged(at: nextFlip, marker: marker)
            nextFlip += Self.flipInterval()
            if nextFlip < now { nextFlip = now + Self.flipInterval() }
        }
        if pacing == .onChange {
            // Allow 2 ms of timer slack so a 30 fps cap is not rounded down
            // to the 20 fps refresh grid.
            guard contentChangedSinceDelivery, now &- lastDelivery + 2_000_000 >= frameInterval else { return }
        }
        contentChangedSinceDelivery = false
        lastDelivery = now
        do {
            // Fresh allocation and pixel writes every tick, including the timed
            // interval. No retained CVPixelBuffer/encoded-frame cache.
            let pixelBuffer = try Self.makeBuffer(width: width, height: height,
                                                  marker: marker, format: pixelFormat, sequence: frames)
            generationNS += nowNanoseconds() - now
            let frame = RTCVideoFrame(buffer: RTCCVPixelBuffer(pixelBuffer: pixelBuffer), rotation: ._0, timeStampNs: Int64(now))
            capturer.delegate?.capturer(capturer, didCapture: frame)
            frames += 1
        } catch { failure = String(describing: error) }
    }

    /// About 300 ms with jitter, so flips land at arbitrary phases of the tick.
    private static func flipInterval() -> UInt64 {
        UInt64.random(in: 280_000_000...360_000_000)
    }

    /// A textured background with changing marker cells and a moving strip.
    /// Every source frame is freshly allocated and all pixels are written.
    private static func makeBuffer(width: Int, height: Int, marker: Int, format: BenchPixelFormat, sequence: Int) throws -> CVPixelBuffer {
        var buffer: CVPixelBuffer?
        let attributes: [String: Any] = [kCVPixelBufferIOSurfacePropertiesKey as String: [String: Any]()]
        let status = CVPixelBufferCreate(
            kCFAllocatorDefault, width, height, format.fourCC, attributes as CFDictionary, &buffer
        )
        guard status == kCVReturnSuccess, let buffer else {
            throw NSError(domain: "latency-bench", code: Int(status))
        }
        CVPixelBufferLockBaseAddress(buffer, [])
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        let planar = format == .nv12
        let base = (planar ? CVPixelBufferGetBaseAddressOfPlane(buffer, 0) : CVPixelBufferGetBaseAddress(buffer))!
            .assumingMemoryBound(to: UInt8.self)
        let stride = planar ? CVPixelBufferGetBytesPerRowOfPlane(buffer, 0) : CVPixelBufferGetBytesPerRow(buffer)
        if planar {
            let uv = CVPixelBufferGetBaseAddressOfPlane(buffer, 1)!
            memset(uv, 128, CVPixelBufferGetBytesPerRowOfPlane(buffer, 1) * CVPixelBufferGetHeightOfPlane(buffer, 1))
        }
        var generator = SplitMix(seed: 0x5EED)
        for y in 0..<height {
            let row = base + y * stride
            for x in 0..<width {
                let inPatch = x >= width / 3 && x < width * 2 / 3 && y >= height / 3 && y < height * 2 / 3
                let detail: UInt8 = generator.next() % 7 == 0 ? 40 : 230
                // Small moving strip guarantees distinct continuous frames;
                // the central patch retains P1's change-latency fixture.
                let moving = y < 32 && (x + sequence * 8) % width < 64
                let value: UInt8
                if inPatch {
                    let column = (x - width / 3) * 4 / (width * 2 / 3 - width / 3)
                    let row = (y - height / 3) * 4 / (height * 2 / 3 - height / 3)
                    value = marker & (1 << (row * 4 + column)) == 0 ? 0 : 255
                } else { value = moving ? 0 : detail }
                if planar {
                    row[x] = UInt8(16 + Int(value) * 219 / 255)
                } else {
                    let pixel = row + x * 4
                    pixel[0] = value; pixel[1] = value; pixel[2] = value; pixel[3] = 255
                }
            }
        }
        return buffer
    }
}

private struct SplitMix {
    var state: UInt64
    init(seed: UInt64) { state = seed }
    mutating func next() -> UInt64 {
        state &+= 0x9E37_79B9_7F4A_7C15
        var z = state
        z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
        z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
        return z ^ (z >> 31)
    }
}

/// Reads the sixteen central marker cells without converting an entire NV12 frame.
private final class LumaRenderer: NSObject, RTCVideoRenderer, @unchecked Sendable {
    private let probe: LatencyProbe

    init(probe: LatencyProbe) { self.probe = probe }

    func setSize(_ size: CGSize) {}

    func renderFrame(_ frame: RTCVideoFrame?) {
        guard let frame, let marker = Self.centralMarker(frame.buffer) else { return }
        probe.rendered(marker: marker, at: nowNanoseconds())
    }

    private static func centralMarker(_ buffer: RTCVideoFrameBuffer) -> Int? {
        if let cv = buffer as? RTCCVPixelBuffer {
            let pixelBuffer = cv.pixelBuffer
            CVPixelBufferLockBaseAddress(pixelBuffer, .readOnly)
            defer { CVPixelBufferUnlockBaseAddress(pixelBuffer, .readOnly) }
            let format = CVPixelBufferGetPixelFormatType(pixelBuffer)
            switch format {
            case kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange:
                guard let base = CVPixelBufferGetBaseAddressOfPlane(pixelBuffer, 0) else { return nil }
                return marker(
                    base.assumingMemoryBound(to: UInt8.self),
                    stride: CVPixelBufferGetBytesPerRowOfPlane(pixelBuffer, 0),
                    width: CVPixelBufferGetWidthOfPlane(pixelBuffer, 0),
                    height: CVPixelBufferGetHeightOfPlane(pixelBuffer, 0),
                    bytesPerPixel: 1
                )
            case kCVPixelFormatType_32BGRA:
                guard let base = CVPixelBufferGetBaseAddress(pixelBuffer) else { return nil }
                return marker(
                    base.assumingMemoryBound(to: UInt8.self).advanced(by: 1),
                    stride: CVPixelBufferGetBytesPerRow(pixelBuffer),
                    width: CVPixelBufferGetWidth(pixelBuffer),
                    height: CVPixelBufferGetHeight(pixelBuffer),
                    bytesPerPixel: 4
                )
            default:
                break
            }
        }
        let i420 = buffer.toI420()
        return marker(i420.dataY, stride: Int(i420.strideY), width: Int(i420.width), height: Int(i420.height), bytesPerPixel: 1)
    }

    private static func marker(
        _ base: UnsafePointer<UInt8>, stride: Int, width: Int, height: Int, bytesPerPixel: Int
    ) -> Int {
        var result = 0
        let patchWidth = width * 2 / 3 - width / 3
        let patchHeight = height * 2 / 3 - height / 3
        for bit in 0..<16 {
            let x = width / 3 + (2 * (bit % 4) + 1) * patchWidth / 8
            let y = height / 3 + (2 * (bit / 4) + 1) * patchHeight / 8
            var total = 0
            for dy in -2..<2 {
                for dx in -2..<2 { total += Int(base[(y + dy) * stride + (x + dx) * bytesPerPixel]) }
            }
            if total > 128 * 16 { result |= 1 << bit }
        }
        return result
    }
}

private final class BenchCandidateRouter: @unchecked Sendable {
    private let lock = NSLock()
    private weak var left: RTCPeerConnection?
    private weak var right: RTCPeerConnection?
    private var leftReady = false
    private var rightReady = false
    private var leftPending: [RTCIceCandidate] = []
    private var rightPending: [RTCIceCandidate] = []

    init(left: RTCPeerConnection, right: RTCPeerConnection) {
        self.left = left
        self.right = right
    }

    func route(_ candidate: RTCIceCandidate, fromLeft: Bool) {
        lock.lock()
        let peer = fromLeft ? right : left
        let ready = fromLeft ? rightReady : leftReady
        if !ready {
            if fromLeft { rightPending.append(candidate) } else { leftPending.append(candidate) }
            lock.unlock()
            return
        }
        lock.unlock()
        peer?.add(candidate) { _ in }
    }

    func markRightRemoteDescriptionSet() { drain(left: false) }
    func markLeftRemoteDescriptionSet() { drain(left: true) }

    private func drain(left isLeft: Bool) {
        lock.lock()
        let pending: [RTCIceCandidate]
        let peer: RTCPeerConnection?
        if isLeft {
            leftReady = true
            pending = leftPending
            leftPending.removeAll()
            peer = left
        } else {
            rightReady = true
            pending = rightPending
            rightPending.removeAll()
            peer = right
        }
        lock.unlock()
        pending.forEach { peer?.add($0) { _ in } }
    }
}

private final class BenchPeerDelegate: NSObject, RTCPeerConnectionDelegate, @unchecked Sendable {
    var onCandidate: ((RTCIceCandidate) -> Void)?

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {}
    func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) { onCandidate?(candidate) }
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {}
}
