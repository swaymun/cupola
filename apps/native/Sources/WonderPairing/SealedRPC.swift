import Foundation
import Compression

/// The socket under a sealed connection: text for the handshake, binary after.
public protocol SealedSocket: Sendable {
    func send(text: String) async throws
    func send(data: Data) async throws
    /// The next message: text or binary.
    func receive() async throws -> SealedSocketMessage
    func close()
}

public enum SealedSocketMessage: Sendable { case text(String), data(Data) }

/// What one sealed connection carries, each matching the host's budget in
/// `crates/wonderd/src/rpc/server.rs`. A frame past a socket's limit closes
/// the whole connection and every call on it, so the phone reads everything
/// the host may send, and the host refuses anything larger with a small 413
/// inside the channel.
public enum SealedBudget {
    /// The largest call the phone sends, under the host's 256 KiB ingress
    /// (`MAX_REQUEST_BYTES`) with room for the envelope and sealing.
    public static let request = 192 * 1024
    /// The largest frame the phone reads, sealing tag included: the host's
    /// `MAX_RESPONSE_WIRE_BYTES`, room for a `decodedBody` answer as base64.
    public static let responseWire = 24 * 1024 * 1024
    /// The largest event or event batch one subscription takes, decoded
    /// (`wonder_api::EVENT_FRAME_MAX_BYTES`).
    public static let eventPlaintext = 1024 * 1024
    /// The largest body an answer decodes or decompresses to: the host's
    /// `MAX_RESULT_BYTES`.
    public static let decodedBody = 16 * 1024 * 1024
}

/// A URLSession WebSocket as a sealed connection's socket.
public struct WebSocketSealedSocket: SealedSocket {
    public let task: URLSessionWebSocketTask
    public init(task: URLSessionWebSocketTask) { self.task = task }
    /// Opens `request` as a sealed connection, reading frames up to the host's
    /// reply budget. URLSession's own limit (1 MiB) refuses a larger frame and
    /// closes the connection, so it is raised before the task starts.
    public static func open(_ request: URLRequest, in session: URLSession) -> WebSocketSealedSocket {
        let task = session.webSocketTask(with: request)
        task.maximumMessageSize = SealedBudget.responseWire
        task.resume()
        return WebSocketSealedSocket(task: task)
    }
    public func send(text: String) async throws { try await task.send(.string(text)) }
    public func send(data: Data) async throws { try await task.send(.data(data)) }
    public func receive() async throws -> SealedSocketMessage {
        switch try await task.receive() {
        case .string(let text): return .text(text)
        case .data(let data): return .data(data)
        @unknown default: throw SealedChannelFailure.frame
        }
    }
    public func close() { close(failure: nil) }

    private func close(failure: SealedRPCFailure?) { task.cancel(with: .goingAway, reason: nil) }
}

public enum SealedRPCFailure: Error, Equatable, Sendable {
    /// The connection ended; reconnect.
    case closed
    /// The host answered a call with a JSON-RPC error.
    case refused(code: Int, message: String)
    /// The host ended an event subscription whose cursor it cannot replay.
    case resync
    /// The computer did not sign this device in: it refused the session, or
    /// its renewal (an unknown, revoked or other key). Only pairing again helps.
    case signedOut
    /// The computer closed the channel without answering a renewal: it
    /// predates renewal through the relay.
    case renewalUnsupported
}

/// Cupola RPC inside the sealed channel, signed in as this device: the
/// session is the first sealed frame, then `auth.challenge` and `initialize`
/// with the device key's signature. The session cookie never crosses the
/// relay. Ordinary API calls travel as `http.request`; live events arrive as
/// `event` notifications.
public actor SealedRPC {
    public typealias Sign = @Sendable (Data) async throws -> String
    public let origin: String
    private let socket: any SealedSocket
    private var channel: SealedChannel?
    private var nextID = 1
    private var pending: [Int: Waiting] = [:]
    private var reader: Task<Void, Never>?
    private var sending: Task<Void, Never>?
    private var ended = false
    private var subscriptions: [String: AsyncThrowingStream<Data, Error>.Continuation] = [:]
    /// Events that arrived before their subscription's stream was made.
    private var early: [String: [Data]] = [:]
    private var earlyEnded: Set<String> = []
    private var subscribing = 0
    private var eventBatches = false
    private var historyCompression = false
    // Bounded frames, each at most `SealedBudget.eventPlaintext`. Overflow
    // ends only that subscription with resync, so a consumer never advances
    // past an event it did not receive and the connection's other calls
    // continue. A batching host sends at most 20 frames/s (about 12 s of a
    // stalled reader); an older host's per-event frames fit one 256-event
    // replay batch.
    static let eventBufferLimit = 256

    /// How long a call that names no deadline waits for its answer: the
    /// handshake, subscriptions and checkpoints.
    public static let callDeadline: Duration = .seconds(30)

    /// A call waiting for its answer. Whichever comes first, the answer, the
    /// deadline, a cancel or the connection closing, removes it and resumes it
    /// once.
    private struct Waiting {
        let continuation: CheckedContinuation<Data, Error>
        let deadline: Task<Void, Never>
    }

    public init(socket: any SealedSocket, origin: String) {
        self.socket = socket
        self.origin = origin
    }

    public var isOpen: Bool { !ended && channel != nil }
    /// The session this connection signed in with.
    public private(set) var sessionToken: String?

    /// The host's code for a device it does not sign in (HTTP 401).
    static let notSignedInCode = -32001
    /// The host refused the session before closing (`id: null`).
    private var refusedSignIn = false

    /// Runs the handshake and signs in. Throws `SealedChannelFailure.hostProof`
    /// when the computer's key is not the one from pairing, and
    /// `SealedRPCFailure.signedOut` when it does not accept the session.
    public func connect(pin: String, credential: Credential, sign: Sign) async throws {
        try await closingOnCancel {
            do {
                var channel = try await handshake(pin: pin)
                let opening = try JSONSerialization.data(withJSONObject: ["session": credential.sessionToken])
                try await socket.send(data: channel.seal(opening))
                try await signIn(channel, credential: credential, sign: sign)
            } catch {
                close()
                throw failure(error)
            }
        }
    }

    /// A cancelled caller (a route check past its deadline) closes this
    /// connection's socket: a WebSocket receive does not end on cancellation,
    /// so a relay that never sends its welcome would otherwise hold the check.
    private func closingOnCancel<T: Sendable>(_ body: () async throws -> T) async throws -> T {
        let socket = socket
        return try await withTaskCancellationHandler { try await body() } onCancel: { socket.close() }
    }

    private func failure(_ error: Error) -> Error {
        if Task.isCancelled { return CancellationError() }
        return refusedSignIn ? SealedRPCFailure.signedOut : error
    }

    /// Signs this device in again when its session has ended, which the relay
    /// offers no other way to do: the computer issues a one-time challenge
    /// for this device and this address inside the channel, the device key
    /// signs it as for `POST /api/v1/pairing/session`, and the computer
    /// answers with a new session. The connection then signs in with it.
    /// Returns the new session for the caller to save. Throws
    /// `SealedRPCFailure.signedOut` when the computer refuses the device and
    /// `.renewalUnsupported` when it closes without answering.
    public func renew(pin: String, credential: Credential, sign: Sign) async throws -> Credential {
        try await closingOnCancel { try await renewing(pin: pin, credential: credential, sign: sign) }
    }

    private func renewing(pin: String, credential: Credential, sign: Sign) async throws -> Credential {
        do {
            var channel = try await handshake(pin: pin)
            let opening = try JSONSerialization.data(withJSONObject: ["renew": ["deviceId": credential.deviceId]])
            try await socket.send(data: channel.seal(opening))
            struct Issued: Decodable { let challenge: Challenge }
            let issued = try await renewalReply(&channel, unanswered: .renewalUnsupported)
            let challenge = try JSONDecoder().decode(Issued.self, from: issued).challenge
            try challenge.validate(origin: origin, hostID: credential.hostInstallationId, deviceID: credential.deviceId)
            let signature = try await sign(challenge.transcript)
            let answer = try JSONSerialization.data(withJSONObject: ["challengeId": challenge.challengeId, "signature": signature])
            try await socket.send(data: channel.seal(answer))
            struct Renewed: Decodable { let renewed: Credential }
            let reply = try await renewalReply(&channel, unanswered: .closed)
            let renewed = try JSONDecoder().decode(Renewed.self, from: reply).renewed
            guard renewed.deviceId == credential.deviceId, renewed.hostInstallationId == credential.hostInstallationId,
                  !renewed.sessionToken.isEmpty else { throw PairingFailure.wrongHost }
            try await signIn(channel, credential: renewed, sign: sign)
            return renewed
        } catch {
            close()
            throw failure(error)
        }
    }

    /// The two handshake messages: checks the computer's proof, opens the channel.
    private func handshake(pin: String) async throws -> SealedChannel {
        try Task.checkCancellation()
        let initiator = SealedInitiator()
        try await socket.send(text: initiator.hello)
        guard case .text(let welcome) = try await socket.receive() else { throw SealedChannelFailure.handshake }
        try Task.checkCancellation()
        return try initiator.finish(welcome: welcome, pin: pin, origin: origin)
    }

    /// One renewal step's answer, read before any call runs. A JSON-RPC
    /// error is the computer's refusal; a socket that ends instead is `unanswered`.
    private func renewalReply(_ channel: inout SealedChannel, unanswered: SealedRPCFailure) async throws -> Data {
        let message: SealedSocketMessage
        do { message = try await socket.receive() } catch { throw unanswered }
        guard case .data(let frame) = message else { throw SealedChannelFailure.frame }
        let plaintext = try channel.open(frame)
        if let object = try? JSONSerialization.jsonObject(with: plaintext) as? [String: Any],
           let error = object["error"] as? [String: Any] {
            let code = error["code"] as? Int ?? 0
            if code == Self.notSignedInCode { refusedSignIn = true }
            throw SealedRPCFailure.refused(code: code, message: error["message"] as? String ?? "")
        }
        return plaintext
    }

    /// Signs in on an open channel whose opening was sent: the signed
    /// challenge, then `initialize` with the session's CSRF token.
    private func signIn(_ channel: SealedChannel, credential: Credential, sign: Sign) async throws {
        self.channel = channel
        sessionToken = credential.sessionToken
        reader = Task { await self.read() }
        let challenge = try JSONDecoder().decode(Challenge.self, from: try await call("auth.challenge", params: nil))
        try challenge.validate(origin: origin, hostID: credential.hostInstallationId, deviceID: credential.deviceId)
        let signature = try await sign(challenge.transcript)
        let initialize = try JSONSerialization.data(withJSONObject: [
            "protocolVersion": 1, "clientInfo": ["name": "cupola-ios"], "capabilities": ["compactView": true],
            "auth": ["deviceId": credential.deviceId, "csrfToken": credential.csrfToken,
                     "challengeId": challenge.challengeId, "signature": signature]])
        let result = try await call("initialize", params: initialize)
        let object = try JSONSerialization.jsonObject(with: result) as? [String: Any]
        let capabilities = object?["capabilities"] as? [String: Any]
        eventBatches = capabilities?["eventBatches"] as? Bool == true
        historyCompression = (capabilities?["historyCompression"] as? [String])?.contains("deflate-raw") == true
    }

    /// One call; `params` and the result are JSON. Without an answer by
    /// `deadline` it throws `URLError(.timedOut)`, as URLSession would; the
    /// host may still have run it, and the connection stays up.
    public func call(_ method: String, params: Data?, deadline: Duration = SealedRPC.callDeadline) async throws -> Data {
        guard !ended, channel != nil else { throw SealedRPCFailure.closed }
        let id = nextID
        nextID += 1
        var frame = Data(#"{"jsonrpc":"2.0","id":\#(id),"method":"#.utf8)
        frame += try JSONSerialization.data(withJSONObject: method, options: .fragmentsAllowed)
        if let params { frame += Data(#","params":"#.utf8) + params }
        frame += Data("}".utf8)
        return try await withTaskCancellationHandler {
            // Registered on this actor before the call suspends, so a cancel
            // or deadline always finds it.
            try await withCheckedThrowingContinuation { continuation in
                start(id, frame: frame, deadline: deadline, continuation)
            }
        } onCancel: {
            Task { await self.finish(id, .failure(CancellationError())) }
        }
    }

    /// Seals and queues one frame. Frames open only in the order they were
    /// sealed, so sealing and sending share one queue, and a sealed frame is
    /// always sent, even when its call ended first.
    private func start(_ id: Int, frame: Data, deadline: Duration, _ continuation: CheckedContinuation<Data, Error>) {
        // Cancelled before it started: nothing is sealed or sent.
        guard !Task.isCancelled else {
            continuation.resume(throwing: CancellationError())
            return
        }
        guard !ended, channel != nil, let sealed = try? channel!.seal(frame) else {
            continuation.resume(throwing: SealedRPCFailure.closed)
            return
        }
        let timer = Task { [weak self] in
            try? await Task.sleep(for: deadline)
            guard !Task.isCancelled else { return }
            await self?.finish(id, .failure(URLError(.timedOut)))
        }
        pending[id] = Waiting(continuation: continuation, deadline: timer)
        let previous = sending, socket = socket
        sending = Task {
            await previous?.value
            do { try await socket.send(data: sealed) } catch { await self.close() }
        }
    }

    /// Ends one waiting call; a later answer, cancel or deadline for it does nothing.
    private func finish(_ id: Int, _ result: Result<Data, Error>) {
        guard let waiting = pending.removeValue(forKey: id) else { return }
        waiting.deadline.cancel()
        waiting.continuation.resume(with: result)
    }

    public func close() { close(failure: nil) }

    private func close(failure: SealedRPCFailure?) {
        guard !ended else { return }
        ended = true
        reader?.cancel()
        socket.close()
        for stream in subscriptions.values { stream.finish(throwing: failure) }
        subscriptions = [:]; early = [:]; earlyEnded = []
        let waiting = pending
        pending = [:]
        for call in waiting.values {
            call.deadline.cancel()
            call.continuation.resume(throwing: SealedRPCFailure.closed)
        }
    }

    private func read() async {
        while !ended {
            do {
                guard case .data(let frame) = try await socket.receive() else { throw SealedChannelFailure.frame }
                guard var channel else { throw SealedChannelFailure.frame }
                let plaintext = try channel.open(frame)
                self.channel = channel
                try route(plaintext)
            } catch {
                close(failure: error as? SealedRPCFailure)
                return
            }
        }
    }

    private func route(_ message: Data) throws {
        guard let object = try JSONSerialization.jsonObject(with: message) as? [String: Any] else { throw SealedChannelFailure.frame }
        if let method = object["method"] as? String, object["id"] == nil {
            guard let params = object["params"] as? [String: Any], let id = params["subscriptionId"] as? String else { return }
            let event = method == "event" ? params["event"]
                : (method == "events.batch" && eventBatches ? params["events"] : nil)
            if let event {
                let data = try JSONSerialization.data(withJSONObject: event)
                if let stream = subscriptions[id] {
                    if data.count > SealedBudget.eventPlaintext { endSubscription(id) }
                    else if case .dropped = stream.yield(data) { endSubscription(id) }
                } else if subscribing > 0 {
                    if data.count > SealedBudget.eventPlaintext || early.values.reduce(0, { $0 + $1.count }) >= Self.eventBufferLimit {
                        // The pending subscribe throws resync instead of returning a gap.
                        guard earlyEnded.count < Self.eventBufferLimit else { throw SealedRPCFailure.resync }
                        early[id] = nil
                        earlyEnded.insert(id)
                    } else if !earlyEnded.contains(id) {
                        early[id, default: []].append(data)
                    }
                }
            } else if method == "events.ended" {
                // Preserve an end racing the subscribe call's continuation.
                if let stream = subscriptions.removeValue(forKey: id) {
                    stream.finish(throwing: SealedRPCFailure.resync)
                } else if subscribing > 0 {
                    guard earlyEnded.count < Self.eventBufferLimit else { throw SealedRPCFailure.resync }
                    earlyEnded.insert(id)
                }
                early[id] = nil
            }
            return
        }
        guard let id = object["id"] as? Int else {
            // `id: null`: the host refused the session and is closing.
            if (object["error"] as? [String: Any])?["code"] as? Int == Self.notSignedInCode { refusedSignIn = true }
            close()
            return
        }
        guard pending[id] != nil else { return }
        if let error = object["error"] as? [String: Any] {
            finish(id, .failure(SealedRPCFailure.refused(code: error["code"] as? Int ?? 0, message: error["message"] as? String ?? "")))
        } else {
            finish(id, .success(try JSONSerialization.data(withJSONObject: object["result"] ?? [String: Any](), options: .fragmentsAllowed)))
        }
    }

    // MARK: Events

    /// Host events after the cursor, one event or negotiated JSON array per frame, in
    /// order. The stream ends when the subscription or the connection does,
    /// and fails with `SealedRPCFailure.resync` when the cursor can't be replayed.
    public func subscribe(hostEpoch: String, after sequence: UInt64) async throws -> (id: String, events: AsyncThrowingStream<Data, Error>) {
        subscribing += 1
        defer { subscribing -= 1 }
        var params: [String: Any] = [:]
        if !hostEpoch.isEmpty { params["after"] = ["hostEpoch": hostEpoch, "sequence": sequence] }
        // Older hosts deny unknown subscription fields, so negotiation is
        // server-first; initialize itself remains compatible with old hosts.
        if eventBatches { params["batch"] = true }
        struct Subscribed: Decodable { let subscriptionId: String }
        let id = try JSONDecoder().decode(Subscribed.self, from: try await call("events.subscribe", params: JSONSerialization.data(withJSONObject: params))).subscriptionId
        guard !ended, earlyEnded.remove(id) == nil else { throw SealedRPCFailure.resync }
        let (events, stream) = AsyncThrowingStream.makeStream(of: Data.self, throwing: Error.self, bufferingPolicy: .bufferingOldest(Self.eventBufferLimit))
        for event in early.removeValue(forKey: id) ?? [] {
            if case .dropped = stream.yield(event) { throw SealedRPCFailure.resync }
        }
        subscriptions[id] = stream
        return (id, events)
    }

    /// A subscription whose reader fell behind: it resyncs, and the host
    /// stops sending it. The connection and other calls are unaffected.
    private func endSubscription(_ id: String) {
        guard let stream = subscriptions.removeValue(forKey: id) else { return }
        stream.finish(throwing: SealedRPCFailure.resync)
        Task { _ = try? await self.call("events.unsubscribe", params: JSONSerialization.data(withJSONObject: ["subscriptionId": id])) }
    }

    public func unsubscribe(_ id: String) async {
        guard let stream = subscriptions.removeValue(forKey: id) else { return }
        stream.finish()
        _ = try? await call("events.unsubscribe", params: JSONSerialization.data(withJSONObject: ["subscriptionId": id]))
    }

    // MARK: HTTP API inside the channel

    /// Runs `request` as `http.request` and answers like URLSession would.
    /// The whole answer arrives as one frame, so the deadline, by default the
    /// request's `timeoutInterval`, covers the transfer as well as the wait.
    public func http(_ request: URLRequest, deadline: TimeInterval? = nil) async throws -> (Data, HTTPURLResponse) {
        guard let url = request.url, var path = URLComponents(url: url, resolvingAgainstBaseURL: false)?.percentEncodedPath else {
            throw PairingFailure.invalidLink
        }
        if let query = url.query(percentEncoded: true) { path += "?" + query }
        var params: [String: Any] = ["method": request.httpMethod ?? "GET", "path": path,
                                     "compact": request.value(forHTTPHeaderField: PairingAPI.compactViewHeader) != nil]
        if let body = request.httpBody { params["body"] = String(decoding: body, as: UTF8.self) }
        if historyCompression { params["responseCompression"] = "deflate-raw" }
        // The host reads calls up to 256 KiB; a larger message would end the
        // connection. JSON escaping can double a body, so the encoded call is measured.
        let encoded = try JSONSerialization.data(withJSONObject: params)
        guard encoded.count <= SealedBudget.request else {
            throw PairingFailure.hostMessage(413, "Large files send on the home network or Tailscale. Connect there, then try again.")
        }
        struct Answer: Decodable { let status: Int; let contentType: String?; let errorCode: String?; let previewSha256: String?; let body: String; let bodyEncoding: String?; let uncompressedBytes: Int? }
        let answer = try JSONDecoder().decode(Answer.self, from: try await call("http.request", params: encoded,
                                                                                    deadline: .seconds(deadline ?? request.timeoutInterval)))
        guard let encoded = Data(base64Encoded: answer.body) else { throw SealedChannelFailure.frame }
        let data = try Self.decodeBody(encoded, encoding: answer.bodyEncoding, size: answer.uncompressedBytes)
        var headers: [String: String] = [:]
        headers["Content-Type"] = answer.contentType
        headers[PairingAPI.errorCodeHeader] = answer.errorCode
        headers[PairingAPI.previewSha256Header] = answer.previewSha256
        guard let response = HTTPURLResponse(url: url, statusCode: answer.status, httpVersion: "HTTP/1.1", headerFields: headers) else {
            throw SealedChannelFailure.frame
        }
        return (data, response)
    }
    /// Apple Compression's ZLIB algorithm consumes raw RFC 1951 DEFLATE,
    /// not an RFC 1950 wrapper. Bound allocation before decoding any frame.
    static func decodeBody(_ data: Data, encoding: String?, size: Int?) throws -> Data {
        guard let encoding else { return data }
        guard encoding == "deflate-raw", let size, size > 0, size <= SealedBudget.decodedBody, !data.isEmpty else {
            throw SealedChannelFailure.frame
        }
        var result = Data(count: size + 1)
        let decoded = result.withUnsafeMutableBytes { output in
            data.withUnsafeBytes { input in
                compression_decode_buffer(output.bindMemory(to: UInt8.self).baseAddress!, size + 1,
                    input.bindMemory(to: UInt8.self).baseAddress!, data.count, nil, COMPRESSION_ZLIB)
            }
        }
        guard decoded == size else { throw SealedChannelFailure.frame }
        result.count = size
        return result
    }

}
