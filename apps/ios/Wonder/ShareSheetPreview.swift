#if WONDER_DIAGNOSTICS
import SwiftUI
import UIKit
import CryptoKit
import UniformTypeIdentifiers
import WonderPairing

/// Hosts the share extension's sheet inside Diagnostics builds, so scenarios can
/// drive it without Photos or Safari. `-diagnostics-share-sheet` opens it with
/// any of `-diagnostics-share-text`, `-diagnostics-share-url` and
/// `-diagnostics-share-image` as the shared items, against a synthetic Mac.
/// `-diagnostics-share-upload-fails` refuses the first upload so the retry path
/// can be checked; each upload takes three seconds, and the retry holds until signalled.
/// `-diagnostics-share-recovery` starts with an earlier share left unconfirmed.
/// `-diagnostics-share-expect-choice` creates a new thread only for Claude Sonnet.
enum DiagnosticShareFixture {
    static var requested: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-share-sheet") }
    static func has(_ flag: String) -> Bool { ProcessInfo.processInfo.arguments.contains(flag) }

    static let hostID = "share-fixture-host"
    static let savedConnections: Data = {
        let credential = #"{"sessionToken":"share-fixture","deviceId":"share-fixture-device","csrfToken":"share-fixture","hostInstallationId":"share-fixture-host","expiresAtMs":null}"#
        let other = #"{"sessionToken":"share-fixture","deviceId":"share-fixture-device","csrfToken":"share-fixture","hostInstallationId":"share-fixture-laptop","expiresAtMs":null}"#
        return Data(#"{"connections":[{"origin":"https://share-fixture.invalid","credential":\#(credential),"hostName":"Studio","requiresPairing":false,"storageDeviceId":"share-fixture-device"},{"origin":"https://share-laptop.invalid","credential":\#(other),"hostName":"MacBook Air","requiresPairing":false,"storageDeviceId":"share-fixture-device"}],"selectedHostIDs":[]}"#.utf8)
    }()

    static func providers() -> [NSItemProvider] {
        var providers: [NSItemProvider] = []
        if has("-diagnostics-share-text") {
            providers.append(NSItemProvider(object: "The upload limit should match the composer: four files, 8 MB each. Check what happens when a photo is larger." as NSString))
        }
        if has("-diagnostics-share-url") {
            providers.append(NSItemProvider(object: URL(string: "https://developer.apple.com/documentation/uikit/share-extensions")! as NSURL))
        }
        if has("-diagnostics-share-image") {
            let provider = NSItemProvider(item: image() as NSData, typeIdentifier: UTType.png.identifier)
            provider.suggestedName = "Mockup.png"
            providers.append(provider)
        }
        if has("-diagnostics-share-video"), let url = Bundle.main.url(forResource: "attachment-video", withExtension: "mp4") {
            let provider = NSItemProvider()
            provider.suggestedName = "Screen Recording.mp4"
            provider.registerFileRepresentation(forTypeIdentifier: UTType.mpeg4Movie.identifier, fileOptions: [], visibility: .all) { completion in
                do {
                    if has("-diagnostics-share-large-video") || has("-diagnostics-share-oversized-video") {
                        // A valid MP4 with an ISO BMFF free box, beyond the old 8 MiB limit.
                        let large = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".mp4")
                        defer { try? FileManager.default.removeItem(at: large) }
                        try FileManager.default.copyItem(at: url, to: large)
                        let handle = try FileHandle(forWritingTo: large)
                        defer { try? handle.close() }
                        try handle.seekToEnd()
                        try handle.write(contentsOf: Data([0, 0, 0, 0, 102, 114, 101, 101]))
                        try handle.truncate(atOffset: has("-diagnostics-share-oversized-video")
                            ? UInt64(AttachmentLimits.maximumVideoBytes + 1) : 12 * 1024 * 1024)
                        completion(large, false, nil)
                    } else { completion(url, false, nil) }
                } catch { completion(nil, false, error) }
                return nil
            }
            if has("-diagnostics-share-video-with-image") {
                // A provider may advertise a still representation as well as the movie.
                provider.registerDataRepresentation(forTypeIdentifier: UTType.png.identifier, visibility: .all) { completion in
                    completion(image(), nil)
                    return nil
                }
            }
            providers.append(provider)
        }
        return providers
    }

    private static func image() -> Data {
        let size = CGSize(width: 1200, height: 800)
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        return UIGraphicsImageRenderer(size: size, format: format).pngData { context in
            let colors = [UIColor.systemIndigo.cgColor, UIColor.systemTeal.cgColor] as CFArray
            let gradient = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: colors, locations: [0, 1])!
            context.cgContext.drawLinearGradient(gradient, start: .zero, end: CGPoint(x: size.width, y: size.height), options: [])
            UIColor.white.withAlphaComponent(0.85).setFill()
            UIBezierPath(roundedRect: CGRect(x: 140, y: 160, width: 920, height: 120), cornerRadius: 28).fill()
            UIBezierPath(roundedRect: CGRect(x: 140, y: 330, width: 620, height: 90), cornerRadius: 24).fill()
            UIBezierPath(roundedRect: CGRect(x: 140, y: 470, width: 780, height: 90), cornerRadius: 24).fill()
        }
    }

    /// A fresh journal each launch, so no scenario sees a share an earlier run
    /// left unconfirmed. `-diagnostics-share-recovery` seeds one: a message to
    /// "Polish the share sheet" whose answer was lost. The remembered destination
    /// (Project, agent and model) is cleared too, so the sheet starts from the
    /// computer's defaults whatever an earlier run chose.
    static func journal() -> ShareJournal {
        UserDefaults.standard.removeObject(forKey: ShareModel.lastDestinationKey)
        let journal = ShareJournal(directory: FileManager.default.temporaryDirectory
            .appendingPathComponent("share-fixture-" + UUID().uuidString, isDirectory: true))
        if has("-diagnostics-share-recovery") {
            let item = PendingShare(host: hostID, deviceID: "share-fixture-device", target: "thread:codex:share",
                destination: .existing(targetID: "thread:codex:share", projectID: "p-wonder", reference: "codex:share",
                                       conversationID: "c-share", attach: false),
                body: "Earlier share: the upload limit notes.", files: [], title: "Polish the share sheet")
            try? journal.begin(item, files: [])
        }
        return journal
    }

    static func api() -> PairingAPI {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [DiagnosticVideoUploadProtocol.self, DiagnosticShareURLProtocol.self]
        return PairingAPI(configuration: configuration)
    }
}

struct DiagnosticShareFixtureView: View {
    @StateObject private var model = ShareModel(api: DiagnosticShareFixture.api(), journal: DiagnosticShareFixture.journal(),
                                                readConnections: { DiagnosticShareFixture.savedConnections },
                                                finish: { _ in })
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    @State private var presented = true
    @State private var finished: String?

    var body: some View {
        VStack(spacing: 12) {
            Text(finished ?? "Share fixture").font(.headline).accessibilityIdentifier("share-fixture-result")
            Button("Share again") { presented = true }.accessibilityIdentifier("share-fixture-reopen")
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .sheet(isPresented: $presented) {
            ShareView(model: model, themeID: ProcessInfo.processInfo.diagnosticThemeOverride ?? UserDefaults.standard.string(forKey: WonderTheme.storageKey))
                .dynamicTypeSize(DiagnosticShareFixture.has("-diagnostics-share-large-type") ? .accessibility5 : dynamicTypeSize)
                .background {
                    GeometryReader { geometry in DuoGeometryEvidence(geometry: geometry, priority: 100, ownsControls: true) }
                }
        }
        .task { model.start(providers: DiagnosticShareFixture.providers()) }
    }
}

private final class DiagnosticShareURLProtocol: URLProtocol, @unchecked Sendable {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var uploads = 0

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}

    override func startLoading() {
        let method = request.httpMethod ?? "GET"
        let path = request.url.map { $0.path + ($0.query.map { "?" + $0 } ?? "") } ?? ""
        var body = request.httpBody
        if body == nil, let stream = request.httpBodyStream {
            stream.open(); defer { stream.close() }
            var data = Data(); var buffer = [UInt8](repeating: 0, count: 65536)
            while stream.hasBytesAvailable { let count = stream.read(&buffer, maxLength: buffer.count); if count <= 0 { break }; data.append(buffer, count: count) }
            body = data
        }
        let laptop = request.url?.host == "share-laptop.invalid"
        let sent = body
        let upload = method == "POST" && path.hasSuffix("/files")
        // The retried upload holds until `wonderctl signal share-upload`, so its progress
        // is asserted however slowly the tap lands (a 3 s timer lost it on the iPad).
        if upload && DiagnosticShareFixture.has("-diagnostics-share-upload-fails") && Self.lock.withLock({ Self.uploads }) >= 1 {
            DiagnosticSubagentFixture.afterSignal("share-upload") { [self] in respond(method: method, path: path, body: sent, laptop: laptop) }
            return
        }
        let delay: Double = upload ? 3 : method == "POST" ? 0.4 : 0.15
        DispatchQueue.global().asyncAfter(deadline: .now() + delay) { [self] in
            respond(method: method, path: path, body: sent, laptop: laptop)
        }
    }

    private func finish(_ status: Int, _ value: Any) {
        let data = (try? JSONSerialization.data(withJSONObject: value)) ?? Data("{}".utf8)
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: "HTTP/1.1",
                                       headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: data)
        client?.urlProtocolDidFinishLoading(self)
    }

    private static let now = Int(Date().timeIntervalSince1970)
    private static func thread(_ reference: String, _ conversation: String?, _ title: String, _ family: String,
                               ago: Int, working: Bool = false, pinned: Bool = false) -> [String: Any] {
        ["reference": reference, "conversationId": conversation ?? NSNull(), "title": title, "family": family,
         "updatedAt": now - ago, "isPinned": pinned, "hasUnread": false, "isWorking": working]
    }
    private static func project(_ id: String, _ name: String, family: String) -> [String: Any] {
        ["id": id, "name": name, "isIncluded": true, "isPinned": false, "rootsRevision": 3, "lastFamily": family,
         "folders": [["id": id + "-folder", "path": "/Users/fixture/" + name, "name": name, "isPrimary": true, "isAvailable": true]],
         "createdAt": "2026-01-01T00:00:00Z"]
    }
    nonisolated(unsafe) private static let threads: [String: [[String: Any]]] = [
        "p-wonder": [thread("codex:share", "c-share", "Polish the share sheet", "codex", ago: 120, working: true),
                     thread("claude:notes", "c-notes", "Release notes for 2.4", "claude", ago: 3_600, pinned: true),
                     thread("codex:pairing", nil, "Fix pairing timeout", "codex", ago: 90_000)],
        "p-site": [thread("claude:landing", "c-landing", "Landing page copy", "claude", ago: 600),
                   thread("codex:images", "c-images", "Compress hero images", "codex", ago: 7_200)],
        "p-research": [thread("claude:papers", "c-papers", "Paper summaries", "claude", ago: 180_000)],
    ]

    private func respond(method: String, path: String, body: Data?, laptop: Bool) {
        let fields = body.flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] } ?? [:]
        let resource = path.split(separator: "?").first.map(String.init) ?? path
        switch (method, resource) {
        case ("GET", "/api/v1/projects"):
            let projects = laptop ? [Self.project("p-laptop", "Notes", family: "claude")]
                : [Self.project("p-wonder", "Wonder", family: "codex"), Self.project("p-site", "Website", family: "claude"),
                   Self.project("p-research", "Research", family: "claude")]
            finish(200, ["projects": projects, "families": [AgentFamily.codex, .claude].map { ["family": $0.rawValue, "available": true, "capabilities": ProviderCapabilities.fixtureMacJSON($0)] },
                         "modesVersion": 1, "pinned": laptop ? [] : [["projectId": "p-wonder", "thread": Self.threads["p-wonder"]![1]]]])
        case ("GET", let value) where value.hasPrefix("/api/v1/projects/") && value.hasSuffix("/threads"):
            let id = value.split(separator: "/")[3]
            let rows = laptop ? [Self.thread("claude:todo", "c-todo", "Weekend to-do", "claude", ago: 4_000)] : Self.threads[String(id)] ?? []
            finish(200, ["threads": rows, "nextCursor": NSNull(), "partial": []])
        case ("POST", let value) where value.hasSuffix("/threads/attach"):
            // Attaching returns the requested thread with its conversation.
            let reference = fields["reference"] as? String ?? ""
            var row = Self.threads.values.flatMap { $0 }.first { $0["reference"] as? String == reference } ?? [:]
            if row["conversationId"] is NSNull { row["conversationId"] = "c-" + reference.replacingOccurrences(of: ":", with: "-") }
            finish(row.isEmpty ? 404 : 200, row)
        case ("POST", let value) where value.hasPrefix("/api/v1/projects/") && value.hasSuffix("/threads"):
            let family = fields["family"] as? String ?? "codex"
            // The scenario picks Claude Sonnet (neither the Project's agent nor the
            // default model); anything else is refused, so a dropped choice fails.
            if DiagnosticShareFixture.has("-diagnostics-share-expect-choice"),
               family != "claude" || fields["model"] as? String != "claude:sonnet" {
                finish(422, ["error": "unexpected agent or model"]); return
            }
            finish(200, ["conversation": Self.thread("wonder:new", "c-new", "New thread", family, ago: 0), "receipt": NSNull()])
        case ("GET", "/api/v1/agent-options"):
            func model(_ id: String, _ name: String, _ family: String, isDefault: Bool) -> [String: Any] {
                ["id": id, "displayName": name, "hidden": false, "agentFamily": family, "isDefault": isDefault,
                 "reasoningEfforts": [["id": "medium", "label": "Medium"], ["id": "high", "label": "High"]], "defaultReasoningEffort": "high"]
            }
            finish(200, ["models": [model("gpt-5.5-codex", "GPT-5.5 Codex", "codex", isDefault: true), model("gpt-5.5-mini", "GPT-5.5 mini", "codex", isDefault: false),
                                    model("claude:opus", "Claude Opus", "claude", isDefault: true), model("claude:sonnet", "Claude Sonnet", "claude", isDefault: false)],
                         "timezone": "UTC", "allowedApprovalPolicies": []])
        case ("POST", let value) where value.hasSuffix("/files"):
            let attempt: Int = Self.lock.withLock { Self.uploads += 1; return Self.uploads }
            if DiagnosticShareFixture.has("-diagnostics-share-upload-fails") && attempt == 1 { finish(503, [:]); return }
            let data = Data(base64Encoded: fields["contentBase64"] as? String ?? "") ?? Data()
            finish(200, ["id": "file-\(attempt)", "name": fields["name"] ?? "", "mimeType": fields["mimeType"] ?? "", "byteSize": data.count,
                         "sha256": ConversationFile.digest(data), "state": "available", "updatedAt": "fixture"])
        case ("POST", let value) where value.hasSuffix("/messages"):
            let attachments = fields["attachmentIds"] as? [String] ?? []
            if DiagnosticShareFixture.has("-diagnostics-share-video"),
               !DiagnosticVideoUploadProtocol.completedShare(attachmentIDs: attachments) {
                finish(422, ["error": "Expected the verified movie file as the message attachment"]); return
            }
            if DiagnosticShareFixture.has("-diagnostics-share-large-video"), !DiagnosticVideoUploadProtocol.verifiedShare(attachmentIDs: attachments) {
                finish(422, ["error": "Expected cancelled upload and verified retry with a new identity"]); return
            }
            let conversation = value.split(separator: "/")[3]
            let text = fields["body"] as? String ?? ""
            finish(200, ["clientMessageId": fields["clientMessageId"] ?? "", "wonderMessageId": "m-\(UUID().uuidString.prefix(6))",
                         "bodySha256": ConversationFile.digest(Data(text.utf8)), "conversationId": String(conversation),
                         "deliveryState": "accepted", "queued": conversation == "c-share"])
        default:
            finish(404, [:])
        }
    }
}

/// The real upload client talks to this isolated, signal-held HTTP peer. It
/// validates offsets and final bytes; no message endpoint or real host is used.
final class DiagnosticVideoUploadProtocol: URLProtocol, @unchecked Sendable {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var cancelledIDs: Set<String> = []
    nonisolated(unsafe) private static var completedIDs: Set<String> = []
    static func verifiedShare(attachmentIDs: [String]) -> Bool {
        lock.withLock { !cancelledIDs.isEmpty && !completedIDs.isEmpty &&
            cancelledIDs.isDisjoint(with: completedIDs) && Set(attachmentIDs) == completedIDs }
    }
    static func completedShare(attachmentIDs: [String]) -> Bool {
        lock.withLock { !completedIDs.isEmpty && Set(attachmentIDs) == completedIDs &&
            attachmentIDs.allSatisfy { sessions[$0]?.metadata["mimeType"] as? String == "video/mp4" &&
                sessions[$0]?.metadata["name"] as? String == "Screen Recording.mp4" } }
    }
    private static var chunkBytes: Int { DiagnosticShareFixture.has("-diagnostics-share-large-video") ? 1024 * 1024 : 4096 }
    nonisolated(unsafe) private static var sessions: [String: (metadata: [String: Any], bytes: Data)] = [:]
    nonisolated(unsafe) private static var released = false
    private let stopLock = NSLock()
    private var stopped = false
    static let api: PairingAPI = {
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [DiagnosticVideoUploadProtocol.self]
        return PairingAPI(configuration: config)
    }()
    override class func canInit(with request: URLRequest) -> Bool {
        request.url?.host?.hasSuffix(".invalid") == true && request.url?.path.contains("/files/uploads") == true
    }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() { stopLock.withLock { stopped = true } }
    override func startLoading() {
        var body = request.httpBody ?? Data()
        if let stream = request.httpBodyStream {
            stream.open(); defer { stream.close() }
            var buffer = [UInt8](repeating: 0, count: 8192)
            while stream.hasBytesAvailable {
                let count = stream.read(&buffer, maxLength: buffer.count)
                guard count > 0 else { break }
                body.append(contentsOf: buffer.prefix(count))
            }
        }
        let payload = body
        let offset = (try? JSONSerialization.jsonObject(with: body) as? [String: Any])?["offset"] as? Int ?? 0
        if request.httpMethod == "PATCH", offset >= Self.chunkBytes, !Self.lock.withLock({ Self.released }) {
            DiagnosticSubagentFixture.afterSignal("video-upload") { [self] in
                Self.lock.withLock { Self.released = true }
                respond(payload)
            }
        } else { respond(payload) }
    }
    private func respond(_ body: Data) {
        guard !stopLock.withLock({ stopped }) else { return }
        let path = request.url!.path
        let json = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any] ?? [:]
        let result: (Int, [String: Any]) = Self.lock.withLock {
            let id = path.hasSuffix("/uploads") ? json["clientUploadId"] as? String ?? "" :
                (path.hasSuffix("/complete") ? request.url!.deletingLastPathComponent().lastPathComponent : request.url!.lastPathComponent)
            if request.httpMethod == "DELETE" { Self.sessions[id] = nil; Self.cancelledIDs.insert(id); return (200, [:]) }
            if path.hasSuffix("/uploads"), Self.sessions[id] == nil {
                guard !Self.cancelledIDs.contains(id) else { return (409, [:]) }
                Self.sessions[id] = (json, Data())
            }
            guard var session = Self.sessions[id] else { return (404, [:]) }
            if request.httpMethod == "PATCH" {
                guard json["offset"] as? Int == session.bytes.count,
                      let encoded = json["contentBase64"] as? String, let bytes = Data(base64Encoded: encoded), bytes.count <= Self.chunkBytes else { return (409, [:]) }
                session.bytes.append(bytes); Self.sessions[id] = session
            }
            let total = session.metadata["byteSize"] as? Int ?? 0
            var reply: [String: Any] = ["uploadId": id, "receivedBytes": session.bytes.count, "totalBytes": total,
                "maxChunkBytes": Self.chunkBytes, "state": "uploading"]
            if path.hasSuffix("/complete") {
                guard session.bytes.count == total, ConversationFile.digest(session.bytes) == session.metadata["sha256"] as? String else { return (422, [:]) }
                Self.completedIDs.insert(id)
                reply["state"] = "complete"
                reply["file"] = ["id": id, "name": session.metadata["name"]!, "mimeType": session.metadata["mimeType"]!,
                    "byteSize": total, "sha256": session.metadata["sha256"]!, "state": "available", "updatedAt": "2026-10-28"]
            }
            return (200, reply)
        }
        let response = HTTPURLResponse(url: request.url!, statusCode: result.0, httpVersion: nil, headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: try! JSONSerialization.data(withJSONObject: result.1))
        client?.urlProtocolDidFinishLoading(self)
    }
}
#endif
