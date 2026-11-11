import Foundation
import AVFoundation
import XCTest
@testable import WonderPairing

private final class MediaResponseProtocol: URLProtocol, @unchecked Sendable {
    private static let revision = String(repeating: "a", count: 64)

    override class func canInit(with request: URLRequest) -> Bool {
        request.url?.host == "media-test.invalid"
    }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        guard let url = request.url else { return }
        let mode = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first(where: { $0.name == "case" })?.value
        let range = request.value(forHTTPHeaderField: "Range")
        let authenticated = request.value(forHTTPHeaderField: "Origin") == "https://media-test.invalid"
            && request.value(forHTTPHeaderField: "Cookie") == "__Host-wonder_session=synthetic"
        let expectedRevision = range == "bytes=0-0" ? nil : Self.revision
        guard authenticated, request.value(forHTTPHeaderField: "X-Wonder-Revision") == expectedRevision else {
            reply(status: 403, body: Data(), headers: [:]); return
        }
        if mode == "asset", let fixture = ProcessInfo.processInfo.environment["WONDER_MEDIA_FIXTURE"],
           let data = try? Data(contentsOf: URL(fileURLWithPath: fixture)),
           let range, range.hasPrefix("bytes=") {
            let bounds = range.dropFirst(6).split(separator: "-").compactMap { Int($0) }
            guard bounds.count == 2, bounds[0] >= 0, bounds[0] < data.count else {
                reply(status: 416, body: Data(), headers: [:]); return
            }
            let end = min(bounds[1], data.count - 1)
            let body = data.subdata(in: bounds[0]..<(end + 1))
            reply(status: 206, body: body, headers: [
                "Content-Type": "video/mp4",
                "Content-Range": "bytes \(bounds[0])-\(end)/\(data.count)",
                "Content-Length": "\(body.count)",
                "X-Wonder-Revision": Self.revision
            ])
            return
        }
        if mode == "stale" && range != "bytes=0-0" {
            reply(status: 409, body: Data(), headers: [:]); return
        }
        let body = range == "bytes=0-0" ? Data("a".utf8) : Data("bcd".utf8)
        let actual = mode == "truncated" ? body.prefix(2) : body[...]
        let start = range == "bytes=0-0" ? 0 : 1
        let end = range == "bytes=0-0" ? 0 : 3
        reply(status: 206, body: Data(actual), headers: [
            "Content-Type": "video/mp4",
            "Content-Range": "bytes \(start)-\(end)/4",
            "Content-Length": "\(body.count)",
            "X-Wonder-Revision": mode == "changed-revision" ? String(repeating: "b", count: 64) : Self.revision
        ])
    }
    override func stopLoading() {}

    private func reply(status: Int, body: Data, headers: [String: String]) {
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: status,
            httpVersion: "HTTP/1.1", headerFields: headers)!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: body)
        client?.urlProtocolDidFinishLoading(self)
    }
}

final class WorkspaceMediaTransportTests: XCTestCase {
    private func fixture() -> (PairingAPI, SavedConnection) {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [MediaResponseProtocol.self]
        let api = PairingAPI(configuration: configuration)
        let credential = Credential(sessionToken: "synthetic", deviceId: "phone",
                                    csrfToken: "csrf", hostInstallationId: "host", expiresAtMs: nil)
        return (api, SavedConnection(origin: "https://media-test.invalid", credential: credential))
    }

    func testMediaRangesAuthenticateAndPinRevision() async throws {
        let (api, connection) = fixture()
        let path = "/api/v1/conversations/chat/workspace/media?root=workspace&path=clip.mp4"
        let first = try await api.downloadWorkspaceMediaRange(path, connection: connection,
                                                               start: 0, end: 0, revision: nil)
        XCTAssertEqual(first.bytes, Data("a".utf8))
        XCTAssertEqual(first.total, 4)
        XCTAssertEqual(first.mimeType, "video/mp4")
        let next = try await api.downloadWorkspaceMediaRange(path, connection: connection,
                                                              start: 1, end: 3, revision: first.revision)
        XCTAssertEqual(next.bytes, Data("bcd".utf8))
        XCTAssertEqual(next.revision, first.revision)
    }

    func testMediaRangeRejectsTruncationAndRevisionChange() async throws {
        let (api, connection) = fixture()
        let base = "/api/v1/conversations/chat/workspace/media?root=workspace&path=clip.mp4&case="
        let revision = String(repeating: "a", count: 64)
        do {
            _ = try await api.downloadWorkspaceMediaRange(base + "truncated", connection: connection,
                                                           start: 1, end: 3, revision: revision)
            XCTFail("Truncated media must not be accepted")
        } catch FileFailure.integrity { }
        do {
            _ = try await api.downloadWorkspaceMediaRange(base + "changed-revision", connection: connection,
                                                           start: 1, end: 3, revision: revision)
            XCTFail("A changed media revision must not be merged")
        } catch FileFailure.stale { }
        let probe = try await api.downloadWorkspaceMediaRange(base + "stale", connection: connection,
                                                               start: 0, end: 0, revision: nil)
        do {
            _ = try await api.downloadWorkspaceMediaRange(base + "stale", connection: connection,
                                                           start: 1, end: 3, revision: probe.revision)
            XCTFail("A 409 after the first probe must request a new preview")
        } catch FileFailure.stale { }
    }

    func testAssetLoaderReadsAuthenticatedRanges() async throws {
        guard ProcessInfo.processInfo.environment["WONDER_MEDIA_FIXTURE"] != nil else {
            throw XCTSkip("Run with a local synthetic MP4 fixture to probe AVFoundation on this host")
        }
        let (api, connection) = fixture()
        let loader = WorkspaceMediaResourceLoader(api: api, connection: connection,
            path: "/api/v1/conversations/chat/workspace/media?root=workspace&path=clip.mp4&case=asset")
        let asset = try loader.asset(filename: "clip.mp4")
        let isPlayable = try await asset.load(.isPlayable)
        let duration = try await asset.load(.duration)
        XCTAssertTrue(isPlayable)
        XCTAssertGreaterThan(duration.seconds, 0)
        let player = AVPlayer(playerItem: AVPlayerItem(asset: asset))
        let deadline = Date().addingTimeInterval(5)
        while player.currentItem?.status == .unknown && Date() < deadline {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(player.currentItem?.status, .readyToPlay)
        let didSeek = await withCheckedContinuation { continuation in
            player.seek(to: CMTime(seconds: 1, preferredTimescale: 600)) { finished in
                continuation.resume(returning: finished)
            }
        }
        XCTAssertTrue(didSeek)
        player.pause()
    }
}

/// A Mac's workspace file route: a weak tag, 304 for a current tag, and the
/// start of the file for `prefix=`. `old-host` answers as a Mac without
/// either; `bad-range` names a range that doesn't match the bytes sent.
private final class ConditionalPreviewProtocol: URLProtocol, @unchecked Sendable {
    static let body = Data((0..<300_000).map { UInt8(97 + $0 % 26) })
    static let tag = "W/\"revision-1\""
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "conditional-preview.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let url = request.url!
        let query = url.query ?? ""
        let prefix = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?
            .first { $0.name == "prefix" }?.value.flatMap { Int($0) }
        let oldHost = query.contains("old-host")
        var status = 200
        var headers = ["Content-Type": "text/plain"]
        var payload = Self.body
        if !oldHost { headers["ETag"] = Self.tag }
        if !oldHost, request.value(forHTTPHeaderField: "If-None-Match") == Self.tag {
            status = 304
            payload = Data()
        } else if !oldHost, let prefix, prefix < Self.body.count {
            status = 206
            payload = Self.body.prefix(prefix)
            let last = query.contains("bad-range") ? prefix : prefix - 1
            headers["Content-Range"] = "bytes 0-\(last)/\(Self.body.count)"
        }
        headers["Content-Length"] = "\(payload.count)"
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: url, statusCode: status, httpVersion: "HTTP/1.1",
                                                             headerFields: headers)!, cacheStoragePolicy: .notAllowed)
        if !payload.isEmpty { client?.urlProtocol(self, didLoad: payload) }
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

private final class LargePreviewProtocol: URLProtocol, @unchecked Sendable {
    static let body: Data = {
        let count = 9 * 1024 * 1024 + 17
        var bytes = [UInt8](repeating: 0, count: count)
        for index in 0..<count { bytes[index] = UInt8(index % 251) }
        return Data(bytes)
    }()
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "large-preview.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    // Touched only on the loading thread.
    private var delivered = 0
    private var finished = false
    private var observation: NSKeyValueObservation?

    override func startLoading() {
        let large = request.url?.query?.contains("too-large") == true
        let status = large ? 413 : 200
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: "HTTP/1.1",
            headerFields: ["Content-Length": large ? "0" : "\(Self.body.count)", "Content-Type": "application/pdf"])!,
            cacheStoragePolicy: .notAllowed)
        guard !large, let task else { client?.urlProtocolDidFinishLoading(self); return }
        // A network delivers the body in pieces over time; progress follows
        // them. Handing every piece over at once lets the session count them
        // in one update, so send the next piece only after the session has
        // counted the previous one.
        let thread = Thread.current
        observation = task.observe(\.countOfBytesReceived) { [weak self] _, _ in
            guard let self else { return }
            self.perform(#selector(self.deliverNextPiece), on: thread, with: nil, waitUntilDone: false)
        }
        deliverNextPiece()
    }
    @objc private func deliverNextPiece() {
        guard !finished, let task, Int(task.countOfBytesReceived) >= delivered else { return }
        guard delivered < Self.body.count else {
            finished = true
            observation?.invalidate()
            client?.urlProtocolDidFinishLoading(self)
            return
        }
        let end = min(delivered + 1024 * 1024, Self.body.count)
        let piece = Self.body.subdata(in: delivered..<end)
        delivered = end
        client?.urlProtocol(self, didLoad: piece)
    }
    override func stopLoading() {
        finished = true
        observation?.invalidate()
    }
}

final class WorkspacePreviewDownloadTests: XCTestCase {
    func testLargePreviewDownloadsCompletelyWithProgressAndRefusesHostLimit() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [LargePreviewProtocol.self]
        let api = PairingAPI(configuration: configuration)
        let connection = SavedConnection(origin: "https://large-preview.invalid", credential: Credential(
            sessionToken: "synthetic", deviceId: "phone", csrfToken: "csrf", hostInstallationId: "host", expiresAtMs: nil))
        let reports = LockedReports()
        let body = LargePreviewProtocol.body
        let data = try await api.downloadWorkspaceBytes("/api/v1/conversations/c/workspace/file?path=a.bin", connection: connection,
            byteSize: body.count, sha256: nil, mimeType: nil) { received, expected in reports.add(received, expected) }
        XCTAssertEqual(data, body)
        let seen = reports.values
        XCTAssertEqual(seen.last?.0, body.count)
        XCTAssertTrue(seen.allSatisfy { $0.1 == body.count })
        XCTAssertTrue(seen.contains { $0.0 > 0 && $0.0 < body.count }, "Large files report progress while they load; saw \(seen)")
        XCTAssertEqual(seen.map(\.0), seen.map(\.0).sorted(), "Progress never goes backwards")
        do {
            _ = try await api.downloadWorkspaceBytes("/api/v1/conversations/c/workspace/file?path=too-large", connection: connection,
                                                     byteSize: nil, sha256: nil, mimeType: nil)
            XCTFail("A host size refusal must not look like a missing file")
        } catch FileFailure.tooLarge {}
        do {
            _ = try await api.downloadWorkspaceBytes("/x", connection: connection,
                byteSize: PairingAPI.workspacePreviewLimit + 1, sha256: nil, mimeType: nil)
            XCTFail("Oversized listings are refused before transfer")
        } catch FileFailure.tooLarge {}
    }

    // B-040: an open preview asks whether its file changed and gets no body
    // while it hasn't; text reads only its start, which is never checked or
    // returned as the whole file; a Mac without either sends the whole file.
    func testPreviewRevalidatesWithoutABodyAndReadsOnlyTheStartOfText() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ConditionalPreviewProtocol.self]
        let api = PairingAPI(configuration: configuration)
        let connection = SavedConnection(origin: "https://conditional-preview.invalid", credential: Credential(
            sessionToken: "synthetic", deviceId: "phone", csrfToken: "csrf", hostInstallationId: "host", expiresAtMs: nil))
        let path = "/api/v1/conversations/c/workspace/file?root=r&path=notes.txt"
        let body = ConditionalPreviewProtocol.body

        guard case .read(let whole) = try await api.readWorkspaceFile(path, connection: connection, byteSize: body.count,
                                                                      sha256: nil, mimeType: "text/plain") else {
            return XCTFail("A first read has a body")
        }
        XCTAssertEqual(whole.data, body)
        XCTAssertFalse(whole.isPartial)
        XCTAssertEqual(whole.validator, ConditionalPreviewProtocol.tag)
        let unchanged = try await api.readWorkspaceFile(path, connection: connection, byteSize: nil, sha256: nil,
                                                        mimeType: "text/plain", ifNoneMatch: whole.validator)
        XCTAssertEqual(unchanged, .unchanged, "An unchanged file is not downloaded again")

        guard case .read(let start) = try await api.readWorkspaceFile(path, connection: connection, byteSize: body.count,
                                                                      sha256: nil, mimeType: "text/plain",
                                                                      prefix: 128 * 1024) else {
            return XCTFail("A prefix read has a body")
        }
        XCTAssertTrue(start.isPartial)
        XCTAssertEqual(start.data, body.prefix(128 * 1024))
        XCTAssertEqual(start.totalBytes, body.count)
        do {
            _ = try await api.downloadWorkspaceBytes(path + "&prefix=1024", connection: connection, byteSize: nil,
                                                     sha256: nil, mimeType: nil)
            XCTFail("A whole-file read never returns only the start")
        } catch {}
        do {
            _ = try await api.readWorkspaceFile(path, connection: connection, byteSize: nil,
                                                sha256: ConversationFile.digest(body), mimeType: nil, prefix: 1024)
            XCTFail("The start of a file can't be checked against the file's hash")
        } catch FileFailure.integrity {}
        do {
            _ = try await api.readWorkspaceFile(path + "&case=bad-range", connection: connection, byteSize: nil,
                                                sha256: nil, mimeType: nil, prefix: 1024)
            XCTFail("A range that doesn't describe the bytes is refused")
        } catch FileFailure.integrity {}

        guard case .read(let old) = try await api.readWorkspaceFile(path + "&case=old-host", connection: connection,
                                                                    byteSize: body.count, sha256: nil,
                                                                    mimeType: "text/plain", prefix: 1024) else {
            return XCTFail("An older Mac answers with the file")
        }
        XCTAssertEqual(old.data, body)
        XCTAssertFalse(old.isPartial)
        XCTAssertNil(old.validator)
        let verified = try await api.downloadWorkspaceBytes(path, connection: connection, byteSize: body.count,
                                                            sha256: ConversationFile.digest(body), mimeType: "text/plain")
        XCTAssertEqual(verified, body)

        XCTAssertEqual(PairingAPI.prefixTotal("bytes 0-9/11", received: 10), 11)
        XCTAssertNil(PairingAPI.prefixTotal("bytes 0-9/10", received: 10), "A range covering the file isn't partial")
        XCTAssertNil(PairingAPI.prefixTotal("bytes 1-10/20", received: 10))
        XCTAssertNoThrow(try PairingAPI.validatePrefix(Data("caf\u{E9}".utf8).dropLast(), mime: "text/plain"),
                         "A text start may end inside a character")
        XCTAssertThrowsError(try PairingAPI.validatePrefix(Data([0xff, 0xfe, 0xfd, 0xfc, 0xfb]), mime: "text/plain"))
    }

    func testPreviewLimitFollowsInstalledMemory() {
        let gigabyte: UInt64 = 1024 * 1024 * 1024
        let megabyte = 1024 * 1024
        // Reported RAM sits a little under the nominal size.
        XCTAssertEqual(PairingAPI.previewLimit(physicalMemory: 3 * gigabyte - 100 * UInt64(megabyte)), 92 * megabyte)
        XCTAssertEqual(PairingAPI.previewLimit(physicalMemory: 4 * gigabyte), 128 * megabyte, "iPhone 11")
        XCTAssertEqual(PairingAPI.previewLimit(physicalMemory: 6 * gigabyte), 192 * megabyte)
        XCTAssertEqual(PairingAPI.previewLimit(physicalMemory: 8 * gigabyte), 256 * megabyte)
        XCTAssertEqual(PairingAPI.previewLimit(physicalMemory: 12 * gigabyte), 256 * megabyte,
                       "The host refuses larger files, so more RAM does not raise the cap")
        XCTAssertEqual(PairingAPI.previewLimit(physicalMemory: 1 * gigabyte), 64 * megabyte)
    }
}

private final class LockedReports: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: [(Int, Int?)] = []
    func add(_ received: Int, _ expected: Int?) { lock.lock(); stored.append((received, expected)); lock.unlock() }
    var values: [(Int, Int?)] { lock.lock(); defer { lock.unlock() }; return stored }
}
