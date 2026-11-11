import Compression
import CryptoKit
#if canImport(Network)
import Network
#endif
import XCTest
@testable import WonderPairing

/// A stand-in for the host's `/api/v1/rpc/sealed`: it runs the host half of
/// the handshake with its own key and answers the calls the phone makes.
final class FakeSealedHost: SealedSocket, @unchecked Sendable {
    let origin: String
    let hostKey: P256.Signing.PrivateKey
    var pin: String { Data(SHA256.hash(data: hostKey.publicKey.derRepresentation)).base64URL }
    /// Answers `http.request` params with a result object.
    var answer: @Sendable ([String: Any]) -> [String: Any] = { _ in
        ["status": 200, "contentType": "application/json", "body": Data("{}".utf8).base64EncodedString()]
    }
    var historyCompression = false
    var eventBatches = false
    var tamperReplies = false
    var subscribeResult: @Sendable (FakeSealedHost) -> Void = { _ in }
    /// Calls to this method get no answer until the test sends one.
    var unansweredMethod: String?
    /// The computer's sessions and paired key, as `wonderd` checks them. Nil:
    /// every opening session is accepted and renewal is not offered.
    let devices: FakeDevices?
    /// The renewal challenge this connection issued, until it is answered.
    private var issued: Challenge?
    private var opened = false

    private let lock = NSLock()
    private var channel: SealedChannel?
    private var frames: [[String: Any]] = []
    private var largest = 0
    private var closed = false
    private let inbox: AsyncStream<SealedSocketMessage>
    private let deliver: AsyncStream<SealedSocketMessage>.Continuation
    private var iterator: AsyncStream<SealedSocketMessage>.Iterator

    init(origin: String, hostKey: P256.Signing.PrivateKey = .init(), devices: FakeDevices? = nil) {
        self.origin = origin
        self.hostKey = hostKey
        self.devices = devices
        (inbox, deliver) = AsyncStream.makeStream()
        iterator = inbox.makeAsyncIterator()
    }

    /// Every sealed message the phone sent, opened.
    var received: [[String: Any]] { lock.withLock { frames } }
    var isClosed: Bool { lock.withLock { closed } }
    /// The largest sealed frame sent to the phone, tag included.
    var largestFrame: Int { lock.withLock { largest } }

    func send(text: String) async throws {
        let (welcome, opened) = try SealedResponder.answer(hello: text, hostKey: hostKey, origin: origin)
        lock.withLock { channel = opened }
        deliver.yield(.text(welcome))
    }

    func send(data: Data) async throws {
        let (message, first, answering): ([String: Any], Bool, Challenge?) = try lock.withLock {
            guard var open = channel else { throw SealedChannelFailure.frame }
            let plaintext = try open.open(data)
            channel = open
            let message = try JSONSerialization.jsonObject(with: plaintext) as! [String: Any]
            frames.append(message)
            defer { opened = true; issued = nil }
            return (message, !opened, issued)
        }
        if let devices, first || answering != nil {
            opening(message, first: first, answering: answering, devices: devices)
            return
        }
        guard let id = message["id"] as? Int, let method = message["method"] as? String,
              method != unansweredMethod else { return }
        switch method {
        case "auth.challenge":
            let now = UInt64(Date().timeIntervalSince1970 * 1000)
            reply(id, ["challengeId": "challenge", "deviceId": "phone", "nonce": "n", "origin": origin,
                       "hostInstallationId": "host", "offerId": "", "issuedAtMs": now, "expiresAtMs": now + 60_000])
        case "initialize": reply(id, ["capabilities": ["eventBatches": eventBatches, "historyCompression": historyCompression ? ["deflate-raw"] : []]])
        case "http.request": reply(id, answer(message["params"] as? [String: Any] ?? [:]))
        case "events.subscribe":
            // The host queues the result before the subscription's events.
            reply(id, ["subscriptionId": "s"])
            subscribeResult(self)
        default: reply(id, [:])
        }
    }

    func reply(_ id: Int, _ result: [String: Any]) { push(["jsonrpc": "2.0", "id": id, "result": result]) }

    /// The opening frame, and a renewal's answer, as `wonderd`'s
    /// `open_sealed` reads them: nothing else is served before them, and any
    /// refusal is one `id: null` error before the socket closes.
    private func opening(_ message: [String: Any], first: Bool, answering: Challenge?, devices: FakeDevices) {
        func refuse() {
            push(["jsonrpc": "2.0", "id": NSNull(), "error": ["code": -32001, "message": "This device is not signed in to this computer. Pair it again."]])
            close()
        }
        if let answering {
            // Only the challenge issued on this connection, signed with the paired key.
            guard message.count == 2, message["challengeId"] as? String == answering.challengeId,
                  let signature = message["signature"] as? String, let renewed = devices.renew(answering, signature: signature) else { return refuse() }
            push(["renewed": renewed])
        } else if let session = message["session"] as? String, message.count == 1 {
            if !devices.accepts(session) { refuse() }
        } else if let renew = message["renew"] as? [String: Any], message.count == 1, devices.renews {
            guard renew["deviceId"] as? String == "phone", !devices.revoked else { return refuse() }
            let now = UInt64(Date().timeIntervalSince1970 * 1000)
            let challenge = try! JSONDecoder().decode(Challenge.self, from: JSONSerialization.data(withJSONObject: [
                "challengeId": UUID().uuidString, "deviceId": "phone", "nonce": UUID().uuidString,
                "origin": devices.challengeOrigin ?? origin, "hostInstallationId": "host", "offerId": "",
                "issuedAtMs": now, "expiresAtMs": now + 60_000]))
            lock.withLock { issued = challenge }
            push(["challenge": try! JSONSerialization.jsonObject(with: JSONEncoder().encode(challenge))])
        } else {
            // A host without renewal does not parse `renew`: it closes without a word.
            close()
        }
    }

    func push(_ message: [String: Any]) {
        let sealed: Data? = lock.withLock {
            guard var open = channel, let text = try? JSONSerialization.data(withJSONObject: message),
                  var frame = try? open.seal(text) else { return nil }
            channel = open
            if tamperReplies { frame[0] ^= 1 }
            largest = max(largest, frame.count)
            return frame
        }
        if let sealed { deliver.yield(.data(sealed)) }
    }

    func receive() async throws -> SealedSocketMessage {
        guard let next = await iterator.next() else { throw URLError(.networkConnectionLost) }
        return next
    }

    func close() {
        lock.withLock { closed = true }
        deliver.finish()
    }
}

/// The computer's record of the phone, shared by its connections: the
/// sessions it accepts and the key the phone paired with.
final class FakeDevices: @unchecked Sendable {
    let key: P256.Signing.PublicKey
    private let lock = NSLock()
    private var sessions: Set<String>
    private var state = (revoked: false, renews: true, challengeOrigin: String?.none, issuedSessions: 0)
    init(key: P256.Signing.PublicKey, sessions: Set<String> = []) { self.key = key; self.sessions = sessions }
    var revoked: Bool { get { lock.withLock { state.revoked } } set { lock.withLock { state.revoked = newValue } } }
    /// False: a computer from before renewal through the relay.
    var renews: Bool { get { lock.withLock { state.renews } } set { lock.withLock { state.renews = newValue } } }
    /// The address a renewal challenge names, when not the connection's own.
    var challengeOrigin: String? { get { lock.withLock { state.challengeOrigin } } set { lock.withLock { state.challengeOrigin = newValue } } }
    var issuedSessions: Int { lock.withLock { state.issuedSessions } }
    func accepts(_ session: String) -> Bool { lock.withLock { sessions.contains(session) } }

    /// A new session when `signature` is the paired key's over the challenge.
    func renew(_ challenge: Challenge, signature: String) -> [String: Any]? {
        let padded = signature.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        guard let raw = Data(base64Encoded: padded + String(repeating: "=", count: (4 - padded.count % 4) % 4)),
              let parsed = try? P256.Signing.ECDSASignature(rawRepresentation: raw),
              key.isValidSignature(parsed, for: challenge.transcript) else { return nil }
        return lock.withLock { () -> [String: Any]? in
            guard !state.revoked else { return nil }
            state.issuedSessions += 1
            let token = "renewed-\(state.issuedSessions)"
            sessions.insert(token)
            return ["sessionToken": token, "csrfToken": "csrf-\(state.issuedSessions)", "deviceId": "phone",
                    "hostInstallationId": "host", "expiresAtMs": UInt64(Date().timeIntervalSince1970 * 1000) + 3_600_000]
        }
    }
}

final class SealedRPCTests: XCTestCase {
    static let relay = "https://r-abcdefghijklmnopqrstuvwxyz.getcupola.app"
    let credential = try! JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"session-token","deviceId":"phone","csrfToken":"csrf-token","hostInstallationId":"host"}"#.utf8))

    private func connected(_ host: FakeSealedHost) async throws -> SealedRPC {
        let client = SealedRPC(socket: host, origin: host.origin)
        try await client.connect(pin: host.pin, credential: credential) { transcript in
            XCTAssertTrue(String(decoding: transcript, as: UTF8.self).hasPrefix("wonder-session-v1\nphone\nchallenge\n"))
            return "device-signature"
        }
        return client
    }

    /// The session goes only inside the channel, first; then the signed
    /// challenge and `initialize` carry the CSRF token and signature.
    func testTheSessionAndSignatureTravelOnlyInsideTheChannel() async throws {
        let host = FakeSealedHost(origin: Self.relay)
        _ = try await connected(host)
        let frames = host.received
        XCTAssertEqual(frames.first?["session"] as? String, "session-token")
        XCTAssertEqual(frames.dropFirst().compactMap { $0["method"] as? String }, ["auth.challenge", "initialize"])
        let auth = (frames.last?["params"] as? [String: Any])?["auth"] as? [String: Any]
        XCTAssertEqual(auth?["csrfToken"] as? String, "csrf-token")
        XCTAssertEqual(auth?["signature"] as? String, "device-signature")
        XCTAssertEqual(auth?["challengeId"] as? String, "challenge")
    }

    func testAnotherComputersKeyStopsBeforeTheSessionIsSent() async throws {
        let host = FakeSealedHost(origin: Self.relay)
        let client = SealedRPC(socket: host, origin: Self.relay)
        let otherPin = Data(SHA256.hash(data: P256.Signing.PrivateKey().publicKey.derRepresentation)).base64URL
        do {
            try await client.connect(pin: otherPin, credential: credential) { _ in "signature" }
            XCTFail("connected to another computer")
        } catch { XCTAssertEqual(error as? SealedChannelFailure, .hostProof) }
        XCTAssertTrue(host.received.isEmpty)
        XCTAssertTrue(host.isClosed)
    }

    /// An ordinary API call becomes `http.request` and comes back as the
    /// route's own status, error code and body, so refusals read as at home.
    func testHTTPCallsAnswerWithTheRoutesStatusAndBody() async throws {
        let host = FakeSealedHost(origin: Self.relay)
        host.answer = { _ in ["status": 403, "contentType": "text/plain", "errorCode": "worktree-not-in-project",
                              "body": Data("Add the folder".utf8).base64EncodedString()] }
        let client = try await connected(host)
        var request = URLRequest(url: URL(string: Self.relay + "/api/v1/conversations/c%201/terminals?x=1")!)
        request.httpMethod = "POST"
        request.httpBody = Data(#"{"cwd":"/"}"#.utf8)
        request.setValue("compact", forHTTPHeaderField: PairingAPI.compactViewHeader)
        let (data, response) = try await client.http(request)
        XCTAssertEqual(response.statusCode, 403)
        XCTAssertEqual(response.value(forHTTPHeaderField: PairingAPI.errorCodeHeader), "worktree-not-in-project")
        XCTAssertEqual(String(decoding: data, as: UTF8.self), "Add the folder")
        let params = host.received.last?["params"] as? [String: Any]
        XCTAssertEqual(params?["method"] as? String, "POST")
        XCTAssertEqual(params?["path"] as? String, "/api/v1/conversations/c%201/terminals?x=1")
        XCTAssertEqual(params?["body"] as? String, #"{"cwd":"/"}"#)
        XCTAssertEqual(params?["compact"] as? Bool, true)
        // A phone-sized picture keeps the hash it names through the channel.
        host.answer = { _ in ["status": 200, "contentType": "image/jpeg", "previewSha256": "abc",
                              "body": Data([255, 216, 255]).base64EncodedString()] }
        let (_, picture) = try await client.http(URLRequest(url: URL(string: Self.relay + "/api/v1/conversations/c/files/f?maxPixels=1080")!))
        XCTAssertEqual(picture.value(forHTTPHeaderField: PairingAPI.previewSha256Header), "abc")

        // A body past the channel's frame bound stays on the phone, with words,
        // including one that only JSON escaping takes past it.
        let sent = host.received.count
        let quotes = Data(repeating: UInt8(ascii: "\""), count: SealedBudget.request * 2 / 3)
        for body in [Data(repeating: 0x61, count: SealedBudget.request + 1), quotes] {
            request.httpBody = body
            do {
                _ = try await client.http(request)
                XCTFail("an oversized body was sent")
            } catch PairingFailure.hostMessage(413, let message) { XCTAssertTrue(message.contains("home network or Tailscale")) }
        }
        XCTAssertEqual(host.received.count, sent)
        // The same body unescaped fits.
        request.httpBody = Data(repeating: 0x61, count: quotes.count)
        _ = try await client.http(request)
        XCTAssertEqual(host.received.count, sent + 1)
        let open = await client.isOpen
        XCTAssertTrue(open)
    }

    func testNegotiatedHistoryCompressionUsesTheBuiltInBoundedDecoder() async throws {
        // RFC 1951 fixture from zlib, independent of Apple's decoder.
        let compressed = Data(base64Encoded: "Ky7JL0pNUcjILAYyKhXyS0sKSku4ikdFR0VHRUdFR0VHRUdFR0XpLgoA")!
        let expected = Data(String(repeating: "stored history output\n", count: 100).utf8)
        let host = FakeSealedHost(origin: Self.relay)
        host.historyCompression = true
        host.answer = { params in
            XCTAssertEqual(params["responseCompression"] as? String, "deflate-raw")
            return ["status": 200, "bodyEncoding": "deflate-raw", "uncompressedBytes": expected.count,
                    "body": compressed.base64EncodedString()]
        }
        let client = try await connected(host)
        let (data, _) = try await client.http(URLRequest(url: URL(string: Self.relay + "/api/v1/conversations/c/history")!))
        XCTAssertEqual(data, expected)
        for size in [0, expected.count - 1, expected.count + 1, 16 * 1024 * 1024 + 1] {
            XCTAssertThrowsError(try SealedRPC.decodeBody(compressed, encoding: "deflate-raw", size: size))
        }
        XCTAssertThrowsError(try SealedRPC.decodeBody(Data([0xff]), encoding: "deflate-raw", size: 10))
        XCTAssertThrowsError(try SealedRPC.decodeBody(compressed, encoding: "zstd", size: expected.count))
        await client.close()
    }

    func testBatchesRequireAdvertisementAndRemainIncrementalFrames() async throws {
        for supportsBatches in [false, true] {
            let host = FakeSealedHost(origin: Self.relay)
            host.eventBatches = supportsBatches
            let client = try await connected(host)
            let (_, events) = try await client.subscribe(hostEpoch: "epoch", after: 0)
            let params = host.received.last?["params"] as? [String: Any]
            XCTAssertEqual(params?["batch"] as? Bool, supportsBatches ? true : nil)
            let event: [String: Any] = ["eventId": "one", "hostEpoch": "epoch", "sequence": 1,
                "occurredAt": "now", "event": ["type": "host_status", "data": ["state": "ready"]]]
            host.push(["jsonrpc": "2.0", "method": supportsBatches ? "events.batch" : "event",
                       "params": ["subscriptionId": "s", supportsBatches ? "events" : "event": supportsBatches ? [event] as Any : event as Any]])
            var iterator = events.makeAsyncIterator()
            let data = try await iterator.next()
            let frame = try ReplayEvent.frame(XCTUnwrap(data))
            XCTAssertEqual(frame.batched, supportsBatches)
            XCTAssertEqual(frame.events.map(\.sequence), [1])
            await client.close()
        }
    }

    /// Overflow ends only the slow subscription, with resync and an
    /// unsubscribe to the host. The connection and its other calls stay up.
    func testSlowEventConsumerResyncsInsteadOfSilentlyDropping() async throws {
        let host = FakeSealedHost(origin: Self.relay)
        host.unansweredMethod = "slow.call"
        let client = try await connected(host)
        let inFlight = Task { try await client.call("slow.call", params: nil) }
        while !host.received.contains(where: { $0["method"] as? String == "slow.call" }) { await Task.yield() }
        let (id, events) = try await client.subscribe(hostEpoch: "epoch", after: 0)
        for sequence in 1...SealedRPC.eventBufferLimit + 1 {
            host.push(["jsonrpc": "2.0", "method": "event", "params": ["subscriptionId": "s", "event": ["sequence": sequence]]])
        }
        // A call ordered after the burst synchronizes with the socket reader.
        _ = try await client.call("host.status", params: nil)
        var sequences: [Int] = []
        do {
            for try await data in events {
                sequences.append((try JSONSerialization.jsonObject(with: data) as! [String: Int])["sequence"]!)
            }
            XCTFail("overflow finished without requesting resync")
        } catch { XCTAssertEqual(error as? SealedRPCFailure, .resync) }
        XCTAssertEqual(sequences, Array(1...SealedRPC.eventBufferLimit))
        let unsubscribed = host.received.contains {
            $0["method"] as? String == "events.unsubscribe"
                && ($0["params"] as? [String: Any])?["subscriptionId"] as? String == id
        }
        XCTAssertTrue(unsubscribed)
        // The call that was waiting when the subscription overflowed still answers.
        let open = await client.isOpen
        XCTAssertTrue(open)
        XCTAssertFalse(host.isClosed)
        let slowID = try XCTUnwrap(host.received.first { $0["method"] as? String == "slow.call" }?["id"] as? Int)
        host.reply(slowID, ["answered": true])
        let answer = try await inFlight.value
        XCTAssertEqual((try JSONSerialization.jsonObject(with: answer) as? [String: Bool])?["answered"], true)
        // A fresh subscription on the same connection works.
        let (_, next) = try await client.subscribe(hostEpoch: "epoch", after: 0)
        host.push(["jsonrpc": "2.0", "method": "event", "params": ["subscriptionId": "s", "event": ["sequence": 1]]])
        var iterator = next.makeAsyncIterator()
        let first = try await iterator.next()
        XCTAssertNotNil(first)
        await client.close()
    }

    func testEventsArriveInOrderAndATamperedFrameEndsTheConnection() async throws {
        let host = FakeSealedHost(origin: Self.relay)
        let client = try await connected(host)
        // The first event can arrive before the subscribe result is read.
        host.subscribeResult = { host in
            host.push(["jsonrpc": "2.0", "method": "event", "params": ["subscriptionId": "s", "event": ["sequence": 1]]])
        }
        let (id, events) = try await client.subscribe(hostEpoch: "epoch", after: 0)
        XCTAssertEqual(id, "s")
        let after = (host.received.last?["params"] as? [String: Any])?["after"] as? [String: Any]
        XCTAssertEqual(after?["hostEpoch"] as? String, "epoch")
        host.push(["jsonrpc": "2.0", "method": "event", "params": ["subscriptionId": "other", "event": ["sequence": 9]]])
        host.push(["jsonrpc": "2.0", "method": "event", "params": ["subscriptionId": "s", "event": ["sequence": 2]]])
        var sequences: [Int] = []
        for try await event in events {
            sequences.append((try JSONSerialization.jsonObject(with: event) as? [String: Any])?["sequence"] as? Int ?? 0)
            if sequences.count == 2 { break }
        }
        XCTAssertEqual(sequences, [1, 2])
        // Unsubscribing ends that stream.
        await client.unsubscribe(id)
        for try await _ in events { XCTFail("an event after unsubscribing") }
        // A subscription the host ends asks the reader to read state again.
        let (_, ending) = try await client.subscribe(hostEpoch: "old-epoch", after: 3)
        host.push(["jsonrpc": "2.0", "method": "events.ended", "params": ["subscriptionId": "s", "reason": "resync"]])
        do {
            for try await _ in ending {}
            XCTFail("the ended subscription finished quietly")
        } catch { XCTAssertEqual(error as? SealedRPCFailure, .resync) }

        host.tamperReplies = true
        do {
            _ = try await client.call("host.status", params: nil)
            XCTFail("a tampered reply was accepted")
        } catch { XCTAssertEqual(error as? SealedRPCFailure, .closed) }
        let open = await client.isOpen
        XCTAssertFalse(open)
        XCTAssertTrue(host.isClosed)
    }

    /// B-038: a session that ended while only the relay reaches the computer
    /// signs in again inside the channel, by the paired key answering a fresh
    /// challenge for this address. The ended session alone, another key, a
    /// challenge for another address or a revoked device get no session; a
    /// computer from before this renewal is told apart from a refusal.
    func testAnEndedSessionSignsInAgainInsideTheChannelOnlyWithThePairedKey() async throws {
        let key = P256.Signing.PrivateKey()
        let devices = FakeDevices(key: key.publicKey)
        let hostKey = P256.Signing.PrivateKey()
        func signer(_ key: P256.Signing.PrivateKey) -> SealedRPC.Sign { { try key.signature(for: $0).rawRepresentation.base64URL } }
        func attempt(_ sign: SealedRPC.Sign) async -> (FakeSealedHost, Result<Credential, Error>) {
            let host = FakeSealedHost(origin: Self.relay, hostKey: hostKey, devices: devices)
            let client = SealedRPC(socket: host, origin: Self.relay)
            do { return (host, .success(try await client.renew(pin: host.pin, credential: credential, sign: sign))) }
            catch { return (host, .failure(error)) }
        }

        // The ended session is refused before any call is answered.
        let ended = FakeSealedHost(origin: Self.relay, hostKey: hostKey, devices: devices)
        do {
            try await SealedRPC(socket: ended, origin: Self.relay).connect(pin: ended.pin, credential: credential, sign: signer(key))
            XCTFail("an ended session signed in")
        } catch { XCTAssertEqual(error as? SealedRPCFailure, .signedOut) }

        // Another key is refused, and nothing was issued.
        let (_, impostor) = await attempt(signer(P256.Signing.PrivateKey()))
        XCTAssertThrowsError(try impostor.get()) { XCTAssertEqual($0 as? SealedRPCFailure, .signedOut) }

        // A challenge naming another address is never signed or answered.
        devices.challengeOrigin = "https://elsewhere.example"
        let (elsewhereHost, elsewhere) = await attempt(signer(key))
        XCTAssertThrowsError(try elsewhere.get()) { guard case PairingFailure.wrongHost = $0 else { return XCTFail("\($0)") } }
        XCTAssertEqual(elsewhereHost.received.count, 1)
        devices.challengeOrigin = nil
        XCTAssertEqual(devices.issuedSessions, 0)

        // The paired key renews, then the connection signs in with the new session.
        let (host, result) = await attempt(signer(key))
        let renewed = try result.get()
        XCTAssertEqual(renewed.sessionToken, "renewed-1")
        XCTAssertEqual(renewed.deviceId, "phone")
        XCTAssertNotNil(renewed.expiresAtMs)
        let frames = host.received
        XCTAssertEqual(frames.first as? [String: [String: String]], ["renew": ["deviceId": "phone"]])
        XCTAssertEqual(Set(frames[1].keys), ["challengeId", "signature"])
        XCTAssertNil(frames.first?["session"])
        XCTAssertEqual(frames.dropFirst(2).compactMap { $0["method"] as? String }, ["auth.challenge", "initialize"])
        let auth = (frames.last?["params"] as? [String: Any])?["auth"] as? [String: Any]
        XCTAssertEqual(auth?["csrfToken"] as? String, "csrf-1")
        // The new session opens later connections directly.
        let next = FakeSealedHost(origin: Self.relay, hostKey: hostKey, devices: devices)
        try await SealedRPC(socket: next, origin: Self.relay).connect(pin: next.pin, credential: renewed, sign: signer(key))
        XCTAssertFalse(next.isClosed)

        // Once revoked, the paired key no longer renews.
        devices.revoked = true
        let (_, revoked) = await attempt(signer(key))
        XCTAssertThrowsError(try revoked.get()) { XCTAssertEqual($0 as? SealedRPCFailure, .signedOut) }
        devices.revoked = false

        // A computer from before this renewal closes without answering.
        devices.renews = false
        let (_, older) = await attempt(signer(key))
        XCTAssertThrowsError(try older.get()) { XCTAssertEqual($0 as? SealedRPCFailure, .renewalUnsupported) }
        XCTAssertEqual(devices.issuedSessions, 1)
    }

    /// PairingAPI answers a relay address whose computer refused the session
    /// with 401, as a direct address does, so the app renews rather than
    /// calling the computer unreachable; the renewed connection then carries
    /// the requests.
    func testPairingAPIRenewsARefusedRelaySessionInsideTheChannel() async throws {
        let key = P256.Signing.PrivateKey()
        let devices = FakeDevices(key: key.publicKey)
        let hosts = Hosts(origin: Self.relay, devices: devices)
        let api = PairingAPI(configuration: .ephemeral, sealedSocket: { _ in hosts.next() })
        api.markRelay([Self.relay])
        let sign: SealedRPC.Sign = { try key.signature(for: $0).rawRepresentation.base64URL }
        do {
            _ = try await api.sealedConnection(origin: Self.relay, credential: credential, pin: hosts.pin, sign: sign)
            XCTFail("an ended session signed in")
        } catch PairingFailure.response(401) {}
        struct Status: Decodable { let hostName: String }
        do {
            let _: Status = try await api.request("/api/v1/host/status", origin: Self.relay, credential: credential)
            XCTFail("a request ran without a session")
        } catch PairingFailure.response(401) {}

        let renewed = try await api.renewSealed(origin: Self.relay, credential: credential, pin: hosts.pin, sign: sign)
        let made = hosts.made.count
        let client = try await api.sealedConnection(origin: Self.relay, credential: renewed, pin: hosts.pin, sign: sign)
        XCTAssertEqual(hosts.made.count, made, "the renewed connection was opened again")
        XCTAssertTrue(api.sealed.client(Self.relay) === client)
        hosts.made.last?.answer = { _ in ["status": 200, "body": Data(#"{"hostName":"Studio"}"#.utf8).base64EncodedString()] }
        let status: Status = try await api.request("/api/v1/host/status", origin: Self.relay, credential: renewed)
        XCTAssertEqual(status.hostName, "Studio")

        // A refused device is a 401 too: its access really ended.
        devices.revoked = true
        do {
            _ = try await api.renewSealed(origin: Self.relay, credential: renewed, pin: hosts.pin, sign: sign)
            XCTFail("a revoked device renewed")
        } catch PairingFailure.response(401) {}
    }

    /// PairingAPI sends a relay address's requests through its sealed
    /// connection and never sends one in plaintext, with or without one.
    func testPairingAPIUsesOnlyTheSealedConnectionForARelayAddress() async throws {
        let api = PairingAPI()
        struct Status: Decodable { let hostName: String }
        do {
            let _: Status = try await api.request("/api/v1/host/status", origin: Self.relay, credential: credential)
            XCTFail("sent without a sealed connection")
        } catch { XCTAssertEqual((error as? URLError)?.code, .networkConnectionLost) }
        let saved = try SavedConnection(origin: Self.relay, credential: credential)
        XCTAssertThrowsError(try api.eventSocket(connection: saved))
        // Terminal and workspace previews explain the relay, not a dropped network.
        XCTAssertThrowsError(try api.terminalSocket(connection: saved, terminalID: "term_1")) {
            XCTAssertEqual($0 as? RelayUnsupported, .terminal)
        }
        do {
            _ = try await api.downloadWorkspaceBytes("/x", connection: saved, byteSize: 1, sha256: nil, mimeType: nil)
            XCTFail("streamed a preview without a sealed connection")
        } catch { XCTAssertEqual(error as? RelayUnsupported, .workspacePreview) }
        do {
            _ = try await api.downloadWorkspaceMediaRange("/x", connection: saved, start: 0, end: 0, revision: nil)
            XCTFail("streamed media without a sealed connection")
        } catch { XCTAssertEqual(error as? RelayUnsupported, .workspacePreview) }

        let host = FakeSealedHost(origin: Self.relay)
        host.answer = { _ in ["status": 200, "body": Data(#"{"hostName":"Studio"}"#.utf8).base64EncodedString()] }
        let client = try await connected(host)
        _ = api.sealed.install(client, for: Self.relay)
        let status: Status = try await api.request("/api/v1/host/status", origin: Self.relay, credential: credential)
        XCTAssertEqual(status.hostName, "Studio")
        XCTAssertEqual((host.received.last?["params"] as? [String: Any])?["path"] as? String, "/api/v1/host/status")

        // A connection that ended is dropped; the next request reports the loss.
        await client.close()
        do {
            let _: Status = try await api.request("/api/v1/host/status", origin: Self.relay, credential: credential)
            XCTFail("used a closed connection")
        } catch { XCTAssertEqual((error as? URLError)?.code, .networkConnectionLost) }
        XCTAssertNil(api.sealed.client(Self.relay))
    }

    /// A renewed session opens a new connection signed in with it, so calls
    /// never ride on a session that is about to expire.
    func testARenewedSessionReplacesTheOpenConnection() async throws {
        let hosts = Hosts(origin: Self.relay)
        let api = PairingAPI(configuration: .ephemeral, sealedSocket: { _ in hosts.next() })
        let pin = hosts.pin
        let first = try await api.sealedConnection(origin: Self.relay, credential: credential, pin: pin) { _ in "s" }
        let same = try await api.sealedConnection(origin: Self.relay, credential: credential, pin: pin) { _ in "s" }
        XCTAssertTrue(first === same)
        let renewed = try JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"renewed","deviceId":"phone","csrfToken":"csrf-2","hostInstallationId":"host"}"#.utf8))
        let second = try await api.sealedConnection(origin: Self.relay, credential: renewed, pin: pin) { _ in "s" }
        XCTAssertFalse(first === second)
        let firstOpen = await first.isOpen
        XCTAssertFalse(firstOpen)
        XCTAssertEqual(hosts.made.last?.received.first?["session"] as? String, "renewed")
    }

    /// Callers that ask while a connection opens share it. Each opening its own
    /// would have the later one replace, and close, the earlier caller's.
    func testConcurrentCallersShareOneOpeningConnection() async throws {
        let hosts = Hosts(origin: Self.relay)
        let api = PairingAPI(configuration: .ephemeral, sealedSocket: { _ in hosts.next() })
        let pin = hosts.pin
        let credential = credential
        async let a = api.sealedConnection(origin: Self.relay, credential: credential, pin: pin) { _ in "s" }
        async let b = api.sealedConnection(origin: Self.relay, credential: credential, pin: pin) { _ in "s" }
        async let c = api.sealedConnection(origin: Self.relay, credential: credential, pin: pin) { _ in "s" }
        let (first, second, third) = try await (a, b, c)
        XCTAssertTrue(first === second && second === third)
        XCTAssertEqual(hosts.made.count, 1, "one connection was opened")
        let open = await first.isOpen
        XCTAssertTrue(open)
        // Once it is open, a later caller takes the open one and nothing stays pending.
        let later = try await api.sealedConnection(origin: Self.relay, credential: credential, pin: pin) { _ in "s" }
        XCTAssertTrue(later === first)
    }

    /// Leaving the conversation detaches the connections open then and closes
    /// them later; a connection a later check opened in between stays open,
    /// whichever order the close and the new connection happen in.
    func testDetachedConnectionsCloseWithoutTouchingANewerOne() async throws {
        for newerFirst in [true, false] {
            let hosts = Hosts(origin: Self.relay)
            let api = PairingAPI(configuration: .ephemeral, sealedSocket: { _ in hosts.next() })
            let older = try await api.sealedConnection(origin: Self.relay, credential: credential, pin: hosts.pin) { _ in "s" }
            let ending = api.detachSealed()
            XCTAssertEqual(ending.count, 1)
            var newer: SealedRPC?
            if newerFirst { newer = try await api.sealedConnection(origin: Self.relay, credential: credential, pin: hosts.pin) { _ in "s" } }
            for client in ending { await client.close() }
            if !newerFirst { newer = try await api.sealedConnection(origin: Self.relay, credential: credential, pin: hosts.pin) { _ in "s" } }
            let olderOpen = await older.isOpen, newerOpen = await newer!.isOpen
            XCTAssertFalse(olderOpen)
            XCTAssertTrue(newerOpen, "newer first: \(newerFirst)")
            XCTAssertTrue(api.sealed.client(Self.relay) === newer)
        }
    }

    private func credential(_ token: String) -> Credential {
        try! JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"\#(token)","deviceId":"phone","csrfToken":"csrf","hostInstallationId":"host"}"#.utf8))
    }

    /// A call still in flight on a replaced connection fails `.closed` after the
    /// renewal installed its successor; that late failure must not unregister the
    /// successor, or the next caller opens a duplicate socket.
    func testLateClosedOnReplacedConnectionDoesNotUnregisterTheNewOne() async throws {
        let hosts = Hosts(origin: Self.relay)
        let api = PairingAPI(configuration: .ephemeral, sealedSocket: { _ in hosts.next() })
        let old = credential("old"), renewed = credential("renewed")
        let first = try await api.sealedConnection(origin: Self.relay, credential: old, pin: hosts.pin) { _ in "s" }
        hosts.made[0].unansweredMethod = "http.request"
        struct Status: Decodable { let hostName: String }
        let inFlight = Task { () -> Error? in
            do { let _: Status = try await api.request("/api/v1/host/status", origin: Self.relay, credential: old); return nil }
            catch { return error }
        }
        for _ in 0..<500 where !hosts.made[0].received.contains(where: { $0["method"] as? String == "http.request" }) {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(hosts.made[0].received.contains { $0["method"] as? String == "http.request" }, "call is in flight")
        let second = try await api.sealedConnection(origin: Self.relay, credential: renewed, pin: hosts.pin) { _ in "s" }
        XCTAssertFalse(first === second)
        let failure = await inFlight.value
        XCTAssertEqual((failure as? URLError)?.code, .networkConnectionLost)
        XCTAssertTrue(api.sealed.client(Self.relay) === second, "the replaced connection's late failure unregistered its successor")
        let third = try await api.sealedConnection(origin: Self.relay, credential: renewed, pin: hosts.pin) { _ in "s" }
        XCTAssertTrue(third === second)
        XCTAssertEqual(hosts.made.count, 2, "no duplicate socket")
    }

    /// Route transitions decide by identity: a superseded opening cannot sign the
    /// address out or install over its successor, and a connection removes only itself.
    func testRouteTransitionsDecideByIdentity() async throws {
        let routes = SealedRoutes()
        let origin = Self.relay
        let hosts = Hosts(origin: origin)
        func client() -> SealedRPC { SealedRPC(socket: hosts.next(), origin: origin) }
        let idle = Task<SealedRPC, Error> { try await Task.sleep(for: .seconds(60)); throw CancellationError() }
        defer { idle.cancel() }
        let (stale, _) = routes.joinConnect(origin: origin, token: "old") { _ in idle }
        let (current, _) = routes.joinConnect(origin: origin, token: "renewed") { _ in idle }
        XCTAssertNotEqual(stale, current)

        // The superseded opening's refusal changes nothing; its connection is not installed.
        XCTAssertFalse(routes.signOut(origin, opening: stale).applied)
        XCTAssertFalse(routes.isSignedOut(origin))
        let lost = client()
        guard case .superseded = routes.install(lost, for: origin, opening: stale) else { return XCTFail("stale opening installed") }

        // The current opening installs; a later stale refusal still changes nothing.
        let a = client()
        guard case .installed(let none) = routes.install(a, for: origin, opening: current), none == nil else { return XCTFail("current opening did not install") }
        XCTAssertFalse(routes.signOut(origin, opening: stale).applied)
        XCTAssertTrue(routes.client(origin) === a)

        // A renewal takes the address from `a`; a late failure of `a` leaves the renewal in place.
        let b = client()
        guard case .installed(let replaced) = routes.install(b, for: origin), replaced === a else { return XCTFail("renewal did not replace") }
        XCTAssertFalse(routes.remove(a, for: origin))
        XCTAssertTrue(routes.client(origin) === b)
        XCTAssertTrue(routes.remove(b, for: origin))
        XCTAssertNil(routes.client(origin))

        // Open and signed out exclude each other.
        let c = client()
        let (refused, _) = routes.joinConnect(origin: origin, token: "t") { _ in idle }
        let signOut = routes.signOut(origin, opening: refused)
        XCTAssertTrue(signOut.applied)
        XCTAssertTrue(routes.isSignedOut(origin))
        XCTAssertNil(routes.client(origin))
        _ = routes.install(c, for: origin)
        XCTAssertFalse(routes.isSignedOut(origin))
        XCTAssertTrue(routes.client(origin) === c)
    }

    // MARK: Budgets

    /// Bytes deflate cannot shrink, like a photo.
    private static func incompressible(_ count: Int) -> Data {
        var state: UInt64 = 0x9e37_79b9_7f4a_7c15
        var bytes = [UInt8](repeating: 0, count: count)
        for index in bytes.indices {
            state ^= state << 13
            state ^= state >> 7
            state ^= state << 17
            bytes[index] = UInt8(truncatingIfNeeded: state)
        }
        return Data(bytes)
    }

    /// RFC 1951 DEFLATE, as the host's history compression sends it.
    private static func deflate(_ data: Data) throws -> Data {
        var output = Data(count: data.count / 64 + 4096)
        let written = output.withUnsafeMutableBytes { out in
            data.withUnsafeBytes { input in
                compression_encode_buffer(out.bindMemory(to: UInt8.self).baseAddress!, out.count,
                    input.bindMemory(to: UInt8.self).baseAddress!, data.count, nil, COMPRESSION_ZLIB)
            }
        }
        guard written > 0 else { throw SealedChannelFailure.frame }
        output.count = written
        return output
    }

    /// An attachment at the host's whole body budget that compression can't
    /// shrink comes through as one frame the phone reads: far past
    /// URLSession's 1 MiB default, within the phone's raised limit.
    func testAFullSizeIncompressibleAttachmentFitsTheFrameThePhoneReads() async throws {
        let host = FakeSealedHost(origin: Self.relay)
        let original = Self.incompressible(SealedBudget.decodedBody)
        host.answer = { _ in ["status": 200, "contentType": "image/png", "body": original.base64EncodedString()] }
        let client = try await connected(host)
        let (data, response) = try await client.http(URLRequest(url: URL(string: Self.relay + "/api/v1/conversations/c/files/f")!))
        XCTAssertEqual(response.statusCode, 200)
        XCTAssertEqual(data, original)
        XCTAssertGreaterThan(host.largestFrame, 1024 * 1024)
        XCTAssertLessThanOrEqual(host.largestFrame, SealedBudget.responseWire)
        let open = await client.isOpen
        XCTAssertTrue(open)
        await client.close()
    }

    /// Stored history compressed by the host decodes up to the body budget.
    func testCompressedHistoryDecodesUpToTheBodyBudget() async throws {
        let history = Data(repeating: UInt8(ascii: "a"), count: SealedBudget.decodedBody)
        let compressed = try Self.deflate(history)
        let host = FakeSealedHost(origin: Self.relay)
        host.historyCompression = true
        host.answer = { _ in ["status": 200, "bodyEncoding": "deflate-raw", "uncompressedBytes": history.count,
                              "body": compressed.base64EncodedString()] }
        let client = try await connected(host)
        let (data, _) = try await client.http(URLRequest(url: URL(string: Self.relay + "/api/v1/conversations/c/history")!))
        XCTAssertEqual(data, history)
        XCTAssertThrowsError(try SealedRPC.decodeBody(compressed, encoding: "deflate-raw", size: SealedBudget.decodedBody + 1))
        await client.close()
    }

    /// An answer the host refuses for its size arrives as a 413 inside the
    /// channel, in words; the connection, a waiting call and live events
    /// carry on.
    func testATooLargeAnswerIsRefusedWithoutEndingTheConnection() async throws {
        let host = FakeSealedHost(origin: Self.relay)
        host.unansweredMethod = "slow.call"
        let detail = "This is too large to open over Reach from anywhere. Connect on the home network or Tailscale, then try again."
        host.answer = { _ in ["status": 413, "contentType": "application/json",
                              "body": try! JSONSerialization.data(withJSONObject: ["detail": detail]).base64EncodedString()] }
        let client = try await connected(host)
        let inFlight = Task { try await client.call("slow.call", params: nil) }
        while !host.received.contains(where: { $0["method"] as? String == "slow.call" }) { await Task.yield() }
        let (_, events) = try await client.subscribe(hostEpoch: "epoch", after: 0)

        let (data, response) = try await client.http(URLRequest(url: URL(string: Self.relay + "/api/v1/conversations/c/files/f")!))
        XCTAssertEqual(response.statusCode, 413)
        XCTAssertEqual((try JSONSerialization.jsonObject(with: data) as? [String: String])?["detail"], detail)

        host.push(["jsonrpc": "2.0", "method": "event", "params": ["subscriptionId": "s", "event": ["sequence": 1]]])
        var iterator = events.makeAsyncIterator()
        let event = try await iterator.next()
        XCTAssertNotNil(event)
        let slowID = try XCTUnwrap(host.received.first { $0["method"] as? String == "slow.call" }?["id"] as? Int)
        host.reply(slowID, ["answered": true])
        let answer = try await inFlight.value
        XCTAssertEqual((try JSONSerialization.jsonObject(with: answer) as? [String: Bool])?["answered"], true)
        let open = await client.isOpen
        XCTAssertTrue(open)
        XCTAssertFalse(host.isClosed)
        await client.close()
    }

    #if canImport(Network)
    /// The socket PairingAPI opens for the relay reads a frame as large as the
    /// host may send, and only that: one byte more is refused by URLSession
    /// itself, which is why the host refuses larger replies inside the channel.
    func testTheRelaySocketReadsFramesUpToTheHostsReplyBudget() async throws {
        let limit = SealedBudget.responseWire
        let cases = [(limit - 1, true), (limit, true), (limit + 1, false)]
        let server = try FrameServer(sizes: cases.map { $0.0 })
        defer { server.stop() }
        let port = try await server.start()
        let session = URLSession(configuration: .ephemeral)
        defer { session.invalidateAndCancel() }
        for (size, fits) in cases {
            let request = URLRequest(url: URL(string: "ws://127.0.0.1:\(port)/api/v1/rpc/sealed")!, timeoutInterval: 30)
            let socket = WebSocketSealedSocket.open(request, in: session)
            XCTAssertEqual(socket.task.maximumMessageSize, limit)
            do {
                guard case .data(let data) = try await socket.receive() else { XCTFail("text frame"); continue }
                XCTAssertTrue(fits, "read a \(size)-byte frame past the budget")
                XCTAssertEqual(data.count, size)
            } catch {
                XCTAssertFalse(fits, "refused a \(size)-byte frame: \(error)")
            }
            socket.close()
        }
    }
    #endif

    // MARK: Deadlines and cancellation

    /// A call with no answer by its deadline fails alone, as URLSession's
    /// timeout would; the connection stays up and a late answer is ignored.
    func testACallWithoutAnAnswerByItsDeadlineTimesOutAlone() async throws {
        let host = FakeSealedHost(origin: Self.relay)
        host.unansweredMethod = "slow.call"
        let client = try await connected(host)
        let started = ContinuousClock.now
        do {
            _ = try await client.call("slow.call", params: nil, deadline: .milliseconds(100))
            XCTFail("waited past the deadline")
        } catch { XCTAssertEqual((error as? URLError)?.code, .timedOut) }
        XCTAssertLessThan(ContinuousClock.now - started, .seconds(10))
        let slowID = try XCTUnwrap(host.received.first { $0["method"] as? String == "slow.call" }?["id"] as? Int)
        host.reply(slowID, ["late": true])
        _ = try await client.call("host.status", params: nil)
        XCTAssertEqual(host.received.filter { $0["method"] as? String == "slow.call" }.count, 1)
        var open = await client.isOpen
        XCTAssertTrue(open)

        // `http.request` waits as long as the request's own timeout, which
        // PairingAPI sets per route.
        host.unansweredMethod = "http.request"
        var request = URLRequest(url: URL(string: Self.relay + "/api/v1/host/status")!)
        request.timeoutInterval = 0.1
        do {
            _ = try await client.http(request)
            XCTFail("waited past the request's timeout")
        } catch { XCTAssertEqual((error as? URLError)?.code, .timedOut) }
        open = await client.isOpen
        XCTAssertTrue(open)
        await client.close()
    }

    /// A cancelled call ends at once, whether it was cancelled after it was
    /// sent or before it started; nothing is sent twice, and the frames after
    /// it still open in order.
    func testCancellingACallEndsItWithoutResendingOrHanging() async throws {
        let host = FakeSealedHost(origin: Self.relay)
        host.unansweredMethod = "slow.call"
        let client = try await connected(host)
        let sent = Task { try await client.call("slow.call", params: nil) }
        while !host.received.contains(where: { $0["method"] as? String == "slow.call" }) { await Task.yield() }
        sent.cancel()
        do {
            _ = try await sent.value
            XCTFail("a cancelled call answered")
        } catch { XCTAssertTrue(error is CancellationError, "\(error)") }

        let started = ContinuousClock.now
        let early = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await client.call("early.call", params: nil)
        }
        do {
            _ = try await early.value
            XCTFail("a call cancelled before it started was sent")
        } catch { XCTAssertTrue(error is CancellationError, "\(error)") }
        XCTAssertLessThan(ContinuousClock.now - started, .seconds(5))

        _ = try await client.call("host.status", params: nil)
        let methods = host.received.compactMap { $0["method"] as? String }
        XCTAssertEqual(methods.filter { $0 == "slow.call" }.count, 1)
        XCTAssertFalse(methods.contains("early.call"))
        XCTAssertEqual(methods.last, "host.status")
        let open = await client.isOpen
        XCTAssertTrue(open)
        await client.close()
    }

    /// A route check that gives up on a relay which never sends its welcome
    /// ends at once and closes only that candidate's socket; a WebSocket
    /// receive alone would wait for the relay.
    func testACancelledCheckEndsAHandshakeTheRelayNeverWelcomes() async throws {
        let socket = SilentSealedSocket()
        let client = SealedRPC(socket: socket, origin: Self.relay)
        let credential = credential, ended = Flag()
        let attempt = Task {
            defer { ended.set() }
            try await client.connect(pin: "pin", credential: credential) { _ in "signature" }
        }
        for _ in 0..<10_000 where !socket.isReceiving { try await Task.sleep(for: .milliseconds(1)) }
        XCTAssertTrue(socket.isReceiving)
        attempt.cancel()
        for _ in 0..<10_000 where !ended.isSet { try await Task.sleep(for: .milliseconds(1)) }
        XCTAssertTrue(ended.isSet, "The handshake outlived its cancelled check")
        XCTAssertTrue(socket.isClosed)
        socket.close() // Lets a stalled attempt end so the test reports instead of hanging.
        do { try await attempt.value; XCTFail("connected without a welcome") } catch is CancellationError { }
        let open = await client.isOpen
        XCTAssertFalse(open)
    }

    /// After the welcome, a host that never answers sign-in is bounded the same way.
    func testACancelledCheckEndsAHandshakeStalledAfterTheWelcome() async throws {
        for method in ["auth.challenge", "initialize"] {
            let host = FakeSealedHost(origin: Self.relay)
            host.unansweredMethod = method
            let client = SealedRPC(socket: host, origin: Self.relay)
            let credential = credential, pin = host.pin
            let attempt = Task { try await client.connect(pin: pin, credential: credential) { _ in "signature" } }
            for _ in 0..<10_000 where !host.received.contains(where: { $0["method"] as? String == method }) {
                try await Task.sleep(for: .milliseconds(1))
            }
            attempt.cancel()
            do { try await attempt.value; XCTFail("signed in without an answer to \(method)") } catch is CancellationError { }
            XCTAssertTrue(host.isClosed, method)
            let open = await client.isOpen
            XCTAssertFalse(open, method)
        }
    }

    /// The shared opening still ends for a cancelled caller: the last waiter to
    /// leave cancels it, so a route check or sign-out is bounded by its deadline
    /// even through `PairingAPI.sealedConnection`.
    func testCancellingEverySharedCallerEndsAnOpeningTheRelayNeverWelcomes() async throws {
        let socket = SilentSealedSocket()
        let api = PairingAPI(configuration: .ephemeral, sealedSocket: { _ in socket })
        let credential = credential, ended = Flag()
        func caller() -> Task<SealedRPC, Error> {
            Task {
                defer { ended.set() }
                return try await api.sealedConnection(origin: Self.relay, credential: credential, pin: "pin") { _ in "s" }
            }
        }
        let firstEnded = Flag()
        let first = Task {
            defer { firstEnded.set() }
            return try await api.sealedConnection(origin: Self.relay, credential: credential, pin: "pin") { _ in "s" }
        }
        let second = caller()
        for _ in 0..<10_000 where !socket.isReceiving { try await Task.sleep(for: .milliseconds(1)) }
        XCTAssertTrue(socket.isReceiving)
        first.cancel()
        for _ in 0..<1_000 where !firstEnded.isSet { try await Task.sleep(for: .milliseconds(1)) }
        XCTAssertTrue(firstEnded.isSet, "a cancelled caller stayed held by the opening another caller still shares")
        XCTAssertFalse(socket.isClosed, "one caller leaving must not end the opening the other still waits on")
        XCTAssertFalse(ended.isSet)
        second.cancel()
        for _ in 0..<10_000 where !ended.isSet { try await Task.sleep(for: .milliseconds(1)) }
        XCTAssertTrue(socket.isClosed, "the last cancelled caller left the opening running")
        socket.close() // Lets a stalled attempt end so the test reports instead of hanging.
        for task in [first, second] {
            do { _ = try await task.value; XCTFail("connected without a welcome") } catch is CancellationError { }
        }
    }
}

/// A relay that never answers, with URLSession's WebSocket behavior: a
/// pending receive ignores task cancellation and ends only when the socket closes.
private final class SilentSealedSocket: SealedSocket, @unchecked Sendable {
    private let lock = NSLock()
    private var waiting: CheckedContinuation<SealedSocketMessage, Error>?
    private var closed = false
    var isReceiving: Bool { lock.withLock { waiting != nil } }
    var isClosed: Bool { lock.withLock { closed } }
    func send(text: String) async throws { if isClosed { throw URLError(.cancelled) } }
    func send(data: Data) async throws { if isClosed { throw URLError(.cancelled) } }
    func receive() async throws -> SealedSocketMessage {
        try await withCheckedThrowingContinuation { continuation in
            let refused = lock.withLock { () -> Bool in
                if closed { return true }
                waiting = continuation
                return false
            }
            if refused { continuation.resume(throwing: URLError(.cancelled)) }
        }
    }
    func close() {
        let waiter = lock.withLock { () -> CheckedContinuation<SealedSocketMessage, Error>? in
            closed = true
            defer { waiting = nil }
            return waiting
        }
        waiter?.resume(throwing: URLError(.cancelled))
    }
}

private final class Flag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    var isSet: Bool { lock.withLock { value } }
    func set() { lock.withLock { value = true } }
}

/// Stand-in computers sharing one host key, one per connection.
private final class Hosts: @unchecked Sendable {
    let origin: String
    let hostKey = P256.Signing.PrivateKey()
    let devices: FakeDevices?
    private let lock = NSLock()
    private var all: [FakeSealedHost] = []
    init(origin: String, devices: FakeDevices? = nil) { self.origin = origin; self.devices = devices }
    var pin: String { Data(SHA256.hash(data: hostKey.publicKey.derRepresentation)).base64URL }
    var made: [FakeSealedHost] { lock.withLock { all } }
    func next() -> FakeSealedHost {
        let host = FakeSealedHost(origin: origin, hostKey: hostKey, devices: devices)
        lock.withLock { all.append(host) }
        return host
    }
}

#if canImport(Network)
/// A loopback WebSocket server that sends each connection, in order, one
/// binary message of the next size.
private final class FrameServer: @unchecked Sendable {
    private let listener: NWListener
    private let lock = NSLock()
    private var sizes: [Int]
    private var connections: [NWConnection] = []

    init(sizes: [Int]) throws {
        self.sizes = sizes
        let parameters = NWParameters.tcp
        parameters.requiredInterfaceType = .loopback
        parameters.defaultProtocolStack.applicationProtocols.insert(NWProtocolWebSocket.Options(), at: 0)
        listener = try NWListener(using: parameters, on: .any)
        listener.newConnectionHandler = { [weak self] connection in self?.serve(connection) }
    }

    /// Starts listening and returns the port.
    func start() async throws -> UInt16 {
        listener.start(queue: .global())
        for _ in 0..<500 {
            if let port = listener.port, port.rawValue != 0 { return port.rawValue }
            try await Task.sleep(for: .milliseconds(10))
        }
        throw URLError(.cannotConnectToHost)
    }

    private func serve(_ connection: NWConnection) {
        let size: Int = lock.withLock {
            connections.append(connection)
            return sizes.isEmpty ? 0 : sizes.removeFirst()
        }
        connection.stateUpdateHandler = { state in
            guard case .ready = state else { return }
            let context = NWConnection.ContentContext(identifier: "frame", metadata: [NWProtocolWebSocket.Metadata(opcode: .binary)])
            connection.send(content: Data(repeating: 0x61, count: size), contentContext: context, isComplete: true,
                            completion: .idempotent)
        }
        connection.start(queue: .global())
    }

    func stop() {
        listener.cancel()
        for connection in lock.withLock({ connections }) { connection.cancel() }
    }
}
#endif
