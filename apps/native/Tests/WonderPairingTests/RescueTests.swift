import CryptoKit
import XCTest
@testable import WonderPairing

/// A supervisor stand-in at the home address's recovery port. `answer` decides each reply by command.
private final class RescueStubProtocol: URLProtocol, @unchecked Sendable {
    struct Seen { let url: URL; let body: [String: Any] }
    nonisolated(unsafe) static var answer: @Sendable (String) -> (Int, String)? = { _ in nil }
    nonisolated(unsafe) static var seen: [Seen] = []
    private static let lock = NSLock()
    static func reset(_ answer: @escaping @Sendable (String) -> (Int, String)?) {
        lock.withLock { seen = []; Self.answer = answer }
    }
    static var requests: [Seen] { lock.withLock { seen } }

    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "192.168.1.20" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let url = request.url!
        var data = request.httpBody ?? Data()
        if data.isEmpty, let stream = request.httpBodyStream {
            stream.open(); defer { stream.close() }
            var buffer = [UInt8](repeating: 0, count: 4096)
            while stream.hasBytesAvailable { let n = stream.read(&buffer, maxLength: buffer.count); if n <= 0 { break }; data.append(buffer, count: n) }
        }
        let body = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        Self.lock.withLock { Self.seen.append(Seen(url: url, body: body)) }
        guard let (status, text) = Self.answer(url.lastPathComponent) else {
            client?.urlProtocol(self, didFailWithError: URLError(.cannotConnectToHost)); return
        }
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: url, statusCode: status, httpVersion: "HTTP/1.1",
                                                              headerFields: ["Content-Type": "application/json"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(text.utf8))
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

final class RescueTests: XCTestCase {
    private func connection(pin: String? = "pin", routes: [ConnectionRoute]? = nil) throws -> SavedConnection {
        let credential = try JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"s","deviceId":"phone","csrfToken":"csrf","hostInstallationId":"host"}"#.utf8))
        return try SavedConnection(routes: routes ?? [ConnectionRoute(kind: .home, origin: "https://192.168.1.20:3778"),
                                                       ConnectionRoute(kind: .tailscale, origin: "https://studio.tail1.ts.net")],
                                   credential: credential, hostName: "Studio", hostKeyPin: pin)
    }

    private func client(sign: @escaping @Sendable (Data) async throws -> String = { _ in "sig" }) -> RescueClient {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RescueStubProtocol.self]
        return RescueClient(api: PairingAPI(configuration: configuration), sign: sign, now: { 1_000 }, nonce: { "nonce-0123456789abcdef" })
    }

    private static let status = #"{"state":"stopped","lastExit":"signal: 9","crashCount":3,"version":"1.2.3"}"#

    /// The host's `RescueCommand::transcript_bytes`, byte for byte (rescue.rs
    /// `each_command_signs_its_own_action_and_path`): the empty body's hash, the
    /// fixed `rescue-v1` binding in the session's place and an empty expected state.
    func testTheSignedTranscriptIsTheOneTheHostBuilds() throws {
        let saved = try connection()
        XCTAssertEqual(String(decoding: RescueCommand.restart.transcript(connection: saved, nonce: "nonce", issuedAtMs: 7), as: UTF8.self),
                       "wonder-action-v1\nrescue.restart\n/api/v1/rescue/restart\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\nnonce\nrescue-v1\nphone\nhost\n7\n")
        XCTAssertEqual(SHA256.hash(data: Data()).map { String(format: "%02x", $0) }.joined(), RescueCommand.bodySHA256)
        XCTAssertEqual(Set(RescueCommand.allCases.map { String(decoding: $0.transcript(connection: saved, nonce: "n", issuedAtMs: 1), as: UTF8.self) }).count, 3)
        let body = try JSONSerialization.jsonObject(with: RescueCommand.status.body(connection: saved, nonce: "n", issuedAtMs: 1, signature: "s")) as? [String: Any]
        XCTAssertEqual(Set(body?.keys.map { $0 } ?? []), ["deviceId", "actionNonce", "issuedAtMs", "signature"])
    }

    func testOnlyHomeAddressesLeadToTheSupervisorOnePortAbove() throws {
        XCTAssertEqual(RescueRoute.origins(try connection()), ["https://192.168.1.20:3779"])
        let noPort = try connection(routes: [ConnectionRoute(kind: .home, origin: "https://192.168.1.20")])
        XCTAssertEqual(RescueRoute.origins(noPort), [])
    }

    func testStatusSignsAndPostsToTheRecoveryPort() async throws {
        RescueStubProtocol.reset { _ in (200, Self.status) }
        let status = try await client { transcript in
            XCTAssertTrue(String(decoding: transcript, as: UTF8.self).hasPrefix("wonder-action-v1\nrescue.status\n"))
            return "sig-1"
        }.status(try connection())
        XCTAssertEqual(status, RescueStatus(state: .stopped, lastExit: "signal: 9", crashCount: 3, version: "1.2.3"))
        let sent = try XCTUnwrap(RescueStubProtocol.requests.first)
        XCTAssertEqual(sent.url.absoluteString, "https://192.168.1.20:3779/api/v1/rescue/status")
        XCTAssertEqual(sent.body["signature"] as? String, "sig-1")
        XCTAssertEqual(sent.body["deviceId"] as? String, "phone")
        XCTAssertEqual(RescueStubProtocol.requests.count, 1, "no other address carries recovery")
    }

    func testAComputerPairedWithoutAKeyIsNeverAsked() async throws {
        RescueStubProtocol.reset { _ in (200, Self.status) }
        do { _ = try await client().status(try connection(pin: nil)); XCTFail("asked an unproven address") }
        catch { XCTAssertEqual(error as? RescueFailure, .unreachable) }
        XCTAssertTrue(RescueStubProtocol.requests.isEmpty)
    }

    func testTheHostsRefusalsBecomeFailuresThePhoneCanExplain() async throws {
        let cases: [(Int, String, RescueFailure)] = [
            (401, #"{"error":"unauthorized","detail":"x"}"#, .refused),
            (401, #"{"error":"stale","detail":"x"}"#, .clockOff),
            (429, #"{"error":"rate_limited","detail":"x","retryAfterSeconds":240}"#, .limited(retryAfterSeconds: 240)),
            (503, #"{"error":"unavailable","detail":"x"}"#, .unavailable),
        ]
        for (status, body, expected) in cases {
            RescueStubProtocol.reset { _ in (status, body) }
            do { _ = try await client().restart(try connection()); XCTFail("accepted a refusal") }
            catch { XCTAssertEqual(error as? RescueFailure, expected) }
        }
        RescueStubProtocol.reset { _ in nil }
        do { _ = try await client().restart(try connection()); XCTFail("answered") }
        catch { XCTAssertEqual(error as? RescueFailure, .unreachable) }
    }

    func testRestartAndDiagnosticsDecode() async throws {
        RescueStubProtocol.reset { command in
            command == "restart" ? (200, #"{"accepted":true,"state":"starting"}"#)
                : (200, #"{"status":\#(Self.status),"generatedAtMs":1700000000000,"logLines":["a","b"]}"#)
        }
        let reply = try await client().restart(try connection())
        XCTAssertEqual(reply.state, .starting)
        let diagnostics = try await client().diagnostics(try connection())
        XCTAssertEqual(diagnostics.logLines, ["a", "b"])
        let report = diagnostics.report(computer: "Studio")
        XCTAssertTrue(report.hasPrefix("Cupola diagnostics for Studio\n"))
        XCTAssertTrue(report.contains("Last stopped: signal: 9"))
        XCTAssertTrue(report.hasSuffix("Recent log\na\nb\n"))
    }

    func testTheNoticeSaysWhatHappenedAndOffersOnlyWhatWorks() {
        XCTAssertEqual(RescueNotice(RescueStatus(state: .stopped))?.message(computer: "Studio"), "Cupola stopped on Studio.")
        XCTAssertEqual(RescueNotice(RescueStatus(state: .gaveUp))?.cause, .stopped)
        XCTAssertEqual(RescueNotice(RescueStatus(state: .running))?.message(computer: "Studio"), "Cupola isn’t responding.")
        XCTAssertNil(RescueNotice(RescueStatus(state: .starting)), "a host that is starting needs no prompt")
        var notice = RescueNotice(cause: .stopped)
        XCTAssertTrue(notice.offersRestart); XCTAssertEqual(notice.restartTitle, "Restart")
        notice.phase = .restarting
        XCTAssertFalse(notice.offersRestart); XCTAssertTrue(notice.isRestarting)
        notice.phase = .stillDown
        XCTAssertTrue(notice.offersRestart); XCTAssertEqual(notice.restartTitle, "Restart again")
        notice.phase = RescueNotice.phase(after: .limited(retryAfterSeconds: 61))
        XCTAssertEqual(notice.phase, .limited(minutes: 2)); XCTAssertFalse(notice.offersRestart)
        XCTAssertEqual(RescueNotice.phase(after: .refused), .refused)
    }
}
