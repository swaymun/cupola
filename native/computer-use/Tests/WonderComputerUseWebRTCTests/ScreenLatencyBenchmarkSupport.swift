import CoreVideo
import Foundation
@preconcurrency import WebRTC
@testable import WonderComputerUse
import WonderComputerUseCore

enum BenchPixelFormat: String, CaseIterable {
    case bgra = "BGRA"
    case nv12 = "420v"

    var fourCC: OSType {
        self == .bgra ? kCVPixelFormatType_32BGRA : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
    }
}

/// Keep production policy unchanged. Level 5.1 exercises its real factory;
/// level 5.2 changes only the encoder's profile-level-id, exactly as P1 does.
final class BenchmarkEncoderFactory: NSObject, RTCVideoEncoderFactory {
    private let publisher = ScreenShareVideoEncoderFactory()
    private let level52: Bool
    let evidence = BenchmarkEncoderEvidence()

    init(level52: Bool) { self.level52 = level52 }

    /// The production factory exactly as the publisher configures it for a
    /// session whose ceiling is `framesPerSecond`.
    init(productionCeiling framesPerSecond: Int) {
        level52 = false
        publisher.h264Level = ScreenShareVideoEncoderFactory.rtcLevel(
            VideoEncodingPolicy.h264Level(forSessionCeiling: framesPerSecond))
    }

    func supportedCodecs() -> [RTCVideoCodecInfo] { publisher.supportedCodecs() }

    func createEncoder(_ info: RTCVideoCodecInfo) -> (any RTCVideoEncoder)? {
        guard level52, info.name == kRTCVideoCodecH264Name,
              let hex = info.parameters["profile-level-id"],
              let parsed = RTCH264ProfileLevelId(hexString: hex),
              let raised = RTCH264ProfileLevelId(profile: parsed.profile, level: .level5_2) else {
            return publisher.createEncoder(info).map { BenchmarkEncoder(base: $0, evidence: evidence) }
        }
        var parameters = info.parameters
        parameters["profile-level-id"] = raised.hexString
        let encoder = RTCVideoEncoderH264(codecInfo: RTCVideoCodecInfo(name: info.name, parameters: parameters))
        return BenchmarkEncoder(base: encoder, evidence: evidence)
    }
}

/// Observe the emitted SPS, not just the requested profile-level-id. Read only
/// keyframe headers; all encoder methods and native-buffer capabilities forward.
private final class BenchmarkEncoder: NSObject, RTCVideoEncoder {
    let base: any RTCVideoEncoder
    let evidence: BenchmarkEncoderEvidence

    init(base: any RTCVideoEncoder, evidence: BenchmarkEncoderEvidence) {
        self.base = base
        self.evidence = evidence
    }

    func setCallback(_ callback: RTCVideoEncoderCallback?) {
        guard let callback else { base.setCallback(nil); return }
        let evidence = evidence
        base.setCallback { frame, info in
            if frame.frameType == .videoFrameKey { evidence.observe(frame.buffer) }
            return callback(frame, info)
        }
    }

    func startEncode(with settings: RTCVideoEncoderSettings, numberOfCores: Int32) -> Int {
        base.startEncode(with: settings, numberOfCores: numberOfCores)
    }
    func release() -> Int { base.release() }
    func encode(_ frame: RTCVideoFrame, codecSpecificInfo info: (any RTCCodecSpecificInfo)?, frameTypes: [NSNumber]) -> Int {
        base.encode(frame, codecSpecificInfo: info, frameTypes: frameTypes)
    }
    func setBitrate(_ bitrateKbit: UInt32, framerate: UInt32) -> Int32 { base.setBitrate(bitrateKbit, framerate: framerate) }
    func implementationName() -> String { base.implementationName() }
    func scalingSettings() -> RTCVideoEncoderQpThresholds? { base.scalingSettings() }
    var resolutionAlignment: Int { base.resolutionAlignment }
    var applyAlignmentToAllSimulcastLayers: Bool { base.applyAlignmentToAllSimulcastLayers }
    var supportsNativeHandle: Bool { base.supportsNativeHandle }
}

final class BenchmarkEncoderEvidence: @unchecked Sendable {
    private let lock = NSLock()
    private var profiles: Set<String> = []

    func observe(_ data: Data) {
        // Annex B: locate an SPS NAL after either three- or four-byte start code.
        data.withUnsafeBytes { (bytes: UnsafeRawBufferPointer) in
            guard bytes.count >= 7 else { return }
            for i in 0..<(bytes.count - 6) {
                guard bytes[i] == 0, bytes[i + 1] == 0, bytes[i + 2] == 1,
                      bytes[i + 3] & 0x1f == 7 else { continue }
                let profile = String(format: "%02x%02x%02x", bytes[i + 4], bytes[i + 5], bytes[i + 6])
                lock.lock()
                profiles.insert(profile)
                lock.unlock()
            }
        }
    }

    func summary() -> String {
        lock.lock()
        defer { lock.unlock() }
        return profiles.isEmpty ? "unobserved" : profiles.sorted().joined(separator: ",")
    }
}

struct BenchStats {
    private let values: [String: NSObject]
    let timestamp: Double
    let codec: String

    init(report: RTCStatisticsReport, type: String) {
        let stat = report.statistics.values.first {
            $0.type == type && ($0.values["kind"] as? String) == "video"
        }
        values = stat?.values ?? [:]
        timestamp = stat?.timestamp_us ?? .nan
        codec = (values["codecId"] as? String)
            .flatMap { report.statistics[$0]?.values["mimeType"] as? String } ?? "missing"
    }

    func number(_ key: String) -> Double { (values[key] as? NSNumber)?.doubleValue ?? .nan }
    func string(_ key: String) -> String { values[key] as? String ?? "missing" }
}

final class BenchStatsCollector: @unchecked Sendable {
    private let lock = NSLock()
    private var report: RTCStatisticsReport?

    func save(_ report: RTCStatisticsReport) {
        lock.lock()
        self.report = report
        lock.unlock()
    }

    func load() -> RTCStatisticsReport? {
        lock.lock()
        defer { lock.unlock() }
        return report
    }
}
