#if WONDER_DIAGNOSTICS
import SwiftUI
import WonderPairing
import MetricKit
import os
import Darwin

struct DiagnosticFrame: Codable, Sendable { let binaryUUID: UUID; let offset: UInt64 }
// MetricKit payloads are immutable snapshots; only the utility queue reads them.
private struct MetricDelivery: @unchecked Sendable {
    var metrics: [MXMetricPayload] = []
    var diagnostics: [MXDiagnosticPayload] = []
}
/// Body evaluations of the views a keystroke must or must not redraw, for
/// the typing budget test. Typing redraws the message editor, not the chat.
@MainActor enum RenderCounts {
    static var conversation = 0
    static var composerEditor = 0
}

struct DiagnosticEvent: Codable, Sendable {
    let operation: String
    var phase = "duration"
    var elapsedMs: Double = 0
    var durationMs: Double = 0
    var count: UInt64 = 0
    var bytes: UInt64 = 0
    var metrics: [String: Double] = [:]
    var frames: [DiagnosticFrame] = []
}
struct DiagnosticBatch: Encodable, Sendable {
    let version = 1
    let id = UUID()
    let sessionId: UUID
    let profile = "diagnostics"
    let build: String
    let appVersion: String
    let deviceModel: String
    let osVersion: String
    let events: [DiagnosticEvent]
}

/// All encoding, persistence, pruning, and export happen on this serial queue.
final class DiagnosticJournal: @unchecked Sendable {
    static let shared = DiagnosticJournal()
    private let queue = DispatchQueue(label: "wonder.diagnostics.storage", qos: .utility, autoreleaseFrequency: .workItem)
    private let root: URL
    private let maximumBytes: Int
    private let retention: TimeInterval
    private let session = UUID()
    private let start = ProcessInfo.processInfo.systemUptime
    private var events: [DiagnosticEvent] = []
    private var host: String?
    private var timer: DispatchSourceTimer?
    private var enabled = true
    private let build = Bundle.main.infoDictionary?["CFBundleVersion"] as? String ?? "0"
    private let version = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0"
    private let osVersion: String = { let v=ProcessInfo.processInfo.operatingSystemVersion; return "\(v.majorVersion).\(v.minorVersion).\(v.patchVersion)" }()
    private let device = DeviceModel.identifier
    init(root: URL? = nil, maximumBytes: Int = 20 * 1024 * 1024, retention: TimeInterval = 7 * 86400) {
        self.maximumBytes=maximumBytes; self.retention=retention
        self.root = root ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Diagnostics", isDirectory: true)
        queue.async { [self] in
            try? FileManager.default.createDirectory(at: self.root, withIntermediateDirectories: true, attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication])
            var excluded = self.root; var values = URLResourceValues(); values.isExcludedFromBackup = true; try? excluded.setResourceValues(values)
            let marker = self.root.appendingPathComponent("session.active")
            if FileManager.default.fileExists(atPath: marker.path) { events.append(DiagnosticEvent(operation: "session", phase: "interrupted")) }
            try? Data(session.uuidString.utf8).write(to: marker, options: .atomic)
            let t = DispatchSource.makeTimerSource(queue: queue)
            t.schedule(deadline: .now() + 5, repeating: 5)
            t.setEventHandler { [weak self] in self?.flush() }
            timer = t; t.resume()
        }
    }
    func setEnabled(_ value: Bool) { queue.async { self.enabled = value } }
    func selectHost(_ id: String) {
        guard UUID(uuidString: id) != nil else { return }
        queue.async { self.flush(); self.host = id; self.flush() }
    }
    private let persistenceSignposter = OSSignposter(subsystem: "com.swaymun.wonder", category: .pointsOfInterest)
    func record(_ event: DiagnosticEvent) {
        // Instruments shows each history flush with the bytes it wrote.
        if event.operation == "persistence.save" {
            persistenceSignposter.emitEvent("HistoryFlush", "bytes=\(event.bytes) ms=\(Int(event.durationMs))")
        }
        queue.async {
            guard self.enabled else { return }
            var event = event
            event.elapsedMs = (ProcessInfo.processInfo.systemUptime - self.start) * 1000
            self.events.append(event)
            if self.events.count >= 64 { self.flush() }
            if self.events.count > 256 { self.events.removeFirst(self.events.count - 256) }
        }
    }
    private func flush() {
        guard let host, !events.isEmpty else { return }
        let directory = root.appendingPathComponent(host, isDirectory: true)
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            // Split large system reports; never allow a batch beyond the receiver's limit.
            while !events.isEmpty {
                var count = min(64, events.count)
                var data: Data
                var batch: DiagnosticBatch
                repeat {
                    batch = DiagnosticBatch(sessionId: session, build: build, appVersion: version, deviceModel: device, osVersion: osVersion, events: Array(events.prefix(count)))
                    data = try JSONEncoder().encode(batch)
                    if data.count <= 256 * 1024 { break }
                    count /= 2
                } while count > 0
                guard count > 0 else { events.removeFirst(); continue }
                try data.write(to: directory.appendingPathComponent(batch.id.uuidString + ".json"), options: .atomic)
                events.removeFirst(count)
            }
            prune()
        } catch { /* Diagnostics never interrupts product work; the bounded buffer retries. */ }
    }
    private func files() -> [URL] {
        (FileManager.default.enumerator(at: root, includingPropertiesForKeys: [.fileSizeKey, .contentModificationDateKey])?.allObjects as? [URL] ?? []).filter { $0.pathExtension == "json" }
    }
    private func prune() {
        let fm = FileManager.default
        let sorted = files().compactMap { url -> (URL, Int, Date)? in
            guard let v = try? url.resourceValues(forKeys: [.fileSizeKey,.contentModificationDateKey]), let date = v.contentModificationDate else { return nil }
            return (url, v.fileSize ?? 0, date)
        }.sorted { $0.2 < $1.2 }
        var bytes = sorted.reduce(0) { $0 + $1.1 }
        for (url,size,date) in sorted where bytes > maximumBytes || Date().timeIntervalSince(date) > retention {
            if (try? fm.removeItem(at: url)) != nil { bytes -= size }
        }
    }
    func pending(host: String) async -> [(URL, Data)] {
        await withCheckedContinuation { continuation in queue.async {
            self.flush(); self.prune()
            let directory = self.root.appendingPathComponent(host)
            let files = ((try? FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)) ?? [])
                .filter { $0.pathExtension == "json" && !$0.lastPathComponent.hasSuffix(".sent.json") }.sorted { $0.lastPathComponent < $1.lastPathComponent }.prefix(64)
            var bytes=0
            let pending=files.compactMap { url -> (URL,Data)? in
                guard let data=try? Data(contentsOf:url), bytes+data.count <= 1024*1024 else { return nil }
                bytes += data.count; return (url,data)
            }
            continuation.resume(returning: pending)
        } }
    }
    func acknowledge(_ url: URL) { queue.async { try? FileManager.default.moveItem(at: url, to: url.deletingPathExtension().appendingPathExtension("sent.json")) } }
    func checkpoint(active: Bool) { queue.async {
        self.flush()
        let marker = self.root.appendingPathComponent("session.active")
        if active { try? Data(self.session.uuidString.utf8).write(to: marker, options: .atomic) }
        else { try? FileManager.default.removeItem(at: marker) }
    } }
    func export() async throws -> URL {
        try await withCheckedThrowingContinuation { continuation in queue.async {
            do {
                self.flush(); self.prune()
                let data = self.files().compactMap { try? Data(contentsOf: $0) }
                let objects = data.compactMap { try? JSONSerialization.jsonObject(with: $0) }
                let output = FileManager.default.temporaryDirectory.appendingPathComponent("Wonder-diagnostics.json")
                try JSONSerialization.data(withJSONObject: objects, options: [.sortedKeys]).write(to: output, options: .atomic)
                continuation.resume(returning: output)
            } catch { continuation.resume(throwing: error) }
        } }
    }
}

@MainActor final class Diagnostics: NSObject, ObservableObject, MXMetricManagerSubscriber {
    static let shared = Diagnostics()
    @Published private(set) var capturing = false
    @Published var status = "Ready"
    @Published private(set) var deliveryStatus = "Reports stay here until a paired Mac is available"
    @Published var recording = UserDefaults.standard.object(forKey:"diagnostics.recordingEnabled") as? Bool ?? true {
        didSet { UserDefaults.standard.set(recording,forKey:"diagnostics.recordingEnabled"); DiagnosticJournal.shared.setEnabled(recording); if !recording { stopCapture() } }
    }
    private var active = true
    private var measuredLaunch = false
    private var link: CADisplayLink?
    private var lastFrame: CFTimeInterval?
    private var gapCount: UInt64 = 0
    private var maxGap: Double = 0
    private var lastSummary = 0.0
    private var deadline = 0.0
    private var pending: [(String, Double, Int, UInt64)] = []
    private var probe: Task<Void, Never>?
    private var upload: Task<Void, Never>?
    private let homeNetworkTrust = HomeNetworkTrust()
    private lazy var reportingAPI = PairingAPI(serverTrustDelegate: homeNetworkTrust)
    private var connections: [SavedConnection] = []
    private let signposter = OSSignposter(subsystem: "com.swaymun.wonder", category: .pointsOfInterest)
    private var intervals: [String: OSSignpostIntervalState] = [:]
    override private init() {
        super.init()
        DiagnosticJournal.shared.setEnabled(recording)
        MXMetricManager.shared.add(self)
        DiagnosticJournal.shared.record(DiagnosticEvent(operation: "launch", phase: "start"))
    }
    func configure(_ connections: [SavedConnection]) {
        self.connections = connections
        homeNetworkTrust.update(connections)
        guard let saved = connections.first else { upload?.cancel(); upload=nil; return }
        let chosen = UserDefaults.standard.string(forKey: "diagnostics.reportingHost")
        DiagnosticJournal.shared.selectHost(connections.first(where: { $0.credential.hostInstallationId == chosen })?.credential.hostInstallationId ?? saved.credential.hostInstallationId)
        guard upload == nil else { return }
        upload = Task { [weak self] in
            while !Task.isCancelled {
                if let self { await self.sendReports(self.connections) }
                do { try await Task.sleep(for: .seconds(30)) } catch { break }
            }
        }
    }
    func selectHost(_ saved: SavedConnection, explicit: Bool = false) {
        if !explicit, let selected = UserDefaults.standard.string(forKey: "diagnostics.reportingHost"), connections.contains(where: { $0.credential.hostInstallationId == selected }) { return }
        UserDefaults.standard.set(saved.credential.hostInstallationId, forKey: "diagnostics.reportingHost")
        DiagnosticJournal.shared.selectHost(saved.credential.hostInstallationId)
    }
    private func sendReports(_ connections: [SavedConnection]) async {
        guard active else { return }
        for connection in connections {
            for (url,data) in await DiagnosticJournal.shared.pending(host: connection.credential.hostInstallationId) {
                do {
                    struct Ack: Decodable, Sendable { let accepted: Bool }
                    let ack: Ack = try await reportingAPI.request("/api/v1/diagnostics/batches", origin: connection.origin, body: data, credential: connection.credential)
                    if ack.accepted { DiagnosticJournal.shared.acknowledge(url); deliveryStatus="Reports copied to your Mac" }
                } catch {
                    deliveryStatus = "Reports saved on this phone. Check your Mac connection and app version."
                    break
                }
            }
        }
    }
    func setActive(_ value: Bool) {
        active = value
        DiagnosticJournal.shared.checkpoint(active: value)
        if value {
            DiagnosticJournal.shared.record(DiagnosticEvent(operation:"session",phase:"sample",metrics:["thermalState":Double(ProcessInfo.processInfo.thermalState.rawValue),"lowPowerMode":ProcessInfo.processInfo.isLowPowerModeEnabled ? 1 : 0]))
            if !measuredLaunch {
                measuredLaunch=true; interaction("launch")
                // Physical XCTest runs request one bounded capture explicitly.
                if ProcessInfo.processInfo.environment["WONDER_DIAGNOSTICS_CAPTURE"] == "1" { startCapture() }
            }
        }
        if !value { stopCapture(); pending.removeAll(); for interval in intervals.values { signposter.endInterval("Interaction", interval) }; intervals.removeAll(); link?.isPaused = true }
    }
    func interaction(_ operation: String, count: UInt64 = 0) {
        guard recording, active else { return }
        pending.append((operation, ProcessInfo.processInfo.systemUptime, 0, count))
        DiagnosticJournal.shared.record(DiagnosticEvent(operation: operation, phase: "start", count: count))
        let state = signposter.beginInterval("Interaction", id: signposter.makeSignpostID())
        intervals["\(operation)-\(pending.last!.1)"] = state
        ensureLink()
    }
    private func ensureLink() {
        if link == nil { let value = CADisplayLink(target: self, selector: #selector(frame(_:))); value.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 60, preferred: 60); value.add(to: .main, forMode: .common); link = value }
        link?.isPaused = false
    }
    @objc private func frame(_ sender: CADisplayLink) {
        let now = ProcessInfo.processInfo.systemUptime
        pending = pending.compactMap { (name,start,ticks,count) in
            guard ticks >= 1 else { return (name,start,ticks+1,count) }
            if let interval = intervals.removeValue(forKey: "\(name)-\(start)") { signposter.endInterval("Interaction", interval) }
            DiagnosticJournal.shared.record(DiagnosticEvent(operation: name, phase: "readiness.proxy", durationMs: (now-start)*1000, count: count))
            return nil
        }
        if capturing {
            if let lastFrame {
                let gap = (sender.timestamp-lastFrame)*1000
                if gap > (sender.targetTimestamp-sender.timestamp)*1500 { gapCount += 1; maxGap=max(maxGap,gap) }
            }
            lastFrame=sender.timestamp
            if now-lastSummary >= 1 {
                DiagnosticJournal.shared.record(DiagnosticEvent(operation: "display.gap", phase: "sample", durationMs: maxGap, count: gapCount))
                DiagnosticJournal.shared.record(Self.memorySample())
                gapCount=0; maxGap=0; lastSummary=now
            }
            if now >= deadline { stopCapture() }
        }
        if pending.isEmpty && !capturing { link?.isPaused = true; lastFrame=nil }
    }
    static func memorySample(count items: UInt64 = 0) -> DiagnosticEvent {
        var info = task_vm_info_data_t(); var count = mach_msg_type_number_t(MemoryLayout.size(ofValue: info) / MemoryLayout<integer_t>.size)
        let result = withUnsafeMutablePointer(to: &info) { ptr in ptr.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count) } }
        return DiagnosticEvent(operation:"memory",phase:"sample",count:items,bytes:result == KERN_SUCCESS ? UInt64(info.resident_size) : 0,metrics:result == KERN_SUCCESS ? ["physicalFootprintBytes":Double(info.phys_footprint)] : [:])
    }
    func startCapture() {
        guard recording, active, !capturing else { return }
        lastFrame=nil; gapCount=0; maxGap=0; lastSummary=ProcessInfo.processInfo.systemUptime
        capturing=true; status="Recording for up to two minutes"; deadline=ProcessInfo.processInfo.systemUptime+120
        DiagnosticJournal.shared.record(DiagnosticEvent(operation: "capture", phase: "start")); ensureLink()
        probe = Task.detached(priority: .utility) {
            while !Task.isCancelled {
                do { try await Task.sleep(for: .milliseconds(250)) } catch { break }
                let start=ProcessInfo.processInfo.systemUptime
                await MainActor.run {
                    DiagnosticJournal.shared.record(DiagnosticEvent(operation: "main.probe", phase: "sample", durationMs: (ProcessInfo.processInfo.systemUptime-start)*1000))
                }
            }
        }
    }
    func stopCapture() {
        guard capturing else { return }
        capturing=false; probe?.cancel(); probe=nil; lastFrame=nil; status="Capture saved"
        DiagnosticJournal.shared.record(DiagnosticEvent(operation: "capture", phase: "end")); DiagnosticJournal.shared.checkpoint(active: active)
    }
    nonisolated func didReceive(_ payloads: [MXMetricPayload]) {
        let delivery=MetricDelivery(metrics:payloads)
        DispatchQueue.global(qos:.utility).async {
            for payload in delivery.metrics {
                var values:[String:Double]=[:]
                if let cpu=payload.cpuMetrics { values["cpuTimeMs"]=cpu.cumulativeCPUTime.converted(to:.milliseconds).value }
                if let memory=payload.memoryMetrics { values["peakMemoryBytes"]=memory.peakMemoryUsage.converted(to:.bytes).value }
                if let animation=payload.animationMetrics { values["scrollHitchTimeRatio"]=animation.scrollHitchTimeRatio.value }
                if !values.isEmpty { DiagnosticJournal.shared.record(DiagnosticEvent(operation:"system.metric",phase:"sample",metrics:values)) }
                if let launch=payload.applicationLaunchMetrics { Self.histogram(launch.histogrammedTimeToFirstDraw,name:"launch") }
                if let response=payload.applicationResponsivenessMetrics { Self.histogram(response.histogrammedApplicationHangTime,name:"hang") }
                Self.systemReport(payload.jsonRepresentation(), operation: "system.metric")
            }
        }
    }
    nonisolated func didReceive(_ payloads: [MXDiagnosticPayload]) {
        let delivery=MetricDelivery(diagnostics:payloads)
        DispatchQueue.global(qos:.utility).async {
            for payload in delivery.diagnostics {
                for hang in payload.hangDiagnostics ?? [] { DiagnosticJournal.shared.record(DiagnosticEvent(operation:"system.hang",phase:"duration",durationMs:hang.hangDuration.converted(to:.milliseconds).value)) }
                for cpu in payload.cpuExceptionDiagnostics ?? [] { DiagnosticJournal.shared.record(DiagnosticEvent(operation:"system.cpu",phase:"duration",durationMs:cpu.totalCPUTime.converted(to:.milliseconds).value,metrics:["sampledTimeMs":cpu.totalSampledTime.converted(to:.milliseconds).value])) }
                Self.systemReport(payload.jsonRepresentation(), operation: "system.metric")
            }
        }
    }
    nonisolated private static func histogram(_ histogram: MXHistogram<UnitDuration>, name: String) {
        for case let bucket as MXHistogramBucket<UnitDuration> in histogram.bucketEnumerator {
            DiagnosticJournal.shared.record(DiagnosticEvent(operation:"system.metric",phase:"sample",count:UInt64(bucket.bucketCount),metrics:[name+"BucketStartMs":bucket.bucketStart.converted(to:.milliseconds).value,name+"BucketEndMs":bucket.bucketEnd.converted(to:.milliseconds).value]))
        }
    }
    /// Keep only numeric measurements and UUID/offset frames, never free-form strings.
    nonisolated private static func systemReport(_ data: Data, operation: String) {
        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
        for (key,value) in object {
            var event = DiagnosticEvent(operation: key.lowercased().contains("crash") ? "system.crash" : key.lowercased().contains("hang") ? "system.hang" : key.lowercased().contains("cpu") ? "system.cpu" : key.lowercased().contains("disk") ? "system.disk" : operation, phase: "sample")
            func walk(_ value: Any, path: String, depth: Int) {
                guard depth < 32 else { return }
                if let fields = value as? [String: Any] {
                    if let uuid = fields["binaryUUID"] as? String, let id = UUID(uuidString: uuid), let offset = fields["offsetIntoBinaryTextSegment"] as? NSNumber, event.frames.count < 128 { event.frames.append(DiagnosticFrame(binaryUUID: id, offset: offset.uint64Value)) }
                    for (k,v) in fields where k.count <= 64 && k.unicodeScalars.allSatisfy({ CharacterSet.alphanumerics.contains($0) || $0 == "_" }) { walk(v, path: path + "." + k, depth: depth+1) }
                } else if let array = value as? [Any] { for (i,v) in array.prefix(128).enumerated() { walk(v,path:path+"[\(i)]",depth:depth+1) } }
                else if let number = value as? NSNumber, event.metrics.count < 128, path.count <= 160, number.doubleValue.isFinite { event.metrics[path]=number.doubleValue }
            }
            walk(value,path:key,depth:0)
            if !event.metrics.isEmpty || !event.frames.isEmpty { DiagnosticJournal.shared.record(event) }
        }
    }
}

/// Wave 2.4d experiment. Reuses the chat-pinned controller and never replaces
/// the transcript/approval panel. This entire file is absent from Release.
struct DiagnosticVoiceRailPrototype: View {
    @ObservedObject var controller: VoiceController
    let scene: ShellState?
    let available: Bool
    let start: () -> Void

    var body: some View {
        Button {
            if controller.isActive { controller.showTarget(in: scene) }
            else { start() }
        } label: {
            Label("Voice", systemImage: controller.isActive ? "waveform.circle.fill" : "waveform.circle")
                .modifier(RailButtonSurface(inBar: true, selected: controller.isActive))
        }
        .buttonStyle(.plain)
        .disabled(!controller.isActive && !available)
        .accessibilityLabel(controller.isActive ? controller.targetLabel : "Talk with voice")
        .accessibilityValue(controller.isActive ? controller.statusText : "Off")
        .accessibilityHint("Opens the voice chat")
        .accessibilityIdentifier("voice-rail-entry")
    }
}

struct DiagnosticsView: View {
    @ObservedObject var library: ConnectionLibrary
    @ObservedObject private var diagnostics = Diagnostics.shared
    @State private var exportURL: URL?
    @State private var failure: String?
    @State private var selectedHost = ""
    @AppStorage("diagnostics.voiceRailPrototype") private var voiceRailPrototype = false
    var body: some View {
        WonderList {
            Section("Prototypes") {
                Toggle("Duo voice rail", isOn: $voiceRailPrototype).systemSwitch()
                    .accessibilityIdentifier("diagnostics-voice-rail")
                Text("A voice shortcut and panel on the Duo rail. Voice stays in the chat where it started.")
                    .font(.footnote).foregroundStyle(.secondary)
            }
            Section("Recording") {
                Toggle("Record performance", isOn: $diagnostics.recording).systemSwitch()
                Text(diagnostics.status).accessibilityIdentifier("diagnostics-status")
                Button(diagnostics.capturing ? "Stop capture" : "Record two minutes") { if diagnostics.capturing { diagnostics.stopCapture() } else { diagnostics.startCapture() } }
                    .disabled(!diagnostics.recording).accessibilityIdentifier("diagnostics-capture")
                Text("Display gaps and readiness are timing estimates. Gesture and hitch tests are reported separately.").font(.caption).foregroundStyle(.secondary)
            }
            Section("Reporting computer") {
                Text(diagnostics.deliveryStatus).font(.caption).foregroundStyle(.secondary)
                Picker("Computer", selection: $selectedHost) {
                    ForEach(library.saved.connections, id: \.credential.hostInstallationId) { saved in Text(library.saved.name(for: saved)).tag(saved.credential.hostInstallationId) }
                }.onChange(of: selectedHost) { _, id in if let saved = library.saved.connections.first(where: { $0.credential.hostInstallationId == id }) { diagnostics.selectHost(saved, explicit: true) } }
                Text("Performance data only. Messages, tool output, and credentials are excluded. Reports wait here while your Mac is unavailable.").font(.caption).foregroundStyle(.secondary)
            }
            Section("Live tests") {
                ForEach(library.saved.connections, id: \.credential.hostInstallationId) { saved in
                    NavigationLink(library.saved.name(for: saved)) { DiagnosticScenariosView(model: library.model(for: saved)) }
                }
            }
            Section {
                Button("Prepare export") { Task { do { exportURL = try await DiagnosticJournal.shared.export(); failure=nil } catch { failure="Could not prepare the report. Try again." } } }
                if let exportURL { ShareLink("Share diagnostics", item: exportURL) }
                if let failure { Text(failure).foregroundStyle(.red) }
            }
        }.settingsNavigationTitle("Diagnostics")
            .onAppear { selectedHost=UserDefaults.standard.string(forKey:"diagnostics.reportingHost") ?? library.saved.connections.first?.credential.hostInstallationId ?? "" }
    }
}
#endif
