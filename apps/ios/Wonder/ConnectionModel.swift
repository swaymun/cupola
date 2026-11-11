import Foundation
import CryptoKit
import WonderPairing
#if WONDER_DIAGNOSTICS
import os
#endif

/// Each result belongs to the route snapshot and purpose that requested it.
/// Different checks serialize so two callers cannot move the event socket at once.
@MainActor final class ConnectionRouteChecks {
    struct Request: Equatable {
        let generation: UUID
        let routes: [ConnectionRoute]
        let origin: String
        let hostKeyPin: String?
        let sessionToken: String
        let localNetworkAvailable: Bool
        let earlierOnly: Bool
        let excluding: String?
    }
    private struct Running {
        let id: UUID
        let request: Request
        let task: Task<Bool, Never>
    }
    private var running: Running?
    private var lifetime = UUID()
    /// The last check that found no way in, and until when it answers for
    /// the same snapshot. A full miss already probed every route a narrower
    /// check (one excluding the failed address, or moving back) would.
    private var miss: (request: Request, until: ContinuousClock.Instant)?
    private let now: () -> ContinuousClock.Instant

    init(now: @escaping () -> ContinuousClock.Instant = { ContinuousClock.now }) { self.now = now }

    func cancel() {
        lifetime = UUID()
        running?.task.cancel()
        running = nil
        miss = nil
    }

    /// Every route of `request` missed. Until `forgetMiss()`, which the
    /// recovery owner calls when its next attempt is due, or a minute has
    /// passed, the same snapshot answers without probing again.
    func rememberMiss(_ request: Request) { miss = (request, now().advanced(by: .seconds(60))) }
    func forgetMiss() { miss = nil }

    private func remembersMiss(_ request: Request) -> Bool {
        guard let miss else { return false }
        guard now() < miss.until else { self.miss = nil; return false }
        let missed = miss.request
        return missed.generation == request.generation && missed.routes == request.routes && missed.origin == request.origin
            && missed.hostKeyPin == request.hostKeyPin && missed.sessionToken == request.sessionToken
            && missed.localNetworkAvailable == request.localNetworkAvailable
            && (missed.excluding == nil || missed.excluding == request.excluding)
    }

    func run(request currentRequest: () -> Request?, check: @escaping @MainActor (Request) async -> Bool) async -> Bool {
        let lifetime = lifetime
        while !Task.isCancelled, self.lifetime == lifetime {
            guard let request = currentRequest() else { return false }
            if remembersMiss(request) { return false }
            if let running {
                let result = await running.task.value
                if self.running?.id == running.id { self.running = nil }
                guard !Task.isCancelled, self.lifetime == lifetime else { return false }
                if running.request == request, currentRequest() == request { return result }
                // A quiet miss says nothing about a full check or a different
                // excluded origin. Capture the current snapshot after waiting.
                continue
            }
            let id = UUID()
            let task = Task {
                guard !Task.isCancelled else { return false }
                return await check(request)
            }
            running = Running(id: id, request: request, task: task)
            let result = await task.value
            if running?.id == id { running = nil }
            let reached = !Task.isCancelled && self.lifetime == lifetime && result
            if reached { miss = nil }
            return reached
        }
        return false
    }
}

/// One owner for trying a computer again after it was out of reach. The
/// replay loop records each miss and waits here; every other caller (the
/// route timer, the foreground safety refresh, failed reads) reaches the
/// computer only through the route checks this owner reopens. Waits grow
/// from 3 s to 10 s, hold while the phone has no network at all, and
/// end at once when the network returns or the person tries again.
/// The wait stops growing at 10 s: nothing tells the phone that a sleeping Mac woke
/// or a restarted helper is back (its path never changes), so the wait is also how
/// long a reply that finished meanwhile stays unseen after the computer returns.
@MainActor final class ConnectionRecovery<C: Clock> where C.Duration == Duration {
    static var first: Duration { .seconds(3) }
    static var longest: Duration { .seconds(10) }
    private let clock: C
    private let jitter: () -> Double
    private(set) var failures = 0
    private var notBefore: C.Instant?
    private var woken = false
    private(set) var pathAvailable = true
    private var sleepers: [UUID: Task<Void, Never>] = [:]

    init(clock: C, jitter: @escaping () -> Double = { Double.random(in: 0...1) }) {
        self.clock = clock
        self.jitter = jitter
    }

    /// The wait after `failures` misses in a row: 3 s doubling to 10 s,
    /// each 75–100 % of that so computers lost together are not retried together.
    static func delay(after failures: Int, jitter: Double) -> Duration {
        let doubled = first * (1 << min(max(failures - 1, 0), 5))
        return min(doubled, longest) * (0.75 + 0.25 * min(max(jitter, 0), 1))
    }

    /// The next attempt may run now.
    var isDue: Bool {
        woken || (pathAvailable && (notBefore.map { clock.now >= $0 } ?? true))
    }
    /// A miss is being waited out: nobody but this owner should try again.
    var isWaiting: Bool { failures > 0 && !isDue }

    func failed() {
        failures += 1
        woken = false
        notBefore = clock.now.advanced(by: Self.delay(after: failures, jitter: jitter()))
    }

    func succeeded() {
        failures = 0; notBefore = nil; woken = false
    }

    /// The person asked to try again, or the app came to the front.
    func wake() {
        woken = true
        wakeSleepers()
    }

    /// The app left the foreground: the next foreground starts over.
    func reset() {
        failures = 0; notBefore = nil; woken = false; pathAvailable = true
        wakeSleepers()
    }

    /// The phone's network path. While it is down no attempt runs; when it
    /// returns the next attempt runs at once.
    func setPath(available: Bool) {
        let recovered = available && !pathAvailable
        pathAvailable = available
        if recovered { wake() }
    }

    /// Returns true once the next attempt is due, false when the caller was cancelled.
    func waitUntilDue() async -> Bool {
        while !Task.isCancelled {
            let wait: Duration
            if woken { return true }
            // No network at all: look again every `longest` (10 s), not once a minute, so a
            // returning network path or woken Mac is noticed within that.
            if !pathAvailable { wait = Self.longest }
            else if let notBefore, clock.now < notBefore { wait = clock.now.duration(to: notBefore) }
            else { return true }
            let id = UUID(), clock = clock
            let sleeper = Task { _ = try? await clock.sleep(for: wait) }
            sleepers[id] = sleeper
            await withTaskCancellationHandler { await sleeper.value } onCancel: { sleeper.cancel() }
            sleepers[id] = nil
        }
        return false
    }

    private func wakeSleepers() {
        for sleeper in sleepers.values { sleeper.cancel() }
    }
}

/// The computer closed the live stream itself: it answered, so the route works.
struct LiveStreamEnded: Error { let code: Int }
/// The connection check found no way in; the recovery owner waits before the next.
struct ConnectionRecoveryMiss: Error {}

/// What a failed request or live stream says about its route. Only an
/// address that stopped answering is a reason to leave it for another way
/// in; an ended session, a resync or a refusal reconnects on the same route.
enum ConnectionFailure: Equatable, Sendable {
    /// The caller stopped; nothing to recover.
    case cancelled
    /// The session ended or must be renewed; the next check renews it.
    case access
    /// The cursor cannot be replayed; read the computer's state again.
    case resync
    /// The computer closed the stream itself (a restart, an expired session).
    case hostEnded
    /// The address stopped answering.
    case route
    case other

    /// `sessionLapsed`: the session's expiry has passed, so the computer
    /// closing the stream is expected. `cancelled`: the caller's task was
    /// cancelled; otherwise a URLSession "cancelled" is a refused
    /// certificate, which is still about this address.
    static func classify(_ error: Error, sessionLapsed: Bool = false, cancelled: Bool = Task.isCancelled) -> ConnectionFailure {
        if error is CancellationError { return .cancelled }
        if let failure = error as? ReadFailure { return failure == .resync ? .resync : .other }
        if let failure = error as? SealedRPCFailure {
            switch failure {
            case .resync: return .resync
            case .refused: return .other
            case .signedOut, .renewalUnsupported: return .access
            case .closed: return sessionLapsed ? .access : .route
            }
        }
        if error is LiveStreamEnded { return .hostEnded }
        if let failure = error as? PairingFailure {
            if case .response(let code) = failure, code == 401 || code == 403 { return .access }
            return .other
        }
        if error is SigningIdentityFailure { return .access }
        if let failure = error as? URLError {
            switch failure.code {
            case .cancelled: if cancelled { return .cancelled }
            case .userAuthenticationRequired, .userCancelledAuthentication: return .access
            case .badURL, .unsupportedURL, .cannotDecodeRawData, .cannotDecodeContentData, .cannotParseResponse,
                 .dataLengthExceedsMaximum:
                return .other
            default: break
            }
            return sessionLapsed ? .access : .route
        }
        // A WebSocket that drops reports the socket's own error.
        let failure = error as NSError
        if failure.domain == NSPOSIXErrorDomain, let code = POSIXErrorCode(rawValue: Int32(truncatingIfNeeded: failure.code)) {
            switch code {
            case .ECONNRESET, .ENOTCONN, .ETIMEDOUT, .ECONNREFUSED, .EHOSTUNREACH, .EHOSTDOWN,
                 .ENETDOWN, .ENETUNREACH, .ENETRESET, .ECONNABORTED, .EPIPE:
                return sessionLapsed ? .access : .route
            default: break
            }
        }
        return .other
    }
}

/// A live stream that stays quiet is asked whether the computer is still
/// there: after 25 s without a frame it is checked (a WebSocket Ping, or the
/// relay's checkpoint), and a check unanswered within 10 s closes it, so a
/// sleeping computer stops looking connected and recovery takes over.
enum ConnectionLiveness {
    static let quiet: Duration = .seconds(25)
    static let deadline: Duration = .seconds(10)

    static func receive<C: Clock>(clock: C, receive: @escaping @Sendable () async throws -> Data,
                                  confirmAlive: @escaping @Sendable () async throws -> Void,
                                  close: @escaping @Sendable () async -> Void) async throws -> Data where C.Duration == Duration {
        try await withThrowingTaskGroup(of: Data?.self) { group in
            group.addTask { try await receive() }
            group.addTask {
                while true {
                    try await clock.sleep(for: quiet)
                    let answered = try await withThrowingTaskGroup(of: Bool.self) { check in
                        check.addTask { try await confirmAlive(); return true }
                        check.addTask { try await clock.sleep(for: deadline); return false }
                        defer { check.cancelAll() }
                        do { return try await check.next() ?? false }
                        catch where Task.isCancelled { throw CancellationError() }
                        catch { return false }
                    }
                    guard answered else { return nil }
                }
            }
            defer { group.cancelAll() }
            guard let frame = try await group.next() ?? nil else {
                // Close before draining the receive, which may not end on cancellation.
                await close()
                throw URLError(.timedOut)
            }
            return frame
        }
    }
}

/// Host proof is bound to both the candidate origin and a fresh nonce. No
/// session cookie is sent to a newly learned address until this verifies.
typealias ConnectionRouteProof = HostProof

/// What this device could not keep for a computer, in words the owner can act on.
enum DeviceStorageFailure: Equatable {
    /// Chats from the computer could not be written to this device.
    case chatsNotSaved
    /// The chats saved earlier could not be read; the computer's copy refills them.
    case savedChatsUnreadable
    /// Where the owner stopped reading could not be kept.
    case positionNotSaved

    var message: String {
        switch self {
        case .chatsNotSaved: "Chats couldn’t be saved on this device. Free up some storage, then try again."
        case .savedChatsUnreadable: "Chats saved on this device couldn’t be read. Try again to reload them from your computer."
        case .positionNotSaved: "Your place in chats couldn’t be saved on this device. Free up some storage, then try again."
        }
    }
}

/// T3 places a newly learned address by its default speed class. Existing
/// routes keep the person's order; I1 still owns provenance and removal rules.
enum ConnectionRouteLearning {
    static func merge(_ advertised: [ConnectionRoute], into saved: SavedConnection, from source: String) throws -> SavedConnection {
        let existing = Set(saved.routes.map(\.origin))
        var merged = saved
        merged.mergeLearnedRoutes(advertised, from: source)
        var ordered = merged.routes.filter { existing.contains($0.origin) }
        func rank(_ kind: ConnectionRoute.Kind) -> Int {
            switch kind { case .home: 0; case .tailscale: 1; case .relay: 2 }
        }
        for route in merged.routes where !existing.contains(route.origin) {
            let position = ordered.firstIndex { rank($0.kind) > rank(route.kind) } ?? ordered.endIndex
            ordered.insert(route, at: position)
        }
        return try SavedConnection(routes: ordered, origin: merged.origin, credential: merged.credential,
            hostName: merged.hostName, requiresPairing: merged.requiresPairing, storageDeviceId: merged.storageDeviceId,
            hostKeyPin: merged.hostKeyPin)
    }
}

/// URLSession's Pong callback is not an async cancellation boundary. The
/// stream lets a timeout/background cancellation release the waiter even if
/// Foundation never calls back; a late callback is harmless after finish.
@MainActor enum ConnectionRoutePong {
    static func wait(send: (@escaping @Sendable (Error?) -> Void) -> Void) async throws {
        try Task.checkCancellation()
        let replies = AsyncThrowingStream<Void, Error> { continuation in
            send { error in
                if let error { continuation.finish(throwing: error) }
                else { continuation.yield(()); continuation.finish() }
            }
        }
        var iterator = replies.makeAsyncIterator()
        guard try await iterator.next() != nil else { throw CancellationError() }
    }
}

/// Open and validate a replacement before committing it. The current stream
/// keeps advancing during open; its durable cursor is read at subscription time.
/// A failed/expired move closes only the candidate, leaving the old stream alive.
/// The live event stream: the `/api/v1/events` socket at home and on
/// Tailscale, `events.subscribe` inside the sealed connection on the relay.
protocol LiveEventStream: AnyObject, Sendable {
    /// The next frame: one replay event's JSON, or a batch of them.
    func receiveFrame() async throws -> Data
    func acknowledge(hostEpoch: String, sequence: UInt64) async throws
    /// Returns once the computer has answered on this stream.
    func confirmAlive() async throws
    func close()
    /// The computer stopped answering: end the whole transport, so the next
    /// connection contacts the computer instead of reusing a dead one.
    func abandon() async
}

extension LiveEventStream {
    /// A stream that owns its transport ends it by closing.
    func abandon() async { close() }
}

final class SocketEventStream: LiveEventStream {
    let task: URLSessionWebSocketTask
    init(task: URLSessionWebSocketTask) { self.task = task }
    func receiveFrame() async throws -> Data {
        let message: URLSessionWebSocketTask.Message
        do { message = try await task.receive() }
        catch where task.closeCode != .invalid { throw LiveStreamEnded(code: task.closeCode.rawValue) }
        switch message {
        case .data(let data): return data
        case .string(let value): return Data(value.utf8)
        @unknown default: throw ReadFailure.resync
        }
    }
    func acknowledge(hostEpoch: String, sequence: UInt64) async throws {
        struct Ack: Encodable { let type = "ack"; let hostEpoch: String; let sequence: UInt64 }
        let data = try JSONEncoder().encode(Ack(hostEpoch: hostEpoch, sequence: sequence))
        try await task.send(.string(String(decoding: data, as: UTF8.self)))
    }
    func confirmAlive() async throws {
        let task = task
        try await ConnectionRoutePong.wait { task.sendPing(pongReceiveHandler: $0) }
    }
    func close() { task.cancel(with: .goingAway, reason: nil) }
}

#if WONDER_DIAGNOSTICS
/// A test stream that stays open and delivers no events.
final class DiagnosticIdleEventStream: LiveEventStream {
    func receiveFrame() async throws -> Data {
        try await Task.sleep(for: .seconds(86_400))
        throw CancellationError()
    }
    func acknowledge(hostEpoch: String, sequence: UInt64) async throws {}
    func confirmAlive() async throws {}
    func close() {}
}
#endif

/// One subscription on the relay's sealed connection. The connection stays
/// open for the phone's other calls when the stream closes.
final class SealedEventStream: LiveEventStream, @unchecked Sendable {
    let client: SealedRPC
    let subscription: String
    private let lock = NSLock()
    private var iterator: AsyncThrowingStream<Data, Error>.Iterator
    init(client: SealedRPC, subscription: String, events: AsyncThrowingStream<Data, Error>) {
        self.client = client; self.subscription = subscription; iterator = events.makeAsyncIterator()
    }
    func receiveFrame() async throws -> Data {
        // One reader at a time: the replay loop.
        var next = lock.withLock { iterator }
        defer { lock.withLock { iterator = next } }
        // The subscription or connection ended: reconnect and resume from the cursor.
        do {
            guard let event = try await next.next() else { throw URLError(.networkConnectionLost) }
            return event
        } catch SealedRPCFailure.resync { throw ReadFailure.resync }
    }
    /// The host's subscription has no acknowledgements; the cursor resumes it.
    func acknowledge(hostEpoch: String, sequence: UInt64) async throws {}
    func confirmAlive() async throws { _ = try await client.call("events.checkpoint", params: nil) }
    func close() {
        let client = client, subscription = subscription
        Task { await client.unsubscribe(subscription) }
    }
    /// Unsubscribing alone would leave the sealed connection open and cached,
    /// and the next connection would reuse it without reaching the computer.
    func abandon() async { await client.close() }
}

@MainActor enum ConnectionRouteHandoff {
    static func move<C: Clock, Socket: Sendable>(
        clock: C,
        open: () async throws -> Socket,
        firstFrame: @escaping @MainActor @Sendable (Socket) async throws -> Data?,
        commit: (Socket, Data?) throws -> Void,
        close: @escaping @MainActor @Sendable (Socket) -> Void
    ) async throws where C.Duration == Duration {
        let candidate = try await open()
        var closed = false
        do {
            let frame = try await withThrowingTaskGroup(of: Data?.self) { group in
                group.addTask { try await firstFrame(candidate) }
                group.addTask { try await clock.sleep(for: .seconds(6)); throw URLError(.timedOut) }
                defer { group.cancelAll() }
                do { return try await group.next()! }
                catch {
                    // Close before draining a pending WebSocket receive/Pong.
                    close(candidate); closed = true
                    throw error
                }
            }
            try Task.checkCancellation()
            if let frame { _ = try ReplayEvent.frame(frame) }
            try commit(candidate, frame)
        } catch {
            if !closed { close(candidate) }
            throw error
        }
    }
}

/// When a session is renewed. The computer ends each session an hour after
/// it starts; renewal begins five minutes before that, so a late wake-up
/// still renews inside the session instead of meeting an ended one.
enum SessionRenewal {
    static let marginMs: UInt64 = 5 * 60 * 1000
    static var nowMs: UInt64 { UInt64(Date().timeIntervalSince1970 * 1000) }
    static func due(expiresAtMs: UInt64?, nowMs: UInt64) -> Bool {
        expiresAtMs.map { $0 <= nowMs &+ marginMs } ?? false
    }
    /// How long until renewal is due, at most an hour (the timer then looks
    /// again); nil for a session that never expires.
    static func delay(expiresAtMs: UInt64?, nowMs: UInt64) -> Duration? {
        guard let expiresAtMs else { return nil }
        let dueAt = expiresAtMs > marginMs ? expiresAtMs - marginMs : 0
        guard dueAt > nowMs else { return .zero }
        return .milliseconds(Int64(min(dueAt - nowMs, 60 * 60 * 1000)))
    }
}

#if os(iOS)
import SwiftUI
import Combine
import PhotosUI
import ImageIO
import UniformTypeIdentifiers
import Network
import Security

/// Bonjour reports the documented policy-denied signal independently of a
/// working backup route. No discovered address is trusted or saved by this monitor.
@MainActor final class HomeNetworkAccess {
    private var browser: NWBrowser?
    private var generation = UUID()
    nonisolated static func denied(_ state: NWBrowser.State) -> Bool? {
        switch state {
        case .waiting(.dns(-65570)): true // kDNSServiceErr_PolicyDenied (Apple TN3179)
        case .ready: false
        default: nil // Offline, timeout and discovery errors do not prove denial.
        }
    }
    func start(changed: @escaping @MainActor @Sendable (Bool) -> Void) {
        guard browser == nil else { return }
        let id = UUID(); generation = id
        let browser = NWBrowser(for: .bonjour(type: "_wonder._tcp", domain: nil), using: .tcp)
        self.browser = browser
        browser.stateUpdateHandler = { [weak self] state in
            guard let denied = Self.denied(state) else { return }
            Task { @MainActor in
                guard let self, self.generation == id else { return }
                changed(denied)
            }
        }
        browser.start(queue: DispatchQueue(label: "wonder.home-network-access"))
    }
    func stop() {
        generation = UUID()
        browser?.cancel(); browser = nil
    }
    deinit { browser?.cancel() }
}

#if WONDER_DIAGNOSTICS
/// The routes-switch scenario uses two synthetic HTTPS addresses and genuine
/// nonce-bound proofs. Its renewal challenges follow the requested address and
/// verify the synthetic device signature. Normal paired connections never use it.
private final class DiagnosticRouteSwitchProtocol: URLProtocol, @unchecked Sendable {
    private final class State: @unchecked Sendable {
        let lock = NSLock()
        let key = P256.Signing.PrivateKey()
        let deviceKey = P256.Signing.PrivateKey()
        var challenges: [String: Challenge] = [:]
        var homeChecks = 0
    }
    private static let state = State()
    static let home = ConnectionRoute(kind: .home, origin: "https://synthetic.invalid:3778")
    static var pin: String { Data(SHA256.hash(data: state.key.publicKey.derRepresentation)).base64URL }
    static var homeChecks: Int { state.lock.withLock { state.homeChecks } }
    static func enabled(_ saved: SavedConnection) -> Bool {
        ProcessInfo.processInfo.arguments.contains("-diagnostics-routes-switch")
            && saved.credential.hostInstallationId == DiagnosticSubagentFixture.hostID
    }
    static var signingIdentity: SigningIdentity {
        let key = state.deviceKey
        return SigningIdentity(read: { key.rawRepresentation }, save: { _ in },
            restore: { _ in EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation,
                sign: { try key.signature(for: $0) }) }, create: { throw SigningIdentityFailure.missing })
    }
    private var work: DispatchWorkItem?

    private func requestBody() -> Data {
        if let body = request.httpBody { return body }
        guard let stream = request.httpBodyStream else { return Data() }
        stream.open()
        defer { stream.close() }
        var body = Data(), buffer = [UInt8](repeating: 0, count: 4096)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            guard count > 0 else { break }
            body.append(contentsOf: buffer.prefix(count))
        }
        return body
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() { work?.cancel() }
    override func startLoading() {
        guard let url = request.url, url.host == "synthetic.invalid",
              request.value(forHTTPHeaderField: "Cookie") == nil,
              request.value(forHTTPHeaderField: "x-wonder-csrf") == nil,
              ["/readyz", "/api/v1/host/proof", "/api/v1/pairing/session/refresh-challenge", "/api/v1/pairing/session"].contains(url.path) else {
            client?.urlProtocol(self, didFailWithError: URLError(.badServerResponse)); return
        }
        let home = url.port == 3778
        if home, url.path == "/readyz" { Self.state.lock.withLock { Self.state.homeChecks += 1 } }
        let blocked = home && url.path == "/readyz" && DiagnosticSubagentFixture.signalled("routes-block-home")
            && !DiagnosticSubagentFixture.signalled("routes-restore-home")
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.work?.isCancelled == false else { return }
            do {
                if blocked { throw URLError(.cannotConnectToHost) }
                var body = Data("{}".utf8)
                if url.path == "/api/v1/pairing/session/refresh-challenge" {
                    guard let origin = self.request.value(forHTTPHeaderField: "Origin"),
                          let input = try JSONSerialization.jsonObject(with: self.requestBody()) as? [String: String],
                          input["deviceId"] == "diagnostic-device" else { throw URLError(.badServerResponse) }
                    let now = UInt64(Date().timeIntervalSince1970 * 1000)
                    body = try JSONSerialization.data(withJSONObject: ["challengeId": UUID().uuidString, "deviceId": "diagnostic-device",
                        "nonce": UUID().uuidString, "origin": origin, "hostInstallationId": DiagnosticSubagentFixture.hostID,
                        "offerId": "", "issuedAtMs": now, "expiresAtMs": now + 60_000])
                    let challenge = try JSONDecoder().decode(Challenge.self, from: body)
                    Self.state.lock.withLock {
                        if Self.state.challenges.count >= 32 { Self.state.challenges.removeAll() }
                        Self.state.challenges[challenge.challengeId] = challenge
                    }
                }
                if url.path == "/api/v1/pairing/session" {
                    guard let origin = self.request.value(forHTTPHeaderField: "Origin"),
                          let input = try JSONSerialization.jsonObject(with: self.requestBody()) as? [String: String],
                          let id = input["challengeId"], let encoded = input["signature"],
                          let challenge = Self.state.lock.withLock({ Self.state.challenges.removeValue(forKey: id) }) else {
                        throw URLError(.badServerResponse)
                    }
                    try challenge.validate(origin: origin, hostID: DiagnosticSubagentFixture.hostID, deviceID: "diagnostic-device")
                    let padded = encoded.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
                    guard let raw = Data(base64Encoded: padded + String(repeating: "=", count: (4 - padded.count % 4) % 4)),
                          Self.state.deviceKey.publicKey.isValidSignature(try P256.Signing.ECDSASignature(rawRepresentation: raw),
                            for: challenge.transcript) else { throw PairingFailure.wrongHost }
                    body = try JSONEncoder().encode(DiagnosticSubagentFixture.savedConnection().credential)
                }
                if url.path == "/api/v1/host/proof" {
                    guard let origin = self.request.value(forHTTPHeaderField: "Origin"),
                          let nonce = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first(where: { $0.name == "nonce" })?.value else {
                        throw URLError(.badServerResponse)
                    }
                    let key = Self.state.key
                    let signature = try key.signature(for: Data("wonder-host-proof-v1\n\(origin)\n\(nonce)".utf8))
                    body = try JSONSerialization.data(withJSONObject: ["hostKey": key.publicKey.derRepresentation.base64URL,
                        "signature": signature.rawRepresentation.base64URL])
                }
                let response = HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: ["Content-Type": "application/json"])!
                self.client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
                self.client?.urlProtocol(self, didLoad: body)
                self.client?.urlProtocolDidFinishLoading(self)
            } catch { self.client?.urlProtocol(self, didFailWithError: error) }
        }
        self.work = work
        // Hold a failed earlier-route check long enough for a foreground check
        // to join it deterministically; scenario signals hold the visible stages.
        DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + (blocked ? 0.4 : home ? 0.02 : 0.08), execute: work)
    }
}
#endif

/// UIKit invokes expiration on the main actor. End the grant before returning
/// from that callback; the cache's atomic write may finish independently.
@MainActor final class CacheBackgroundTask {
    private var identifier: UIBackgroundTaskIdentifier = .invalid
    private var finished = false
    private let end: (UIBackgroundTaskIdentifier) -> Void

    init(begin: (@escaping @MainActor @Sendable () -> Void) -> UIBackgroundTaskIdentifier = {
        UIApplication.shared.beginBackgroundTask(withName: "Save chats", expirationHandler: $0)
    }, end: @escaping (UIBackgroundTaskIdentifier) -> Void = { UIApplication.shared.endBackgroundTask($0) }) {
        self.end = end
        let identifier = begin { [weak self] in self?.finish() }
        // Also handle a grant expiring inside an injected begin operation.
        if finished {
            if identifier != .invalid { end(identifier) }
        } else { self.identifier = identifier }
    }

    /// Normal completion, foregrounding and ownership cancellation share this
    /// idempotent endpoint. Invalid/expired grants are never ended twice.
    func finish() {
        guard !finished else { return }
        finished = true
        let identifier = identifier
        self.identifier = .invalid
        if identifier != .invalid { end(identifier) }
    }
}

struct CodexUsageWindow: Decodable, Identifiable, Sendable {
    let id: String
    let label: String
    let usedPercent: Double
    let remainingPercent: Double
    let windowDurationMins: UInt64?
    let resetsAt: UInt64?

    /// The reset time, read as milliseconds when a provider reports them
    /// (the host normalizes the same way). Every gate and label uses this.
    var resetDate: Date? { resetsAt.flatMap(Self.normalizedReset) }

    static func normalizedReset(_ raw: UInt64) -> Date? {
        guard raw > 0 else { return nil }
        return Date(timeIntervalSince1970: raw > 100_000_000_000 ? Double(raw) / 1000 : Double(raw))
    }

    /// Whether the window's reported reset time has passed.
    func hasReset(now: Date = Date()) -> Bool { resetDate.map { $0 <= now } ?? false }

    var roundedRemainingPercent: Int {
        Int(min(max(remainingPercent, 0), 100).rounded())
    }
}

struct CodexUsageResponse: Decodable, Sendable {
    var agentFamily: String? = nil
    let checkedAtMs: UInt64
    let windows: [CodexUsageWindow]
    var additionalUsageAvailable: Bool? = nil
    /// The plan the provider names ("pro", "max"); absent from older Macs.
    var planType: String? = nil
    /// Names the provider account, the same on every computer signed in to it;
    /// absent from older Macs and when the provider names no account.
    var accountKey: String? = nil
    /// The account's email, for the owner to recognise it.
    var accountLabel: String? = nil
}

struct CodexUsageCacheEntry: Sendable {
    let response: CodexUsageResponse
    let fetchedAt: Date

    func exhaustedWindow(model: String, now: Date = Date()) -> CodexUsageWindow? {
        guard now.timeIntervalSince(fetchedAt) < 300, response.additionalUsageAvailable != true else { return nil }
        let name = model.lowercased()
        return response.windows.first { window in
            guard window.usedPercent >= 100, window.remainingPercent <= 0 else { return false }
            if window.hasReset(now: now) { return false }
            switch window.id {
            case "seven_day_sonnet": return name.contains("sonnet")
            case "seven_day_opus": return name.contains("opus")
            default: return true
            }
        }
    }
}

/// One usage-limit row in the context panel: "Session limit", "Weekly · all
/// models", "Weekly · Fable", in that order, with when it resets.
struct PlanLimitRow: Equatable, Identifiable {
    let id: String
    let title: String
    let used: Int
    let resetsAt: Date?
    /// Nil once the reported reset has passed: the value is stale, not "now".
    var resetText: String? { resetsAt.flatMap { $0 > Date() ? Self.resetText($0) : nil } }

    static func rows(_ response: CodexUsageResponse?) -> [PlanLimitRow] {
        guard let response else { return [] }
        func row(_ window: CodexUsageWindow, _ title: String) -> PlanLimitRow {
            PlanLimitRow(id: window.id, title: title, used: Int(min(max(window.usedPercent, 0), 100).rounded()),
                         resetsAt: window.resetDate)
        }
        let claude = response.agentFamily == AgentFamily.claude.rawValue // provider identity: Claude window names
            || response.windows.contains { $0.id == "five_hour" || $0.id.hasPrefix("seven_day") }
        if claude {
            var rows: [PlanLimitRow] = []
            if let window = response.windows.first(where: { $0.id == "five_hour" }) { rows.append(row(window, "Session limit")) }
            if let window = response.windows.first(where: { $0.id == "seven_day" }) { rows.append(row(window, "Weekly · all models")) }
            for window in response.windows where window.id.hasPrefix("seven_day_") {
                rows.append(row(window, window.label.hasPrefix("Weekly · ") ? window.label
                    : window.id == "seven_day_opus" ? "Weekly · Opus" : window.id == "seven_day_sonnet" ? "Weekly · Sonnet" : window.label))
            }
            return rows
        }
        // Codex reports only the windows the plan has, labeled by length.
        var seen = Set<String>()
        return response.windows
            .sorted { ($0.windowDurationMins ?? .max) < ($1.windowDurationMins ?? .max) }
            .filter { seen.insert($0.id).inserted }
            .map { window in
                let minutes = window.windowDurationMins
                return row(window, minutes == 300 ? "Session limit" : Self.windowName(minutes: minutes, fallback: window.label))
            }
    }

    /// A window's name from its length: "5 hours", "Daily", "Weekly", "30 days".
    static func windowName(minutes: UInt64?, fallback: String) -> String {
        guard let minutes, minutes > 0 else { return fallback.isEmpty ? "Usage" : fallback }
        switch minutes {
        case 60: return "Hourly"
        case 1_440: return "Daily"
        case 10_080: return "Weekly"
        case let m where m % 1_440 == 0: return "\(m / 1_440) days"
        case let m where m % 60 == 0: return "\(m / 60) hours"
        default: return "\(minutes) minutes"
        }
    }

    /// "Resets in 1 hr 39 min" within a day, else "Resets Sun 1:00 PM".
    static func resetText(_ date: Date, now: Date = Date(), calendar: Calendar = .current) -> String {
        let seconds = date.timeIntervalSince(now)
        if seconds <= 60 { return "Resets in 1 min" }
        if seconds < 24 * 3600 {
            let minutes = Int((seconds / 60).rounded(.up))
            let hours = minutes / 60, rest = minutes % 60
            if hours == 0 { return "Resets in \(rest) min" }
            return rest == 0 ? "Resets in \(hours) hr" : "Resets in \(hours) hr \(rest) min"
        }
        let style = Date.FormatStyle(calendar: calendar).weekday(.abbreviated).hour().minute()
        return "Resets " + date.formatted(style)
    }
}

/// A provider's plan name as people say it: "pro" → "Pro", "max" → "Max".
enum PlanName {
    static func title(_ raw: String) -> String {
        raw.replacingOccurrences(of: "_", with: " ").split(separator: " ").map { $0.capitalized }.joined(separator: " ")
    }
}

struct ConversationGoal: Decodable, Equatable, Sendable {
    let objective: String
    let status: String
    let createdAt: Date?
    let tokenBudget: Int?
    let tokensUsed: Int?
    let timeBudgetSeconds: Int?
    let timeUsedSeconds: Int?

    private enum CodingKeys: String, CodingKey {
        case objective, status, createdAt, tokenBudget, tokensUsed, timeBudgetSeconds, timeUsedSeconds
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        objective = try values.decode(String.self, forKey: .objective)
        status = try values.decode(String.self, forKey: .status)
        let stamp = try values.decodeIfPresent(Double.self, forKey: .createdAt)
        createdAt = stamp.map { Date(timeIntervalSince1970: $0 > 1_000_000_000_000 ? $0 / 1000 : $0) }
        tokenBudget = try values.decodeIfPresent(Int.self, forKey: .tokenBudget)
        tokensUsed = try values.decodeIfPresent(Int.self, forKey: .tokensUsed)
        timeBudgetSeconds = try values.decodeIfPresent(Int.self, forKey: .timeBudgetSeconds)
        timeUsedSeconds = try values.decodeIfPresent(Int.self, forKey: .timeUsedSeconds)
    }
}

private struct ConversationGoalResponse: Decodable, Sendable { let goal: ConversationGoal? }
private struct EmptyGoalResponse: Decodable, Sendable {}
private struct ChatNotificationsResponse: Decodable, Sendable { let notificationLevel: ChatNotificationLevel }

/// The Mac's merge readiness, or its plain-language refusal.
private enum MergeStateReply: Decodable, Sendable {
    case state(PullRequestMergeState)
    case refused(String)
    private struct Refusal: Decodable { let message: String }
    init(from decoder: Decoder) throws {
        if let state = try? PullRequestMergeState(from: decoder) { self = .state(state); return }
        self = .refused(try Refusal(from: decoder).message)
    }
}

/// Wake-ups the Mac sends an open Project chat when another app writes it.
/// A wake recorded while nothing waits stays pending (one per chat, however
/// many arrive) until taken, so a wake between checks or before the first wait
/// is never lost; a waiting screen resumes as the wake is recorded instead of
/// polling for it.
@MainActor final class NativeWakeSignal {
    private var pending: Set<String> = []
    private var waiters: [String: [UUID: CheckedContinuation<Bool, Never>]] = [:]

    /// Screens waiting now, across chats.
    var waitingCount: Int { waiters.values.reduce(0) { $0 + $1.count } }

    /// Resumes the chat's waiting screens, or keeps the wake for the next take.
    func record(_ chat: String) {
        guard let waiting = waiters.removeValue(forKey: chat), !waiting.isEmpty else {
            pending.insert(chat)
            return
        }
        for continuation in waiting.values { continuation.resume(returning: true) }
    }

    /// Whether the chat was woken since the last take.
    func take(_ chat: String) -> Bool { pending.remove(chat) != nil }

    /// Drops wakes from a connection that ended, so they never count for the
    /// next one. Waiting screens keep waiting for a new wake or their deadline.
    func discardPending() { pending.removeAll() }

    /// A pending wake returns at once; otherwise this waits for the next one
    /// (true), the deadline or cancellation (false).
    func wait(_ chat: String, until deadline: ContinuousClock.Instant) async -> Bool {
        if take(chat) { return true }
        guard !Task.isCancelled, ContinuousClock.now < deadline else { return false }
        let id = UUID()
        let timer = Task { [weak self] in
            try? await Task.sleep(until: deadline, clock: .continuous)
            self?.finish(chat, id)
        }
        defer { timer.cancel() }
        return await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                waiters[chat, default: [:]][id] = continuation
            }
        } onCancel: {
            Task { @MainActor [weak self] in self?.finish(chat, id) }
        }
    }

    /// Ends one wait without a wake; a no-op once a wake resumed it.
    private func finish(_ chat: String, _ id: UUID) {
        guard let continuation = waiters[chat]?.removeValue(forKey: id) else { return }
        if waiters[chat]?.isEmpty == true { waiters[chat] = nil }
        continuation.resume(returning: false)
    }
}

@MainActor final class ConnectionModel: ObservableObject {
    let computerPictures = ComputerViewSnapshots()
    @Published var connection: SavedConnection? {
        didSet {
            homeNetworkTrust.update(connection)
            api.markRelay(Set(connection?.routes.filter { $0.kind == .relay }.map(\.origin) ?? []))
            if oldValue?.credential.hostInstallationId != connection?.credential.hostInstallationId ||
                oldValue?.credential.deviceId != connection?.credential.deviceId {
                stopRouteMonitoring()
                homeNetworkDenied = false
                hostPlatform = nil
                resetNativeReadings(); nativeActivityRequests = [:]; nativeHistoryReadRevisions = [:]; nativeHistoryCompletedReads = [:]; conversationCompletedReads = [:]; settleRepairs = [:]; settleRepairsLoaded = []
                // A refresh still running for the old pairing no longer clears its own marker (its scope check fails).
                nativeHistoryRefreshing = []; nativeHistoryFailures = []; pendingCatchUp = []
                cancelApprovalSettings()
                connectedAppsCache = [:]
                usageCache = [:]
                goals = [:]; goalErrors = [:]; goalMutationTokens = [:]
                resetImagePreviews()
                computerPictures.invalidate()
                dictation.connectionChanged()
                endVoice()
                cameraContextID = UUID()
            }
            if routeMonitor != nil { updateHomeNetworkAccess() }
            // A replaced session has its own expiry.
            if oldValue?.credential.sessionToken != connection?.credential.sessionToken ||
                oldValue?.credential.expiresAtMs != connection?.credential.expiresAtMs { scheduleRenewal() }
            noteListChange()
        }
    }
    var connectedAppsCache: [String: ConnectedAppCacheEntry] = [:]
    /// Account usage per provider, then per assignment scope. Only providers
    /// whose record has `usageLimits` are fetched or shown.
    @Published var usageCache: [AgentFamily: [String: CodexUsageCacheEntry]] = [:]
    let imagePreviews = ToolImagePreviews()
    private(set) var imagePreviewScope = UUID()
    private func resetImagePreviews() {
        imagePreviewScope = UUID()
        imagePreviews.invalidate()
    }
    lazy var dictation = DictationController(model: self)
    /// Owns one voice session for this connection, pinned to the chat that
    /// started it. Conversation visibility and navigation never end it.
    lazy var voice = VoiceController(model: self)
    /// Pairing changed, access ended or the Mac was forgotten: voice acts for
    /// this pairing, so its session ends with it.
    private func endVoice() {
        if voice.isActive { voice.end() }
    }
    /// Owner-selected projects on this Mac, observed directly by the sidebar.
    lazy var projects = ProjectLibrary(model: self)
    func isProject(_ chat: ChatSummary) -> Bool { projectDetail(chat) != nil }
    /// Native provider history being re-read; Send waits so a new turn never
    /// follows a stale view of work done on the Mac.
    @Published private(set) var nativeHistoryRefreshing: Set<String> = []
    /// Chats whose recovery catch-up found an import in flight; the import's cleanup rereads them.
    private var pendingCatchUp: Set<String> = []
    @Published private(set) var nativeHistoryFailures: Set<String> = []
    /// `revision`: the provider's history revision read before this re-read;
    /// a completed re-read makes the saved copy current at it.
    func reloadNativeHistory(_ chat: ChatSummary, revision: String? = nil) async {
        let scope = assignmentScope
        let completed = nativeHistoryCompletedReads[chat.id]
        try? await refreshCoordinator.read(.nativeHistory(chat.id)) { [self] in await performNativeHistoryReload(chat) }
        // Joining an event-driven import also reads the opening caller's
        // revision. Only a completed import plus canonical GET proves it read;
        // timeouts, failures and a replaced pairing must not earn this marker.
        if let revision, scope == assignmentScope, !Task.isCancelled,
           let current = nativeHistoryCompletedReads[chat.id], current != completed {
            nativeHistoryReadRevisions[chat.id] = revision
        }
    }

    private func performNativeHistoryReload(_ chat: ChatSummary) async {
        guard isProject(chat), !previewMode, !nativeHistoryRefreshing.contains(chat.id) else { return }
        let scope = assignmentScope
        nativeHistoryRefreshing.insert(chat.id)
        #if WONDER_DIAGNOSTICS
        LatencyHops.mark("reload.start", chat.id)
        #endif
        guard await connectionReady(), let saved = connection, !accessEnded, scope == assignmentScope else {
            if scope == assignmentScope {
                nativeHistoryRefreshing.remove(chat.id)
                if !Task.isCancelled { nativeHistoryFailures.insert(chat.id) }
            }
            return
        }
        defer {
            if scope == assignmentScope {
                nativeHistoryRefreshing.remove(chat.id)
                let caughtUp = pendingCatchUp.remove(chat.id) != nil
                if projection.dirty.contains(chat.id) { scheduleConversationRefresh(chat, run: generation) }
                else if caughtUp { rereadAfterRecovery(chat, run: generation) }
            }
        }
        struct Status: Decodable, Sendable { let state: String }
        let path = "/api/v1/conversations/\(Self.escape(chat.id))/history/refresh"
        #if WONDER_DIAGNOSTICS
        LatencyHops.mark("reload.ready", chat.id)
        #endif
        do {
            let detail = try await projects.loadDetail(chat.id)
            #if WONDER_DIAGNOSTICS
            LatencyHops.mark("reload.detail", chat.id)
            #endif
            guard detail.hasNativeSession else {
                nativeHistoryFailures.remove(chat.id)
                // A chat started in T3 Code (OpenCode/Cursor) has no native
                // session: the host already holds its history (read from T3
                // Code), so read it here as a native chat's refresh ends,
                // on a cold launch and after each new turn.
                await refreshConversation(chat, afterCurrentRead: true)
                return
            }
            #if WONDER_DIAGNOSTICS
            LatencyHops.mark("refresh.post", chat.id)
            #endif
            var status: Status = try await api.request(path, origin: saved.origin, body: Data("{}".utf8), credential: saved.credential, decodingStatuses: [202])
            #if WONDER_DIAGNOSTICS
            LatencyHops.mark("refresh.posted", chat.id, "state=\(status.state)")
            #endif
            // Bounded wait; saved history stays on screen meanwhile. A long chat
            // can take a while: one still refreshing is not a failure, and the
            // open conversation's activity check picks up its result later.
            // Short first waits: a small chat's import is done in tens of milliseconds, and a
            // fixed 250 ms step added up to that much to every open and every wake.
            var polls = 0
            for _ in 0..<120 where status.state == "refreshing" {
                try await Task.sleep(for: .milliseconds(min(250, 40 << min(polls, 3))))
                polls += 1
                guard scope == assignmentScope, !Task.isCancelled else { return }
                status = try await api.request(path, origin: saved.origin, credential: saved.credential)
            }
            guard scope == assignmentScope else { return }
            #if WONDER_DIAGNOSTICS
            LatencyHops.mark("refresh.finished", chat.id, "state=\(status.state)")
            #endif
            if status.state == "refreshing" {
                nativeHistoryFailures.remove(chat.id)
                // A long native scan can already have saved newer messages.
                // The wait limit must not keep the phone on its old snapshot.
                await refreshConversation(chat, afterCurrentRead: true)
                return
            }
            guard status.state == "completed" else { throw PairingFailure.response(503) }
            nativeHistoryFailures.remove(chat.id)
            // An older GET can finish while this waits. Its success cannot
            // certify the canonical GET requested after this import.
            let read = conversationCompletedReads[chat.id]
            let olderRead = loadingTokens[chat.id]
            await refreshConversation(chat, afterCurrentRead: true)
            if scope == assignmentScope, !Task.isCancelled,
               let current = conversationCompletedReads[chat.id], current != read, current != olderRead {
                nativeHistoryCompletedReads[chat.id] = UUID()
            }
            // T3 Code queues a message behind its running reply and starts it
            // later; its queue is read on the same check (the Mac reads T3
            // Code's queue when asked, at most every few seconds).
            if t3CodeSends(chat.id) || queues[chat.id]?.contains(where: { $0.waitsInT3Code == true }) == true {
                try? await loadQueue(chat, afterCurrentRead: true)
            }
        } catch is CancellationError {
        } catch PairingFailure.response(429) {
            // Other history is refreshing on the Mac; the next check retries.
        } catch {
            guard scope == assignmentScope else { return }
            nativeHistoryFailures.insert(chat.id)
        }
    }
    /// Opening a chat asks the Mac to re-read its native history (and waits
    /// for it) unless the saved copy was re-read at the provider's current
    /// history revision this launch. Turn ID and status alone are not enough:
    /// Claude adds to and revises a finished turn while the phone is away.
    func reloadNativeHistoryIfBehind(_ chat: ChatSummary) async {
        guard isProject(chat), !previewMode else { return }
        let check = await nativeHistoryCurrent(chat)
        if !check.current || check.revision == nil || nativeHistoryReadRevisions[chat.id] != check.revision {
            await reloadNativeHistory(chat, revision: check.revision)
        }
        #if WONDER_DIAGNOSTICS
        diagnosticOpenHistoryChecked.insert(chat.id)
        #endif
    }
    #if WONDER_DIAGNOSTICS
    private var diagnosticOpenHistoryChecked: Set<String> = []
    #endif
    /// The provider history revision (`historyRevision`, its transcript's
    /// size and change time) each chat was last fully re-read at. In memory:
    /// a launch re-reads a chat once when it opens.
    private var nativeHistoryReadRevisions: [String: String] = [:]
    private var nativeHistoryCompletedReads: [String: UUID] = [:]
    // UI errors deliberately stay hidden for a cached chat. Only installing a
    // successful canonical read issues a receipt that an import can rely on.
    private var conversationCompletedReads: [String: UUID] = [:]
    /// Whether the shown history already ends with the newest native turn,
    /// settled or still running live here. False whenever that cannot be
    /// confirmed. Also returns the provider's history revision where the Mac
    /// reports one.
    private func nativeHistoryCurrent(_ chat: ChatSummary) async -> (current: Bool, revision: String?) {
        guard isProject(chat), !previewMode, let saved = connection, !accessEnded,
              !nativeActivityUnsupported.contains(assignmentScope), let snapshot = snapshots[chat.id] else { return (false, nil) }
        struct Activity: Decodable, Sendable {
            let latestTurnId: String?; let latestTurnStatus: String; let runningElsewhere: Bool; let threadStatus: NativeThreadStatus?
            let historyRevision: String?
        }
        let scope = assignmentScope, requestID = UUID()
        #if WONDER_DIAGNOSTICS
        LatencyHops.mark("activity.request", chat.id)
        defer { LatencyHops.mark("activity.done", chat.id) }
        #endif
        nativeActivityRequests[chat.id] = requestID
        defer { if nativeActivityRequests[chat.id] == requestID { nativeActivityRequests.removeValue(forKey: chat.id) } }
        guard let activity: Activity = try? await api.request("/api/v1/conversations/\(Self.escape(chat.id))/history/activity",
            origin: saved.origin, credential: saved.credential), scope == assignmentScope, nativeActivityRequests[chat.id] == requestID, !accessEnded, !Task.isCancelled else {
            if scope == assignmentScope, nativeActivityRequests[chat.id] == requestID { noteNativeThreadStatus(nil, chat: chat.id, failed: true) }
            return (false, nil)
        }
        noteNativeThreadStatus(activity.threadStatus, chat: chat.id)
        let settled = activity.threadStatus?.isWorking != true && snapshot.showsLatestNativeTurn(id: activity.latestTurnId, status: activity.latestTurnStatus,
                                                                                                 runningElsewhere: activity.runningElsewhere)
        // A reply this screen already follows live is as current as a re-read
        // can make it: the re-read cannot settle a running turn, and a message
        // sent now waits behind it. Only a history read this connection and
        // kept current by the same open stream since counts; a saved copy (a
        // cold load, a return from the background, a stream that ended and
        // reopened, even if replaying what it missed) re-reads, as does a
        // newer or foreign turn, or a finished one whose process still works
        // (Claude background tasks).
        let live = snapshots[chat.id]
        let following = activity.latestTurnStatus == "inProgress" && !activity.runningElsewhere && activity.latestTurnId != nil
            && eventStreamLive && historyReadOnStream[chat.id] == eventStreamBreaks
            && !projection.dirty.contains(chat.id) && projection.snapshots[chat.id] != nil
            && live?.activeTurnID == activity.latestTurnId && live?.activeTurnRunsElsewhere == false
        return (settled || following, activity.historyRevision)
    }
    /// Claude conversations another app on the Mac holds (Claude Code in a
    /// terminal, or an app still running background work there), which keeps
    /// Wonder's messages waiting until it lets go or Send now takes it over.
    @Published private(set) var nativeOpenElsewhere: Set<String> = []
    /// The app on the Mac that has each Claude conversation open.
    @Published private(set) var nativeHolders: [String: MacHolder] = [:]
    /// Claude on the Mac finished its reply but still runs agent tasks, commands
    /// or monitors in its own process for this chat; sending ends them.
    @Published private(set) var nativeTasksOnMac: Set<String> = []
    /// Threads T3 Code started whose replies Cupola runs itself while T3 Code
    /// isn't running on the Mac (`runsWithoutT3Code`), for the composer's note.
    @Published private(set) var nativeRunsWithoutT3Code: Set<String> = []
    /// Threads T3 Code started, not open there, whose waiting message T3 Code's
    /// send can't carry (`t3CodeCantSend`: "attachments", "skills" or "model").
    @Published private(set) var nativeT3CodeCantSend: [String: String] = [:]
    /// Chats whose Mac (Claude desktop or another Agent SDK app) is writing a
    /// reply that a message sent now stops: the last sender wins. The Mac
    /// decides; an older Mac never reports it and keeps queuing.
    @Published private(set) var nativeStopsMacReply: Set<String> = []
    /// How full each Project chat's context window is, only where its
    /// provider reported it (the thread header's ring). A later read that
    /// can't measure keeps the last known value; only a newer one replaces it.
    /// The last reading per chat is saved with the pairing's chats and restored
    /// at launch, so an opening chat shows it before the computer answers.
    @Published private(set) var nativeContextUsage: [String: ContextUsage] = [:]
    /// Why a chat has no reading, in the provider's own words (from the computer).
    @Published private(set) var nativeContextNote: [String: String] = [:]
    /// Project chats another app on the Mac wrote since their open screen last
    /// checked: the host's `native_history` wake-ups. The open screen waits on
    /// them, so they are not published.
    private let nativeWakes = NativeWakeSignal()
    /// Whether the Mac woke this chat since the last call.
    func takeNativeWake(_ chat: String) -> Bool { nativeWakes.take(chat) }
    /// Waits for the Mac's next wake-up for this chat (true), or until the
    /// deadline passes or the task is cancelled (false).
    func waitForNativeWake(_ chat: String, until deadline: ContinuousClock.Instant) async -> Bool {
        await nativeWakes.wait(chat, until: deadline)
    }
    /// The thread header offers its context ring once the provider reported
    /// tokens, or once the computer said why it has not (the panel then says so
    /// plainly); before the first answer there is nothing to show. Plan limits
    /// are not here: Usage beside Settings is their one entry point.
    func showsContextButton(_ chat: ChatSummary) -> Bool {
        isProject(chat) && (nativeContextUsage[chat.id] != nil || nativeContextNote[chat.id] != nil)
    }
    /// A host reading replaces the saved one only when it is not older.
    private func noteContextUsage(_ usage: ContextUsage?, note: String?, chat: String) {
        if let usage = usage?.known {
            if let current = nativeContextUsage[chat], (current.atMs ?? 0) > (usage.atMs ?? 0) { return }
            guard nativeContextUsage[chat] != usage else { return }
            nativeContextUsage[chat] = usage
            nativeContextNote.removeValue(forKey: chat)
            saveContextUsage()
        } else if nativeContextUsage[chat] == nil {
            // Nothing known: the host says why, and the button explains it.
            if nativeContextNote[chat] != note { nativeContextNote[chat] = note }
        }
    }
    private func saveContextUsage() {
        guard !previewMode else { return }
        nativeContextUsage = nativeContextUsage.trimmedToNewest()
        // The intent writer keeps order, waits out a locked phone and is
        // discarded by Forget; it is nil while no pairing is loaded.
        intents?.saveContextUsage(nativeContextUsage)
    }
    /// Latest request owns transient process state across concurrent send/poll reads.
    private var nativeActivityRequests: [String: UUID] = [:]
    /// The Mac's last activity-check answer per chat: the one store behind a chat's
    /// "working" from the Mac. `ThreadActivity.derive` decides what it means.
    private var nativeReadings: [String: ThreadActivity.HostReading] = [:]
    /// The fresh readings that say work is running; Live Activities and the
    /// current step read these. Only `publishNativeReadings` writes it.
    @Published private(set) var nativeThreadStatus: [String: NativeThreadStatus] = [:]
    private var nativeReadingExpiry: Task<Void, Never>?
    /// Records one check's outcome. A failed check keeps the last answer and its
    /// age (no flicker); an answer expires on its own (`ThreadActivity.hostReadingLifetime`).
    private func noteNativeThreadStatus(_ status: NativeThreadStatus?, chat: String, failed: Bool = false) {
        let now = Date()
        if failed {
            nativeReadings[chat] = (nativeReadings[chat] ?? ThreadActivity.HostReading(status: nil, answeredAt: now)).failing(at: now)
        } else if let status {
            nativeReadings[chat] = ThreadActivity.HostReading(status: status, answeredAt: now)
        } else {
            nativeReadings.removeValue(forKey: chat)
        }
        if nativeReadings.count > 64,
           let oldest = nativeReadings.filter({ $0.key != chat }).min(by: { $0.value.answeredAt < $1.value.answeredAt })?.key {
            nativeReadings.removeValue(forKey: oldest)
        }
        publishNativeReadings()
    }
    private func forgetNativeReading(_ chat: String) {
        guard nativeReadings.removeValue(forKey: chat) != nil else { return }
        publishNativeReadings()
    }
    private func resetNativeReadings() {
        nativeReadingExpiry?.cancel(); nativeReadingExpiry = nil
        nativeReadings = [:]
        if !nativeThreadStatus.isEmpty { nativeThreadStatus = [:] }
    }
    private func publishNativeReadings() {
        let now = Date()
        nativeReadings = nativeReadings.filter { $0.value.isFresh(at: now) || $0.value.failedSince != nil }
        let working = nativeReadings.compactMapValues { reading -> NativeThreadStatus? in
            reading.isFresh(at: now) && reading.status?.isWorking == true ? reading.status : nil
        }
        if nativeThreadStatus != working { nativeThreadStatus = working }
        // One timer, aimed at the next answer to lapse, so a check that stopped
        // can't leave "Working" up until something else happens to re-render.
        nativeReadingExpiry?.cancel(); nativeReadingExpiry = nil
        guard let next = working.keys.compactMap({ nativeReadings[$0]?.expiry() }).min() else { return }
        nativeReadingExpiry = Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(max(0.05, next.timeIntervalSinceNow) + 0.05))
            guard let self, !Task.isCancelled else { return }
            self.publishNativeReadings()
        }
    }
    /// What a chat is doing: the single answer every surface reads (the open
    /// chat's Working and composer, the Chats list's rows, Live Activities).
    func threadActivity(_ chat: String, rowWorking: Bool = false) -> ThreadActivity {
        let snapshot = snapshots[chat]
        return ThreadActivity.derive(turnActive: snapshot?.activeTurnID != nil,
                                     turnStartedAt: nil,
                                     pendingMessage: snapshot?.hasUnassignedPreTurnWork == true,
                                     host: nativeReadings[chat], rowWorking: rowWorking, now: Date())
    }
    /// A Chats row: the list's own answer joins the thread's.
    func rowActivity(_ thread: ProjectThreadSummary) -> ThreadActivity {
        guard let chat = thread.conversationId else { return thread.isWorking ? .working(.row, since: nil) : .idle }
        return threadActivity(chat, rowWorking: thread.isWorking)
    }
    /// A provider can continue after a receipt ends. Keep checking its process
    /// level without fabricating a turn ID for Guide or Stop.
    func shouldPollNativeActivity(_ chat: String) -> Bool {
        !conversationWorking(chat) || turnRunsElsewhere(chat) || activeTurn(chat) == nil
    }
    private var nativeActivity: [String: String] = [:]
    private var nativeTerminalActivity: Set<String> = []
    func nativeHistoryNeedsPromptRefresh(_ chat: String) -> Bool { nativeTerminalActivity.contains(chat) }
    private var nativeActivityUnsupported: Set<String> = []
    /// Checks whether work on the Mac started, finished or moved on, without
    /// re-reading history. Returns true when the conversation should reload.
    /// `holderOnly` (while Cupola's own reply runs and turn polling pauses)
    /// reads only which app on the Mac has the chat open: Cupola's turn says
    /// nothing about that app, so the notices and Send now stay current.
    func nativeActivityChanged(_ chat: ChatSummary, holderOnly: Bool = false) async -> Bool {
        guard isProject(chat), !previewMode, let saved = connection, !accessEnded,
              !nativeActivityUnsupported.contains(assignmentScope) else { return false }
        struct Activity: Decodable, Sendable {
            let latestTurnId: String?; let latestTurnStatus: String; let runningElsewhere: Bool; let openElsewhere: Bool?; let tasksOnMac: Bool?
            let sendStopsMacReply: Bool?
            let heldBy: MacHolder?
            /// A thread T3 Code started, whose reply Cupola runs because T3 Code isn't running.
            let runsWithoutT3Code: Bool?
            /// What T3 Code's send can't carry in the waiting message to such a thread.
            let t3CodeCantSend: String?
            let threadStatus: NativeThreadStatus?
            /// How full the context window is; absent or null while unknown.
            let contextUsage: ContextUsage?
            /// Why `contextUsage` is absent, in the provider's own words.
            let contextUsageNote: String?
            /// Claude can add to a finished turn: its own reply when a background task ends.
            let latestTurnItems: Int?
        }
        let scope = assignmentScope, requestID = UUID()
        nativeActivityRequests[chat.id] = requestID
        defer { if nativeActivityRequests[chat.id] == requestID { nativeActivityRequests.removeValue(forKey: chat.id) } }
        #if WONDER_DIAGNOSTICS
        LatencyHops.mark("poll.activity.request", chat.id, holderOnly ? "holderOnly" : "")
        defer { LatencyHops.mark("poll.activity.done", chat.id) }
        #endif
        do {
            let activity: Activity = try await api.request("/api/v1/conversations/\(Self.escape(chat.id))/history/activity",
                origin: saved.origin, credential: saved.credential)
            guard scope == assignmentScope, nativeActivityRequests[chat.id] == requestID, !accessEnded, !Task.isCancelled else { return false }
            setMember(\.nativeOpenElsewhere, chat.id, activity.openElsewhere == true)
            if nativeHolders[chat.id] != activity.heldBy { nativeHolders[chat.id] = activity.heldBy }
            setMember(\.nativeTasksOnMac, chat.id, activity.tasksOnMac == true)
            setMember(\.nativeRunsWithoutT3Code, chat.id, activity.runsWithoutT3Code == true)
            if nativeT3CodeCantSend[chat.id] != activity.t3CodeCantSend { nativeT3CodeCantSend[chat.id] = activity.t3CodeCantSend }
            setMember(\.nativeStopsMacReply, chat.id, activity.sendStopsMacReply == true)
            // An unmeasured read (a slow one, a transcript line without usage)
            // says nothing new, so the ring keeps its value. The reading is the
            // host's newest, so Cupola's own running reply updates it too.
            noteContextUsage(activity.contextUsage, note: activity.contextUsageNote, chat: chat.id)
            // Cupola's running turn owns its own status, history and baseline.
            guard !holderOnly else { return false }
            noteNativeThreadStatus(activity.threadStatus, chat: chat.id)
            let signature = [activity.latestTurnId ?? "", activity.latestTurnStatus, String(activity.runningElsewhere),
                             activity.latestTurnItems.map(String.init) ?? ""].joined(separator: "\u{1F}")
            if ["completed", "failed", "interrupted", "cancelled"].contains(activity.latestTurnStatus),
               !activity.runningElsewhere, activity.threadStatus?.isWorking != true {
                nativeTerminalActivity.insert(chat.id)
            } else { nativeTerminalActivity.remove(chat.id) }
            let previous = nativeActivity.updateValue(signature, forKey: chat.id)
            return previous != nil && previous != signature
                || (previous == nil && activity.runningElsewhere != turnRunsElsewhere(chat.id))
        } catch PairingFailure.response(404) {
            // An older Mac without the check: keep the slower running refresh.
            if scope == assignmentScope, nativeActivityRequests[chat.id] == requestID {
                nativeActivityUnsupported.insert(scope)
                if !holderOnly { forgetNativeReading(chat.id) }
            }
            return false
        } catch {
            if scope == assignmentScope, nativeActivityRequests[chat.id] == requestID, !holderOnly { noteNativeThreadStatus(nil, chat: chat.id, failed: true) }
            return false
        }
    }
    /// Which app on the Mac holds a chat is known only while its screen checks
    /// (`nativeActivityChanged`). Once the check stops (the chat closed, Cupola
    /// went to the background, the pairing changed), the last answer can be
    /// out of date, so the notices, waiting hints and Send now forget it until
    /// the next check answers.
    func forgetNativeHolder(_ chat: String) {
        refreshCoordinator.discardRequests(.nativeHistory(chat))
        nativeActivityRefreshes.remove(chat)
        nativeTerminalActivity.remove(chat)
        forgetNativeReading(chat)
        setMember(\.nativeOpenElsewhere, chat, false)
        if nativeHolders[chat] != nil { nativeHolders.removeValue(forKey: chat) }
        setMember(\.nativeTasksOnMac, chat, false)
        setMember(\.nativeRunsWithoutT3Code, chat, false)
        if nativeT3CodeCantSend[chat] != nil { nativeT3CodeCantSend.removeValue(forKey: chat) }
        setMember(\.nativeStopsMacReply, chat, false)
    }
    private func forgetNativeHolders() {
        for chat in nativeOpenElsewhere.union(nativeHolders.keys).union(nativeTasksOnMac).union(nativeRunsWithoutT3Code)
            .union(nativeT3CodeCantSend.keys).union(nativeStopsMacReply) where chat != "preview" {
            forgetNativeHolder(chat)
        }
    }
    /// Polls write their answer through these: assigning a @Published value
    /// (even inserting a present member) re-renders every observer, so an
    /// unchanged answer must leave the value alone.
    private func setMember(_ set: ReferenceWritableKeyPath<ConnectionModel, Set<String>>, _ member: String, _ present: Bool) {
        if self[keyPath: set].contains(member) != present {
            if present { self[keyPath: set].insert(member) } else { self[keyPath: set].remove(member) }
        }
    }
    private func setIfChanged<Value: Equatable>(_ path: ReferenceWritableKeyPath<ConnectionModel, Value>, _ value: Value) {
        if self[keyPath: path] != value { self[keyPath: path] = value }
    }
    @Published var status = "Connect to your computer to get started."
    /// Why this computer can't be used until the owner acts, when that is
    /// known; nil while it is merely unreachable. `status` carries the words.
    @Published private(set) var connectionLoss: ConnectionLoss?
    @Published var busy = false
    @Published var verification: String?
    @Published var error: String?
    @Published var accessEnded = false { didSet { noteListChange(); if accessEnded { computerPictures.invalidate(); resetNativeReadings(); nativeActivityRequests = [:]; cancelApprovalSettings(); connectedAppsCache = [:]; usageCache = [:]; goals = [:]; goalErrors = [:]; goalMutationTokens = [:]; chatNotifications.forgetAll(); chatNotificationReads = []; projectSubagents = [:]; pullRequests = [:]; pullRequestErrors = [:]; pullRequestLoadTokens = [:]; gitSummaries = [:]; gitSummaryLoadTokens = [:]; threadChangeCounts = [:]; threadChangeLoadTokens = [:]; devServers = [:]; devServerLoadTokens = [:]; threadRepositories = [:]; threadRepositoryLoadTokens = [:]; projectSubagentLookup = [:]; projectSubagentFreshIDs = [:]; projectSubagentAvailability = [:]; projectSubagentErrors = [:]; projectSubagentNextCurrentCursor = [:]; projectSubagentNextArchivedCursor = [:]; projectSubagentInitialCurrentCursor = [:]; projectSubagentInitialArchivedCursor = [:]; projectSubagentSeenCurrentCursors = [:]; projectSubagentSeenArchivedCursors = [:]; projectSubagentExpandedParents = []; projectSubagentPagingLimited = []; projectSubagentRefreshPending = []; projectSubagentPageRevision = [:]; loadingOlderProjectSubagents = []; projectSubagentLoadTokens = [:]; resetImagePreviews(); dictation.forget(); endVoice(); cameraContextID = UUID() } } }
    #if WONDER_DIAGNOSTICS
    @Published var chats: [ChatSummary] = []
    #endif
    @Published var projectSubagents: [String: [ProjectSubagentSummary]] = [:] {
        // A Claude session can list a thousand tasks; group them once per change, not per render.
        didSet {
            var sections: [String: ProjectSubagentSections] = [:]
            for (id, agents) in projectSubagents {
                sections[id] = oldValue[id] == agents ? projectSubagentSections[id] ?? agents.sectioned() : agents.sectioned()
            }
            projectSubagentSections = sections
        }
    }
    private(set) var projectSubagentSections: [String: ProjectSubagentSections] = [:]
    @Published var projectSubagentFreshIDs: [String: Set<String>] = [:]
    private var projectSubagentLookup: [String: [String: ProjectSubagentSummary]] = [:]
    @Published var projectSubagentAvailability: [String: Bool] = [:]
    @Published var projectSubagentErrors: [String: String] = [:]
    /// Pull requests per Project thread, read through the Mac's GitHub CLI.
    @Published var pullRequests: [String: ThreadPullRequests] = [:]
    @Published var pullRequestErrors: [String: String] = [:]
    @Published var refreshingPullRequests: Set<String> = []
    private var pullRequestLoadTokens: [String: UUID] = [:]
    /// The branch each Project thread's folder is on, read with Git on the Mac.
    @Published var gitSummaries: [String: WorkspaceGitSummary] = [:]
    private var gitSummaryLoadTokens: [String: UUID] = [:]
    /// How many files each Project thread changed itself ("This thread"), for
    /// the Git pill; absent where the Mac can't say.
    @Published private(set) var threadChangeCounts: [String: ThreadChangeCount] = [:]
    private var threadChangeLoadTokens: [String: UUID] = [:]
    /// Dev servers each Project runs on the Mac, for the composer's globe pill (by Project id).
    @Published var devServers: [String: DevServerPresence] = [:]
    private var devServerLoadTokens: [String: UUID] = [:]
    /// Every repository each Project thread worked with, from the Mac's index
    /// of the thread's own records. Absent for a Mac without the index.
    @Published var threadRepositories: [String: ThreadRepositories] = [:]
    private var threadRepositoryLoadTokens: [String: UUID] = [:]
    /// Task IDs (tool-use IDs) with a Stop request in flight, per parent conversation.
    @Published var stoppingProjectSubagents: [String: Set<String>] = [:]
    @Published var projectSubagentStopErrors: [String: String] = [:]
    @Published var projectSubagentNextCurrentCursor: [String: String] = [:]
    @Published var projectSubagentNextArchivedCursor: [String: String] = [:]
    @Published var loadingOlderProjectSubagents: Set<String> = []
    private var projectSubagentLoadTokens: [String: UUID] = [:]
    private var projectSubagentSeenCurrentCursors: [String: Set<String>] = [:]
    private var projectSubagentSeenArchivedCursors: [String: Set<String>] = [:]
    private var projectSubagentInitialCurrentCursor: [String: String] = [:]
    private var projectSubagentInitialArchivedCursor: [String: String] = [:]
    private var projectSubagentExpandedParents: Set<String> = []
    private var projectSubagentPagingLimited: Set<String> = []
    private var projectSubagentRefreshPending: Set<String> = []
    private var projectSubagentPageRevision: [String: Int] = [:]
    @Published var goals: [String: ConversationGoal] = [:]
    @Published var goalErrors: [String: String] = [:]
    /// Each chat's notification choice on this computer (`ChatNotificationChoices`).
    @Published var chatNotifications = ChatNotificationChoices()
    private(set) var chatNotificationReads: Set<String> = []
    private var goalMutationTokens: [String: UUID] = [:]
    @Published var savingComposerSettings: Set<String> = []
    /// A Project thread's model chosen from the other provider, carried by its next messages until the host has moved the thread.
    @Published var projectNextModels: [String: ProjectMessageModel] = [:]
    @Published var projectAccessChanges: [String: ProjectAccessChange] = [:]
    var projectAccessTasks: [String: Task<Void, Never>] = [:]
    var projectAccessTokens: [String: UUID] = [:]
    private(set) var currentActivities: [String: CurrentActivity] = [:]
    private(set) var computerActivities: [String: ComputerAgentActivity] = [:]
    /// Canonical history per conversation: reads always see the latest
    /// state, including a chat whose live updates were not announced.
    private var storedSnapshots: [String: ConversationSnapshot] = [:]
    /// Assigning announces every change. The live stream instead goes through
    /// `installSnapshots(_:announce: false)`, which announces only what a view
    /// can show: the open conversation, or a chat's summary (see `SnapshotSummary`).
    var snapshots: [String: ConversationSnapshot] {
        get { storedSnapshots }
        set { installSnapshots(newValue, announce: true) }
    }
    /// Whether any chat's history shows work running. Live Activities wake on
    /// this instead of rescanning every snapshot on each streamed change.
    @Published private(set) var hasSnapshotWork = false
    /// What views outside the open conversation read from a chat's history:
    /// the Chats list's working state and current step.
    private struct SnapshotSummary: Equatable {
        let activeTurnID: String?
        let runsElsewhere: Bool
        let unassignedWork: Bool
        let activity: CurrentActivity?
        let computer: ComputerAgentActivity?
    }
    private func summary(_ id: String) -> SnapshotSummary? {
        storedSnapshots[id].map {
            SnapshotSummary(activeTurnID: $0.activeTurnID, runsElsewhere: $0.activeTurnRunsElsewhere,
                            unassignedWork: $0.hasUnassignedPreTurnWork,
                            activity: currentActivities[id], computer: computerActivities[id])
        }
    }
    #if WONDER_DIAGNOSTICS
    /// Diagnostics: snapshot installs that announced a change, and those that stayed silent.
    private(set) var snapshotAnnouncements = (announced: 0, silent: 0)
    /// Installs history as the live stream does (`publish(.snapshots)`).
    func diagnosticInstallStreamedSnapshots(_ value: [String: ConversationSnapshot]) {
        installSnapshots(value, announce: false)
    }
    #endif
    /// Chats whose history shows work running; kept per changed chat.
    private var workingSnapshots: Set<String> = []
    /// A streamed change to a chat nobody is looking at updates its history
    /// and derived state without re-rendering every view observing this model.
    private func installSnapshots(_ value: [String: ConversationSnapshot], announce force: Bool) {
        let oldValue = storedSnapshots
        // Unchanged snapshots share storage with the old ones, so comparing them is cheap.
        let changed = value.compactMap { oldValue[$0.key] != $0.value ? $0.key : nil }
        let removed = oldValue.keys.filter { value[$0] == nil }
        let watched = Set([visibleChat?.id, selectedChat?.id, rowCache?.id].compactMap { $0 })
        let before = Dictionary(uniqueKeysWithValues: changed.map { ($0, summary($0)) })
        // Announce before changing when the open chat or the set of chats changes.
        let announceNow = force || !removed.isEmpty || changed.contains { oldValue[$0] == nil || watched.contains($0) }
        if announceNow { objectWillChange.send() }
        storedSnapshots = value
        snapshotsChanged(from: oldValue)
        let announceAfter = !announceNow && changed.contains { before[$0] != summary($0) }
        if announceAfter { objectWillChange.send() }
        #if WONDER_DIAGNOSTICS
        if announceNow || announceAfter { snapshotAnnouncements.announced += 1 } else { snapshotAnnouncements.silent += 1 }
        #endif
        workingSnapshots.subtract(removed)
        for id in changed {
            if let snapshot = value[id], snapshot.activeTurnID != nil || snapshot.hasUnassignedPreTurnWork { workingSnapshots.insert(id) }
            else { workingSnapshots.remove(id) }
        }
        if hasSnapshotWork != !workingSnapshots.isEmpty { hasSnapshotWork = !workingSnapshots.isEmpty }
        armWorkWatchdog()
    }
    private func snapshotsChanged(from oldValue: [String: ConversationSnapshot]) {
        // Prepare stream changes at the model boundary, before the view
        // asks for rows. Exact input validation also catches same-sequence
        // refreshes, edits and history revisions that count checks missed.
        if let cached = rowCache, oldValue[cached.id] != snapshots[cached.id] {
            if let snapshot = snapshots[cached.id] {
                rowCache = (cached.id, cached.title, cached.family, prepareRows(snapshot, author: cached.family?.title ?? cached.title))
            } else {
                rowCache = nil
                timelineCache = nil
                rowProjection = ConversationRowProjection()
            }
        }
        for chat in awaitingAcceptance.keys where oldValue[chat]?.lastSequence != snapshots[chat]?.lastSequence {
            reconcileAcceptance(chat)
        }
        currentActivities = currentActivities.filter { snapshots[$0.key] != nil }
        computerActivities = computerActivities.filter { snapshots[$0.key] != nil }
        for (id, snapshot) in snapshots {
            let old = oldValue[id]
            if old?.hostEpoch != snapshot.hostEpoch || old?.lastSequence != snapshot.lastSequence
                || old?.activeTurnID != snapshot.activeTurnID || old?.thread.nextCursor != snapshot.thread.nextCursor
                || old?.thread.turns?.last?.items.count != snapshot.thread.turns?.last?.items.count {
                currentActivities[id] = CurrentActivity.latest(in: snapshot)
                computerActivities[id] = ComputerAgentActivity.latest(in: snapshot)
            }
        }
        // No list revision: the Chats list, root and widget read the
        // Project catalog, not history, so a streamed frame re-renders
        // only the views observing this model.
    }
    func currentStep(_ conversation: String) -> String? {
        guard macConnected == true, !accessEnded else { return nil }
        if let activity = currentActivities[conversation]?.label { return activity }
        guard let status = nativeThreadStatus[conversation], status.isWorking == true else { return nil }
        return status.activeFlags?.contains(where: { ["waitingOnApproval", "waitingOnUserInput"].contains($0) }) == true
            ? "Needs input" : "Working"
    }
    // Retain only the most recently presented conversation. Expansion and scroll
    // state do not change its source rows or require reparsing every timestamp.
    private var rowProjection = ConversationRowProjection()
    private var rowCache: (id: String, title: String, family: AgentFamily?, rows: [ReadRow])? { didSet { rowRevision &+= 1 } }
    private var rowRevision: UInt64 = 0
    private var timelineCache: (key: ConversationTimeline.Key, value: ConversationTimeline)?
    /// The Chats list observes this instead of every model change, so typing,
    /// sends and per-chat state do not re-render the whole list and root.
    @Published private(set) var listRevision: UInt64 = 0
    private func noteListChange() { listRevision &+= 1 }
    @Published var selectedChat: ChatSummary? {
        didSet { if selectedChat?.id != oldValue?.id { visibleChat = selectedChat } }
    }
    // The list keeps the root selected while navigation can show a verified child.
    @Published private(set) var visibleChat: ChatSummary? {
        didSet {
            guard visibleChat?.id != oldValue?.id else { return }
            cameraContextID = UUID()
            releaseClosedConversation()
        }
    }
    /// Leaving a conversation drops what was derived for showing it (rows,
    /// grouped timeline) and its catch-up events. Its saved history, draft,
    /// pins and reading position stay; reopening derives the rows again.
    private func releaseClosedConversation() {
        if rowCache != nil, rowCache?.id != visibleChat?.id {
            rowCache = nil
            timelineCache = nil
            rowProjection = ConversationRowProjection()
        }
        liveLog.watch(visibleChat?.id, from: projection.lastSequence)
        if let id = visibleChat?.id { projection.touchConversation(id) }
        scheduleResidentTrim()
    }
    @Published private(set) var cameraContextID = UUID()
    @Published var searchFocus: SearchMessageFocus?
    private var searchFocusTask: Task<Void, Never>?
    /// Guards overlapping list loads. Not published: nothing on screen reads it.
    private var loadingChats = false
    /// Conversations with a history request in flight. Per conversation so a
    /// slow load cannot make a different chat look busy or empty.
    @Published private(set) var loadingConversationIDs: Set<String> = []
    /// Set only when a conversation has no saved content and loading failed.
    @Published private(set) var conversationLoadFailures: [String: ChatOpenFailure] = [:]
    // Refreshes assign these through setIfChanged: @Published emits on every
    // assignment, before didSet could compare with oldValue.
    @Published var cachedConversationIds: Set<String> = [] { didSet { if cachedConversationIds != oldValue { noteListChange() } } }
    /// Something this device could not keep for this computer, shown under the
    /// computer in Chats until a later save succeeds or Try again clears it.
    @Published private(set) var storageFailure: DeviceStorageFailure?
    @Published private(set) var hasConnectedThisLaunch = false { didSet { if hasConnectedThisLaunch != oldValue { noteListChange() } } }
    @Published private(set) var checkingConnection = false
    @Published var macConnected: Bool? {
        didSet {
            if macConnected == true, !hasConnectedThisLaunch { hasConnectedThisLaunch = true }
            if macConnected != oldValue { noteListChange() }
            if macConnected == true { endRescue() }
            // Reconnecting (including on returning to the foreground) sends
            // what waited in the outbox.
            if macConnected == true, oldValue != true {
                outboxAttempts = 0
                scheduleOutboxDrain()
                for chat in composers.keys { forgetRemovedUploads(chat) }
            }
        }
    }
    /// The computer's one name, as its host reports it. The host owns it:
    /// renaming here or in the Mac app changes it for both.
    var macName: String {
        #if WONDER_DIAGNOSTICS
        if let diagnosticRouteCheckpoint { return diagnosticRouteCheckpoint }
        #endif
        return connection?.hostName ?? (previewMode ? "Studio" : "Your computer")
    }
    /// Keeps the host's name with the saved connection.
    func applyHostName(_ name: String) throws {
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty, let latest = connection, latest.hostName != name else { return }
        let updated = SavedConnection(origin: latest.origin, credential: latest.credential, hostName: name,
                                      storageDeviceId: latest.storageDeviceId).preservingStorage(from: latest)
        try saveConnection(updated)
        connection = updated
    }
    // MARK: Reach from anywhere

    /// Whether this computer is reached through Cupola's relay right now.
    var usingRelay: Bool { connection.map { api.isRelay($0.origin) } ?? false }

    private var connectedStatus: String {
        usingRelay ? "Connected through Cupola’s relay." : "Connected to your computer."
    }

    /// The computer's relay state (`GET /api/v1/relay`).
    func relayStatus() async throws -> RelayStatus {
        guard let saved = connection, !accessEnded, !isDemo, !previewMode else { throw CancellationError() }
        return try await api.request("/api/v1/relay", origin: saved.origin, credential: saved.credential)
    }

    /// Finishes turning on Reach from anywhere after the owner allowed it on
    /// the computer: a one-time grant from the Cupola account for this
    /// computer's pinned key, handed over signed by this device.
    func enableRelay(account: WonderAccount) async throws -> RelayStatus {
        guard let saved = connection, !accessEnded, !isDemo, !previewMode else { throw CancellationError() }
        // Paired with a code: no saved key to check the computer against.
        guard let pin = saved.hostKeyPin else { throw RelayEnableFailure.pairedWithCode }
        let nonce = UUID().uuidString.replacingOccurrences(of: "-", with: "")
        let proof: HostProof = try await api.request("/api/v1/host/proof?nonce=\(nonce)", origin: saved.origin)
        try proof.validate(pin: pin, origin: saved.origin, nonce: nonce)
        guard let hostKey = proof.rawHostKey else { throw PairingFailure.wrongHost }
        let (grant, label) = try await account.relayGrant(hostKey: hostKey)
        let actionNonce = UUID().uuidString, issued = UInt64(Date().timeIntervalSince1970 * 1000)
        let transcript = try RelayEnable.transcript(grant: grant, label: label, nonce: actionNonce, connection: saved, issuedAtMs: issued)
        let body = try RelayEnable.body(grant: grant, label: label, nonce: actionNonce, issuedAtMs: issued,
                                        signature: try await signingIdentity.sign(transcript))
        let status: RelayStatus = try await api.request(RelayEnable.path, origin: saved.origin, body: body, credential: saved.credential)
        // The computer now advertises the relay address; learn it.
        Task { await self.check() }
        return status
    }

    /// Why Reach from anywhere didn't turn on, in the person's words.
    func relayFailure(_ error: Error) -> String {
        switch error {
        case AccountFailure.signedOut: "Sign in to your Cupola account in Notifications, then try again."
        case let failure as AccountFailure: failure.localizedDescription
        case RelayEnableFailure.pairedWithCode: RemoteAccessPresentation.pairedWithCode(macName)
        case PairingFailure.wrongHost: "\(macName) couldn’t prove its security key. Pair it again, then try again."
        case PairingFailure.response(403):
            "Reach from anywhere isn’t available for this Cupola account, or \(macName) doesn’t allow it. On \(macName), run `cupola relay on`, then try again."
        case PairingFailure.response(409): "Your Cupola account already reaches five computers this way. On one of them, run `cupola relay off`."
        case PairingFailure.response(503), PairingFailure.response(429): "Cupola’s relay is busy right now. Try again in a few minutes."
        default: "Couldn’t turn on Reach from anywhere. Check the connection to \(macName) and try again."
        }
    }

    // MARK: Recovery when Cupola is down on the computer

    /// What the host's supervisor said while the main route did not answer:
    /// nil unless it answered and a restart could help. One notice, shown by
    /// the chat's composer line and the computer's row in Chats.
    @Published private(set) var rescue: RescueNotice?
    /// The redacted bundle for Get diagnostics.
    @Published private(set) var rescueDiagnostics: RescueDiagnosticsState = .idle
    private var rescueProbe: Task<Void, Never>?
    private var rescueRestart: Task<Void, Never>?
    private var rescueProbedAt: ContinuousClock.Instant?
    /// When the host's "try again later" ends; Restart comes back then, even if the cause never changed.
    private var rescueLimitedUntil: ContinuousClock.Instant?
    private var rescueLimitExpiry: Task<Void, Never>?
    private var rescueClient: RescueClient {
        RescueClient(api: api, sign: { [signingIdentity] in try await signingIdentity.sign($0) })
    }

    /// After a main-route miss that was about reaching the computer: ask its
    /// supervisor, at most every few seconds, and only on the home network.
    private func probeRescue(_ saved: SavedConnection) {
        guard !isDemo, !previewMode, saved.hostKeyPin != nil, !accessEnded, connectionLoss == nil,
              !homeNetworkDenied, rescueProbe == nil, rescueRestart == nil, !RescueRoute.origins(saved).isEmpty else { return }
        let clock = ContinuousClock()
        if let last = rescueProbedAt, last.duration(to: clock.now) < .seconds(10) { return }
        rescueProbedAt = clock.now
        let run = generation
        rescueProbe = Task { [weak self] in
            let status = try? await self?.rescueClient.status(saved)
            guard let self, run == self.generation, !Task.isCancelled else { return }
            self.rescueProbe = nil
            guard self.macConnected != true, self.rescueRestart == nil else { return }
            let found = status.flatMap(RescueNotice.init)
            // A later miss keeps the person's place (a restart that failed stays said).
            if found?.cause != self.rescue?.cause || found == nil { self.setRescue(found) }
            self.expireRescueLimit()
        }
    }

    /// Offers Restart again once the host's cool-down has passed.
    private func expireRescueLimit() {
        guard let until = rescueLimitedUntil, until <= ContinuousClock().now else { return }
        rescueLimitedUntil = nil
        guard var notice = rescue, case .limited = notice.phase else { return }
        notice.phase = .ready
        setRescue(notice)
    }

    /// The transport generation changed (backgrounding, a new pairing check):
    /// a probe or restart in flight can no longer report, so free its slot
    /// and let a restart that was waiting be offered again.
    private func cancelRescueWork() {
        rescueProbe?.cancel(); rescueProbe = nil
        rescueRestart?.cancel(); rescueRestart = nil
        rescueProbedAt = nil
        if var notice = rescue, notice.phase == .restarting {
            notice.phase = .ready
            setRescue(notice)
        }
    }

    private func setRescue(_ notice: RescueNotice?) { if rescue != notice { rescue = notice } }

    /// The computer answered again, or the pairing changed: nothing to offer.
    private func endRescue() {
        cancelRescueWork()
        rescueLimitExpiry?.cancel(); rescueLimitExpiry = nil
        rescueLimitedUntil = nil
        setRescue(nil)
        if rescueDiagnostics != .idle { rescueDiagnostics = .idle }
    }

    /// Asks the supervisor to start Cupola, then keeps checking the main
    /// route until it answers. A tap starts it; nothing restarts on its own.
    func restartHost() {
        guard let notice = rescue, notice.offersRestart, rescueRestart == nil, let saved = connection,
              !accessEnded, !isDemo, !previewMode else { return }
        var started = notice; started.phase = .restarting
        let restarting = started
        setRescue(restarting)
        let run = generation
        rescueRestart = Task { [weak self] in
            guard let self else { return }
            defer { if run == self.generation { self.rescueRestart = nil } }
            @MainActor func finish(_ phase: RescueNotice.Phase) {
                guard run == self.generation, !Task.isCancelled, self.macConnected != true else { return }
                var next = restarting; next.phase = phase
                self.setRescue(next)
                if case .limited(let minutes) = phase { self.armRescueLimit(minutes: minutes) }
            }
            do { _ = try await self.rescueClient.restart(saved) }
            catch let failure as RescueFailure { finish(RescueNotice.phase(after: failure)); return }
            catch { finish(.unreachable); return }
            // The supervisor stops the old process, starts a new one and waits for it
            // to answer; ask the main route as it does, and watch for it giving up.
            let clock = ContinuousClock(), deadline = clock.now.advanced(by: .seconds(60))
            var stoppedAnswers = 0
            while clock.now < deadline {
                do { try await Task.sleep(for: .seconds(2)) } catch { return }
                guard run == self.generation, !Task.isCancelled else { return }
                await self.check(renew: true)
                if self.macConnected == true { return }
                if let status = try? await self.rescueClient.status(saved) {
                    stoppedAnswers = status.state == .gaveUp || status.state == .stopped ? stoppedAnswers + 1 : 0
                    if stoppedAnswers >= 2 { break }
                }
            }
            finish(.stillDown)
        }
    }

    private func armRescueLimit(minutes: Int) {
        let clock = ContinuousClock(), wait = Duration.seconds(minutes * 60)
        rescueLimitedUntil = clock.now.advanced(by: wait)
        rescueLimitExpiry?.cancel()
        rescueLimitExpiry = Task { [weak self] in
            try? await Task.sleep(for: wait)
            guard !Task.isCancelled else { return }
            self?.expireRescueLimit()
        }
    }

    /// The host's redacted log and state, for the person to read and share.
    func loadRescueDiagnostics() async {
        guard let saved = connection, !isDemo, !previewMode else { return }
        rescueDiagnostics = .loading
        do {
            let bundle = try await rescueClient.diagnostics(saved)
            guard !Task.isCancelled else { return }
            rescueDiagnostics = .ready(bundle.report(computer: macName))
        } catch is CancellationError {
            rescueDiagnostics = .idle
        } catch {
            rescueDiagnostics = .failed
        }
    }

    /// Renames the computer on its host; empty goes back to the computer's
    /// own name. Returns why it failed, in the owner's words.
    func renameHost(_ name: String) async -> String? {
        struct Reply: Decodable, Sendable { let name: String }
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            let body = try JSONSerialization.data(withJSONObject: ["name": trimmed.isEmpty ? NSNull() : trimmed as Any])
            let reply: Reply = try await manage("/api/v1/host/name", method: "PUT", body: body)
            try applyHostName(reply.name)
            return nil
        } catch is CancellationError {
            return nil
        } catch PairingFailure.response(422) {
            return "Keep the name to 60 characters on one line."
        } catch {
            return macConnected == true ? "\(macName) didn’t save the name. Try again." : "Connect to \(macName) to rename it."
        }
    }
    /// The host's operating system as it reports it (`macos`, `linux`); nil
    /// until the first status read and for hosts that predate the field.
    @Published private(set) var hostPlatform: String?
    /// Mac-only wording ("your Mac", Mac app settings) applies unless the host
    /// says it is something else.
    var isMacHost: Bool { hostPlatform == nil || hostPlatform == "macos" }
    /// Use the paired computer's name once its platform is known. Old hosts and
    /// the first status read must not assume that the computer is a Mac.
    var hostReference: String { hostPlatform == nil ? "your computer" : macName }
    /// Mid-sentence subject form of the same reference.
    var hostSubject: String { hostPlatform == nil ? "the computer" : macName }
    /// Keys this host and device's stored state so a replaced pairing never reuses it.
    var assignmentScope: String {
        guard let connection else { return "preview" }
        return connection.credential.hostInstallationId + ":" + connection.credential.deviceId
    }
    /// Changes when this pairing is replaced, retired or forgotten, unlike
    /// `assignmentScope`. A route move or session renewal keeps it.
    var pairingFence: String { assignmentScope + "|" + (partition ?? "") }
    /// Unsent work kept from chats that disappeared from the computer; see `OrphanedDraft`.
    @Published private(set) var orphanedDrafts: [OrphanedDraft] = []
    @Published private(set) var orphanedDraftError: String?
    /// The kept messages' file could not be read; nothing may replace it.
    private var orphanedDraftsUnreadable = false
    /// The canonical composer intents. Every change notifies the model's
    /// observers, except typing that leaves the draft's `ComposerDraftFacts`
    /// unchanged: that reaches only the chat's `draftText(for:)`, which its
    /// message editor observes, so a keystroke does not re-render the chat.
    var composers: [String: ComposerIntent] {
        get { composerIntents }
        set {
            objectWillChange.send()
            composerIntents = newValue
            for (chat, draft) in draftTexts where draft.text != newValue[chat]?.draft ?? "" { draft.text = newValue[chat]?.draft ?? "" }
        }
    }
    private var composerIntents: [String: ComposerIntent] = [:]
    private var draftTexts: [String: ComposerDraftText] = [:]
    /// The chat's typed text as its message editor observes it.
    func draftText(for chat: String) -> ComposerDraftText {
        if let draft = draftTexts[chat] { return draft }
        let draft = ComposerDraftText(composerIntents[chat]?.draft ?? "")
        draftTexts[chat] = draft
        return draft
    }
    #if WONDER_DIAGNOSTICS
    /// Lets hosted tests hold the real worker at this ownership boundary.
    var attachmentPersistenceWillPrepare: (() -> Void)?
    #endif
    @Published private(set) var staleAnnotationIDs: [String: Set<String>] = [:]
    private var staleAnnotationScope: String?
    private struct AnnotationRevisionKey: Hashable {
        let scope: String
        let chatID: String
        let rootID: String
        let path: String
    }
    private var annotationRevisionRequests: [AnnotationRevisionKey: UUID] = [:]
    @Published var composerErrors: [String: String] = [:]
    /// What history said about an edit the phone could not confirm; the chat shows it once.
    @Published var rewindResolutions: [String: String] = [:]
    @Published var sending: Set<String> = []
    @Published private(set) var preparingSends: Set<String> = []
    /// Sends waiting for dictation's final words. Send shows preparation and
    /// ignores another tap, while the draft still accepts the transcript.
    @Published private(set) var finishingDictation: Set<String> = []
    @Published var queues: [String: [QueuedMessage]] = [:] {
        didSet { for chat in awaitingAcceptance.keys { reconcileAcceptance(chat) } }
    }
    /// Sends decided as queued when they were made (a turn was running,
    /// earlier messages waited, or the Mac held the chat), by client message
    /// ID, until they start. The Mac's receipt confirms or corrects the
    /// decision; see `queuedClientIDs`.
    @Published private(set) var queuedSends: [String: Set<String>] = [:]
    /// Sends the agent has not taken yet, by chat and client message ID: a Send
    /// now, or a send that stops Claude's reply on the Mac. The message shows
    /// "Sending…" in the conversation until the Mac reports the agent took it,
    /// or it shows as Queued once the Mac puts it in the queue instead.
    @Published private(set) var awaitingAcceptance: [String: [String: SendAcceptance]] = [:]
    private var awaitingSince: [String: ContinuousClock.Instant] = [:]
    @Published var uploading: Set<String> = []
    /// Computer and thread pairs whose removed draft uploads are being forgotten.
    private var forgettingUploads: Set<String> = []
    @Published var loadingPhotos: Set<String> = []
    private var photoImportIDs: [String: UUID] = [:]
    @Published var files: [String: [ConversationFile]] = [:]
    @Published var savedDecisions: [String: DecisionIntent] = [:]
    @Published var asyncQuestions: [String: [AsyncQuestion]] = [:]
    @Published private(set) var retryableAsyncReplies: Set<String> = []
    @Published private(set) var savedAsyncReplies: [String: AsyncAnswerIntent] = [:]
    @Published var attention: [AttentionRequest] = [] { didSet { noteListChange() } }
    /// A reply finished in a chat, from the live stream (voice announces it
    /// for the chat it is pinned to). `key` is unique to the event.
    let replyFinished = PassthroughSubject<(conversation: String, key: String), Never>()
    @Published var attentionErrors: [String: String] = [:]
    /// A conversation to open with its pending request in front (an in-app banner was tapped).
    @Published var attentionRequestFocus: String?
    @Published var attentionFocus: String?
    @Published var resolving: Set<String> = []
    @Published var stopping: Set<String> = []
    /// How long Stop keeps showing progress after the Mac accepted it but the
    /// agent has not yet ended the turn (the Mac answers 202 until it does).
    var stopConfirmationWait: Duration = .seconds(30)
    @Published var controlErrors: [String: String] = [:]
    private var previewBytes: [String: Data] = [:]
    private var intentLoadFailures: Set<String> = []
    private let identity = PhoneIdentity()
    private let signingIdentity: SigningIdentity
    var identityNeedsRepair: (() throws -> Void)?
    var previousPairingConnection: ((String) -> SavedConnection?)?
    private let persistConnection: ((SavedConnection?) throws -> Void)?
    let api: PairingAPI
    private let homeNetworkTrust = HomeNetworkTrust()
    #if WONDER_DIAGNOSTICS
    private static func diagnosticAPI(trust: HomeNetworkTrust) -> PairingAPI { PairingAPI(serverTrustDelegate: trust) { network, decode, bytes, success in
        DiagnosticJournal.shared.record(DiagnosticEvent(operation: "network", phase: success ? "duration" : "failed", durationMs: network, bytes: UInt64(bytes)))
        DiagnosticJournal.shared.record(DiagnosticEvent(operation: "decode", durationMs: decode, bytes: UInt64(bytes)))
    } }
    #endif
    private var connectionCheck: Task<Void, Never>?
    /// The one session renewal running, keyed by the session it replaces;
    /// every caller that needs that session renewed joins it.
    private var renewal: (token: String, task: Task<SavedConnection?, Error>)?
    /// Renews the session ahead of its expiry while the live connection runs.
    private var renewalTimer: Task<Void, Never>?
    /// The one recovery from a request the computer refused for its session.
    private var accessRecovery: (id: UUID, task: Task<Void, Never>)?
    var recoveringAccess: Bool { accessRecovery != nil }
    /// The session the last recovery renewed to, and when. A request refused
    /// on it soon after is not recovered again (no renew, refuse, renew loop);
    /// once that session could have ended too, it is.
    private var recoveredSession: (token: String, at: ContinuousClock.Instant)?
    private func recentlyRecovered(_ token: String) -> Bool {
        guard let recoveredSession, recoveredSession.token == token else { return false }
        return ContinuousClock.now - recoveredSession.at < .seconds(60)
    }
    /// An expiry a renewal returned that was already inside the margin: the
    /// pairing itself ends then, so renewing again before it gains nothing.
    private var renewalCappedAtMs: UInt64?
    private var preparation: Task<Void, Never>?
    private var writer: ProjectionWriter?
    /// Reading positions, written off the main thread in order.
    private var intents: IntentWriter?
    /// Positions accepted this session. They win over the file, which a
    /// queued write may not have reached yet.
    private var savedPositions: [String: String] = [:]
    /// Reading positions whose last write failed, kept so Try again can
    /// write them again; a later successful write of that chat drops it.
    private var unsavedPositions: [String: String] = [:]
    private var cacheBackgroundTask: CacheBackgroundTask?
    private var loadingTokens: [String: UUID] = [:]
    private var enrollment: Task<Void, Never>?
    private var replay: Task<Void, Never>?
    @Published private var socket: (any LiveEventStream)? {
        didSet {
            if socket == nil {
                streamOwner = nil; streamConfirmedAt = nil
                if oldValue != nil { eventStreamBreaks &+= 1 }
            }
        }
    }
    /// A stream is evidence only for the route and identity that opened it.
    private var streamOwner: (run: UUID, saved: SavedConnection)?
    private var streamConfirmedAt: ContinuousClock.Instant?
    private var refusedStreamSession: String?

    private func isCurrentConnection(_ saved: SavedConnection, run: UUID) -> Bool {
        guard run == generation, !Task.isCancelled, !retiredAfterPairing,
              let current = connection, !current.requiresPairing else { return false }
        return current.origin == saved.origin && current.hostKeyPin == saved.hostKeyPin
            && current.credential.hostInstallationId == saved.credential.hostInstallationId
            && current.credential.deviceId == saved.credential.deviceId
            && current.credential.sessionToken == saved.credential.sessionToken
            && current.storageDeviceId == saved.storageDeviceId
    }

    private var currentStreamEligible: Bool {
        guard socket != nil, foreground, !accessEnded, !sessionLapsed,
              let owner = streamOwner, refusedStreamSession != owner.saved.credential.sessionToken else { return false }
        return isCurrentConnection(owner.saved, run: owner.run)
    }

    private var confirmedCurrentStream: Bool {
        guard currentStreamEligible, let at = streamConfirmedAt else { return false }
        return ContinuousClock.now - at <= ConnectionLiveness.quiet + ConnectionLiveness.deadline
    }

    /// Called only after a fresh, contiguous frame or an answered liveness check.
    /// The false -> true transition owns pending writes; subsequent frames do not
    /// restart a write's backoff or duplicate its in-flight request.
    @discardableResult private func confirmStreamActivity(_ active: any LiveEventStream, run: UUID) -> Bool {
        guard run == generation, active === socket, currentStreamEligible else { return false }
        streamConfirmedAt = .now
        recovery.succeeded()
        routeChecks.forgetMiss()
        setIfChanged(\.macConnected, true)
        setIfChanged(\.status, connectedStatus)
        setIfChanged(\.error, nil)
        noteLoss(nil)
        return true
    }

    /// The Mac's event stream is open, so its wake-ups (another app wrote a
    /// chat, work there started or ended) arrive without polling.
    var eventStreamLive: Bool { socket != nil }
    /// Times the live stream ended this launch. A route or session handoff
    /// keeps the old stream until the new one is live, so it is not an end.
    private var eventStreamBreaks: UInt64 = 0
    /// Per chat: the unbroken live stream (by `eventStreamBreaks`) its
    /// history was last read under. A stream that reopened may still be
    /// replaying what it missed, so history read before it ended is not
    /// current until it is read again.
    private var historyReadOnStream: [String: UInt64] = [:]
    #if WONDER_DIAGNOSTICS
    /// Tests: an open event stream that delivers nothing (replay is off in
    /// fixtures), so history the stream keeps current differs from a saved copy.
    func diagnosticAttachIdleEventStream() { socket = DiagnosticIdleEventStream() }
    /// Tests: the live stream ends and reopens, with what it missed not yet
    /// replayed.
    func diagnosticReopenIdleEventStream() {
        socket?.close(); socket = nil
        socket = DiagnosticIdleEventStream()
    }
    /// Tests: saved history is unverified, as after a cold load or a return
    /// from the background.
    func diagnosticMarkSavedHistoryStale() {
        projection.invalidateAll()
        cachedConversationIds = projection.dirty
    }
    #endif
    private var routeMonitor: NWPathMonitor?
    private var routeMonitorID = UUID()
    @Published private(set) var homeNetworkDenied = false
    private let homeNetworkAccess = HomeNetworkAccess()
    private var routeLocalNetworkAvailable = true
    private var routeTimer: Task<Void, Never>?
    private let routeChecks = ConnectionRouteChecks()
    /// Owns when an out-of-reach computer is tried again (see `ConnectionRecovery`).
    private let recovery = ConnectionRecovery(clock: ContinuousClock())
    /// A miss is being waited out; the replay loop tries again when it is due.
    var awaitingReconnect: Bool { recovery.isWaiting }
    /// The person asked to try again: the next attempt runs now, probing every route.
    func reconnectNow() {
        recovery.wake()
        routeChecks.forgetMiss()
    }
    /// The session's expiry has passed, so the computer ending its stream is expected.
    private var sessionLapsed: Bool {
        connection?.credential.expiresAtMs.map { $0 <= UInt64(Date().timeIntervalSince1970 * 1000) + 5_000 } ?? false
    }
    private func failureKind(_ error: Error) -> ConnectionFailure {
        ConnectionFailure.classify(error, sessionLapsed: sessionLapsed)
    }
    /// The last full route check found no usable route and at least one
    /// answered with another security key.
    private var routeKeyChanged = false
    let refreshCoordinator = RefreshCoordinator()
    private var eventListConversations: Set<String> = []
    private var eventListDiscovery = false
    private var nativeActivityRefreshes: Set<String> = []
    /// Events for the open conversation, so a read that lands after newer
    /// events catches up instead of being repeated.
    private var liveLog = LiveEventLog()
    private var generation = UUID()
    private var projection = ProjectionState()
    private var partition: String?
    private var retiredAfterPairing = false
    private var store: ReadStore?
    /// Whether this Mac's live connection runs (the app is in front, or voice talks).
    private(set) var foreground = false
    private var appInForeground = true
    /// The demo computer (`DemoMode`): served inside the app, never paired, never
    /// enrolled for notifications, presence or Live Activities.
    private(set) var isDemo = false
    #if WONDER_DIAGNOSTICS
    private var diagnosticReplayEnabled = true
    private var diagnosticRouteAPI: PairingAPI?
    /// Unit fixtures open live streams through this instead of a socket.
    private var diagnosticEventStreamOpener: (@MainActor (SavedConnection) async throws -> any LiveEventStream)?
    private var diagnosticRouteTask: Task<Void, Never>?
    @Published private var diagnosticRouteCheckpoint: String?
    private var previewProjectFilesRootReads = 0
    /// `-project-files-worktree-outside`: Add folder was tapped.
    private var previewWorktreeAdded = false
    private var previewWorkspaceRootReads = 0
    private var previewWorkspaceFileOpens: [String: Int] = [:]
    #endif
    var previewMode: Bool {
        #if DEBUG || WONDER_DIAGNOSTICS
        ProcessInfo.processInfo.arguments.contains("-read-preview") || ProcessInfo.processInfo.arguments.contains("-connections-preview")
        #else
        false
        #endif
    }
    init(saved: SavedConnection? = nil, persistConnection: ((SavedConnection?) throws -> Void)? = nil, api: PairingAPI? = nil, signingIdentity: SigningIdentity = PhoneIdentity.signing) {
        defer { homeNetworkTrust.update(connection) }
        self.signingIdentity = signingIdentity
        self.persistConnection = persistConnection
        #if WONDER_DIAGNOSTICS
        self.api = api ?? Self.diagnosticAPI(trust: homeNetworkTrust)
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-linux-host") {
            hostPlatform = "linux"
        }
        #else
        self.api = api ?? PairingAPI(serverTrustDelegate: homeNetworkTrust)
        #endif
        #if DEBUG || WONDER_DIAGNOSTICS
        if previewMode {
            // Explicit synthetic UI fixture. No pairing, network or production storage.
            // Seed Project details under the same host the preview library opens.
            connection = saved
            let arguments = ProcessInfo.processInfo.arguments
            #if WONDER_DIAGNOSTICS
            homeNetworkDenied = arguments.contains("-diagnostics-home-network-denied")
            #endif
            // The preview Mac has published its provider records, as a paired Mac has.
            projects.installPreviewFamilies(AgentFamily.allCases.map {
                ProjectFamilyAvailability(family: $0, available: true, capabilities: .fixtureMac($0))
            })
            // Claude fixtures: the task checklist, and Claude driving Chrome through Claude in Chrome.
            let previewFamily = arguments.contains("-task-checklist-preview") || arguments.contains("-handoff-voiceover-preview") ? "claude" : "codex"
            let composerAttachmentsPreview = arguments.contains("-composer-attachments-preview")
            let composerRestoredPreview = arguments.contains("-composer-restored-attachments-preview")
            let composerRunningPreview = arguments.contains("-composer-running-preview")
            let composerQueuedPreview = arguments.contains("-composer-queued-preview")
            let messageAttachmentsPreview = arguments.contains("-message-attachments-preview")
            let projectRunningElsewherePreview = arguments.contains("-project-running-elsewhere-preview")
            // With it: a Mac that stops its reply for a message sent now.
            if arguments.contains("-project-mac-reply-stops-preview") { nativeStopsMacReply.insert("preview") }
            // A Claude chat open, idle, in Claude on the Mac, with a message waiting for it.
            let projectOpenOnMacPreview = arguments.contains("-project-open-on-mac-preview")
            if projectOpenOnMacPreview { nativeOpenElsewhere.insert("preview") }
            // The same, while Claude on the Mac still runs agent tasks after its reply.
            let projectTasksOnMacPreview = arguments.contains("-project-tasks-on-mac-preview")
            if projectTasksOnMacPreview { nativeTasksOnMac.insert("preview") }
            let projectTerminalTurnPreview = arguments.contains("-project-terminal-turn-preview") || projectRunningElsewherePreview
            let previewTitle = saved?.hostName == "Laptop" ? "Travel plans" : saved?.hostName == "Home" ? "Reading list" : "Weekend plans"
            var previewMessages: [[String: Any]] = [
                ["messageId": "1", "body": "Can you help me plan a relaxed Saturday?", "state": "completed", "createdAt": "1700000000000", "attachmentIds": []],
                ["messageId": "3", "body": "Sounds good. Keep the evening free too.", "state": "completed", "createdAt": "1700000002000", "attachmentIds": []]
            ]
            var previewReplies: [[String: Any]] = [
                ["messageId": "2", "text": "Start with breakfast at home, then take a walk. Leave the afternoon open so the day stays flexible.", "state": "completed", "createdAt": "1700000001000", "updatedAt": "1700000001000"],
                ["messageId": "4", "text": "A quiet evening it is. You can return to these notes even when your computer is offline.", "state": "completed", "createdAt": "1700000003000", "updatedAt": "1700000003000"]
            ]
            if arguments.contains("-scroll-performance-preview") {
                previewMessages = []; previewReplies = []
                for index in 1...300 {
                    let timestamp = String(1_700_000_000_000 + index * 1000)
                    if index.isMultiple(of: 2) {
                        previewReplies.append(["messageId": "perf-\(index)", "text": "Message \(index)\n\nA repeatable conversation for scrolling measurements. **Readable text** and a [link](https://example.com) exercise the normal message renderer.\n\n- Keep existing messages stable.\n- Return to the latest message when requested.\n\nThis final paragraph makes each response tall enough to scroll through at a natural reading pace.", "state": "completed", "createdAt": timestamp, "updatedAt": timestamp])
                    } else {
                        previewMessages.append(["messageId": "perf-\(index)", "body": "Question \(index): How does this conversation behave while scrolling?", "state": "completed", "createdAt": timestamp, "attachmentIds": []])
                    }
                }
            }
            if arguments.contains("-layout-reachability-preview") {
                // Keep the gesture target fully visible with either device's
                // keyboard. A long performance history can leave its last AX
                // element under the composer even though the tree still lists it.
                previewMessages = [["messageId": "layout-user", "body": "Check composer reachability.",
                                    "state": "completed", "createdAt": "1700000000000", "attachmentIds": []]]
                previewReplies = [["messageId": "layout-reply",
                                   "text": "Layout reply\n\nSelect these words or open the [link](https://example.com).",
                                   "state": "completed", "createdAt": "1700000001000", "updatedAt": "1700000001000"]]
            }
            var previewSnapshot: [String: Any] = ["conversationId": "preview", "hostEpoch": "preview", "lastSequence": 1,
                "messages": previewMessages, "assistantMessages": previewReplies, "thread": ["hydrated": true]]
            if arguments.contains("-layout-reachability-preview") {
                previewSnapshot["assistantMessages"] = []
                previewSnapshot["thread"] = ["hydrated": true, "turns": [["id": "layout-turn", "status": "completed", "items": [
                    ["id": "layout-command", "type": "commandExecution", "state": "completed", "createdAt": "1700000000500",
                     "payload": ["command": "fixture layout check", "exitCode": 0, "output": "Layout disclosure opened."]],
                    ["id": "layout-reply", "type": "agentMessage", "state": "completed", "createdAt": "1700000001000",
                     "text": "Layout reply\n\nSelect these words or open the [link](https://example.com)."]
                ]]]]
            }
            if arguments.contains("-handoff-voiceover-preview") {
                previewSnapshot["messages"] = []
                previewSnapshot["assistantMessages"] = []
                previewSnapshot["thread"] = ["hydrated": true, "turns": [["id": "handoff", "status": "completed", "items": [
                    ["id": "before", "type": "agentMessage", "state": "completed", "createdAt": "1700000000000", "text": "I checked the implementation."],
                    ["id": "switch", "type": "providerSwitch", "state": "completed", "createdAt": "1700000001000",
                     "text": "Switched to Claude Sonnet · Codex history was handed over", "payload": ["fromFamily": "codex", "toFamily": "claude"]],
                    ["id": "after", "type": "agentMessage", "state": "completed", "createdAt": "1700000002000", "text": "I reviewed the changes."]
                ]]]]
            }
            if let data = try? JSONSerialization.data(withJSONObject: previewSnapshot),
               let value = try? JSONDecoder().decode(ConversationSnapshot.self, from: data) {
                snapshots = ["preview": value]
                chats = [ChatSummary(conversationId: "preview", title: previewTitle,
                    lastMessagePreview: nil, lastMessageAt: nil, messageCount: previewMessages.count + previewReplies.count,
                    deliveryState: nil, hasUnread: false, isArchived: false, isPinned: false)]
                let project: [String: Any] = ["conversationId": "preview", "projectId": "preview-project", "projectName": "Preview project",
                    "title": previewTitle, "family": previewFamily, "accessMode": "workspace", "workingFolder": "/preview", "workingFolderName": "Preview",
                    "isPinned": false, "hasUnread": false, "hasNativeSession": true, "folderInProject": true]
                if let data = try? JSONSerialization.data(withJSONObject: project),
                   let detail = try? JSONDecoder().decode(ProjectConversationDetail.self, from: data) {
                    #if WONDER_DIAGNOSTICS
                    projects.installPreviewFilesConversation(detail)
                    #endif
                }
                macConnected = !ProcessInfo.processInfo.arguments.contains("-disconnected-preview")
                cachedConversationIds = ["preview"]
                if ProcessInfo.processInfo.arguments.contains("-send-preview") ||
                    composerAttachmentsPreview || composerRestoredPreview || composerRunningPreview || composerQueuedPreview || messageAttachmentsPreview || projectTerminalTurnPreview ||
                    (ProcessInfo.processInfo.arguments.contains("-connections-preview") && saved?.hostName == "Studio") {
                    var thread: [String: Any] = composerRunningPreview
                        ? ["hydrated": true, "turns": [[
                            "id": "fixture-turn", "status": "inProgress", "startedAt": "1700000000000", "items": []
                        ]]]
                        : ["hydrated": true]
                    if projectRunningElsewherePreview {
                        // A turn the Claude desktop app is running on the Mac.
                        thread = ["hydrated": true, "turns": [[
                            "id": "fixture-turn", "status": "inProgress", "runningElsewhere": true, "items": [[
                                "id": "desktop-command", "type": "commandExecution", "state": "started",
                                "createdAt": "1700000001000", "payload": ["command": "swift test"]
                            ]]
                        ]]]
                    } else if projectTerminalTurnPreview {
                        thread = ["hydrated": true, "turns": [[
                            "id": "fixture-turn", "status": "interrupted", "items": [[
                                "id": "stopped-command", "type": "commandExecution", "state": "interrupted",
                                "createdAt": "1700000001000", "payload": ["command": "swift test"]
                            ], [
                                "id": "compact-stopped", "type": "contextCompaction", "state": "interrupted",
                                "createdAt": "1700000002000"
                            ]]
                        ]]]
                    }
                    var assistantMessages: [[String: Any]] = [[
                        "messageId": "2", "codexTurnId": "fixture-turn", "itemId": "fixture-item",
                        "text": "Start with breakfast at home, then take a walk. Leave the afternoon open so the day stays flexible.",
                        "state": "completed", "createdAt": "1700000001000", "updatedAt": "1700000001000"
                    ]]
                    if projectTerminalTurnPreview { assistantMessages = [] }
                    #if WONDER_DIAGNOSTICS
                    if DiagnosticDevServerFixture.servers != nil {
                        // Loopback links a reply names: 5173 runs for this thread, 3000 does not.
                        assistantMessages.append([
                            "messageId": "3", "codexTurnId": "fixture-turn", "itemId": "fixture-links",
                            "text": "The planner is at [localhost:5173](http://localhost:5173/?view=plan) and its API at [localhost:3000](http://localhost:3000/docs).",
                            "state": "completed", "createdAt": "1700000002000", "updatedAt": "1700000002000"
                        ])
                    }
                    #endif
                    #if WONDER_DIAGNOSTICS
                    if arguments.contains("-browser-reading-preview") {
                        assistantMessages = (1...60).map { index in
                            let timestamp = String(1_700_000_000_000 + index * 1000)
                            return ["messageId": "browser-reading-\(index)", "codexTurnId": "reading-\(index)",
                                    "itemId": "item-\(index)", "state": "completed", "createdAt": timestamp, "updatedAt": timestamp,
                                    "text": "Reading note \(index)\n\nKeep this older reply in place while a page is open. The chat has sixty notes, so Done must not jump to the latest one.\n\nOpen the [planner](http://localhost:5173/?view=plan) or a [web page](https://example.com)."]
                        }
                        savedPositions["preview"] = "reading-20/item-20"
                    }
                    #endif
                    if composerQueuedPreview {
                        assistantMessages = []
                        for index in 1...14 {
                            let timestamp = String(1_700_000_001_000 + index * 1000)
                            assistantMessages.append([
                                "messageId": "queued-fixture-\(index)", "codexTurnId": "fixture-turn-\(index)",
                                "itemId": "fixture-item-\(index)",
                                "text": "Planning note \(index): keep the conversation readable while a queued message remains anchored after the history.",
                                "state": "completed", "createdAt": timestamp, "updatedAt": timestamp
                            ])
                        }
                    }
                    let fixture: [String: Any] = [
                        "conversationId": "preview", "hostEpoch": "preview", "lastSequence": 1,
                        "messages": [["messageId": "1", "body": messageAttachmentsPreview ? "Here are the reference images and notes." : "Can you help me plan a relaxed Saturday?", "state": (ProcessInfo.processInfo.arguments.contains("-active-preview") || composerRunningPreview) ? "streaming" : "completed", "codexTurnId": "fixture-turn", "codexThreadId": "fixture-thread", "createdAt": "1700000000000", "attachmentIds": messageAttachmentsPreview ? ["message-image-1", "message-image-2", "message-notes"] : []]]
                            + (projectOpenOnMacPreview ? [["messageId": "2", "body": "Also check the tests.", "state": "accepted_by_wonder", "createdAt": "1700000002000", "attachmentIds": [String]()]] : []),
                        "assistantMessages": assistantMessages,
                        "thread": thread
                    ]
                    let summary: [String: Any] = ["conversationId": "preview", "title": "Project notes", "lastMessagePreview": "Leave the afternoon open so the day stays flexible.", "messageCount": 2, "hasUnread": false, "isArchived": false, "isPinned": false]
                    if let data = try? JSONSerialization.data(withJSONObject: fixture),
                       let snapshot = try? JSONDecoder().decode(ConversationSnapshot.self, from: data),
                       let data = try? JSONSerialization.data(withJSONObject: summary),
                       let chat = try? JSONDecoder().decode(ChatSummary.self, from: data) {
                        snapshots = ["preview": snapshot]; chats = [chat]
                        do {
                            let projectDetail: [String: Any] = [
                                "conversationId": "preview", "projectId": "preview-project", "projectName": "Preview project",
                                "title": "Project notes", "family": previewFamily, "accessMode": "workspace",
                                "workingFolder": "/preview", "workingFolderName": "Preview", "isPinned": false,
                                "hasUnread": false, "hasNativeSession": true, "folderInProject": true
                            ]
                            if let data = try? JSONSerialization.data(withJSONObject: projectDetail),
                               let detail = try? JSONDecoder().decode(ProjectConversationDetail.self, from: data) {
                                #if WONDER_DIAGNOSTICS
                                projects.installPreviewFilesConversation(detail)
                                #endif
                            }
                            #if WONDER_DIAGNOSTICS
                            if let summary = DiagnosticGitFixture.summary { gitSummaries["preview"] = summary }
                            // The preview never loads the Git summary, so the pill's own-thread count is seeded too.
                            if let changes = DiagnosticGitFixture.threadChanges(location: nil) {
                                threadChangeCounts["preview"] = ThreadChangeCount(changes)
                            }
                            if let servers = DiagnosticDevServerFixture.servers {
                                var presence = DevServerPresence(); presence.update(servers, at: Date())
                                devServers["preview"] = presence
                            }
                            if let index = DiagnosticGitFixture.repositories { threadRepositories["preview"] = index }
                            #endif
                            if arguments.contains("-project-goal-preview"),
                               let goal = try? JSONDecoder().decode(ConversationGoal.self, from: Data(#"{"objective":"Review the Project plan","status":"active","timeBudgetSeconds":600,"timeUsedSeconds":60}"#.utf8)) {
                                goals["preview"] = goal
                            }
                        }
                        var intent = ComposerIntent(); intent.draft = "Keep the evening free too."
                        if ProcessInfo.processInfo.arguments.contains("-send-unconfirmed-preview") {
                            try? intent.begin(device: "preview")
                            intent.draft = "Tomorrow works too."
                        }
                        if ProcessInfo.processInfo.arguments.contains("-files-preview") || composerAttachmentsPreview || composerQueuedPreview {
                            intent.stagedFiles = [try! StagedFile(id: "composer-file", name: "weekend-notes.txt", mimeType: "text/plain", data: Data("Keep Saturday relaxed. Leave Sunday free.".utf8))]
                            let pdf = UIGraphicsPDFRenderer(bounds: CGRect(x: 0, y: 0, width: 300, height: 420)).pdfData { context in
                                context.beginPage()
                                ("Weekend notes\n\nSaturday: breakfast and a walk.\nSunday: free time." as NSString).draw(in: CGRect(x: 24, y: 24, width: 252, height: 360), withAttributes: [.font: UIFont.systemFont(ofSize: 16)])
                                if arguments.contains("-artifact-annotation-preview") {
                                    context.beginPage()
                                    ("Second page\n\nReview the schedule." as NSString).draw(in: CGRect(x: 24, y: 24, width: 252, height: 360), withAttributes: [.font: UIFont.systemFont(ofSize: 16)])
                                }
                            }
                            let image = UIGraphicsImageRenderer(size: CGSize(width: 300, height: 180)).pngData { context in
                                UIColor.systemTeal.setFill(); context.fill(CGRect(x: 0,y: 0,width: 300,height: 180))
                                ("Saturday" as NSString).draw(at: CGPoint(x: 24,y: 70),withAttributes: [.font:UIFont.systemFont(ofSize: 28),.foregroundColor:UIColor.white])
                            }
                            let samples: [(String,String,Data)] = [("notes.pdf","application/pdf",pdf),("Saturday.png","image/png",image),("notes.txt","text/plain",Data("Saturday: breakfast and a walk. Sunday: free time.".utf8)),("planner.html","text/html",Data("<h1>Weekend planner</h1><button onclick=\"this.textContent='Saturday selected'\">Choose Saturday</button>".utf8))]
                            var list: [ConversationFile] = []
                            for (name,mime,data) in samples {
                                let value: [String:Any] = ["id":name,"name":name,"mimeType":mime,"byteSize":data.count,"sha256":ConversationFile.digest(data),"state":"available","updatedAt":"Synthetic fixture"]
                                if let file = try? JSONDecoder().decode(ConversationFile.self,from: JSONSerialization.data(withJSONObject:value)) { list.append(file); previewBytes[file.id] = data }
                            }
                            if arguments.contains("-preview-malformed-image") {
                                let data = Data("not an image".utf8)
                                let value: [String:Any] = ["id":"broken.png","name":"broken.png","mimeType":"image/png","byteSize":data.count,"sha256":ConversationFile.digest(data),"state":"available","updatedAt":"Synthetic fixture"]
                                if let file = try? JSONDecoder().decode(ConversationFile.self,from:JSONSerialization.data(withJSONObject:value)) { list.append(file); previewBytes[file.id] = data }
                            }
                            files["preview"] = list
                        }
                        if composerAttachmentsPreview {
                            let image = UIGraphicsImageRenderer(size: CGSize(width: 360, height: 240)).pngData { context in
                                UIColor.systemIndigo.setFill(); context.fill(CGRect(x: 0, y: 0, width: 360, height: 240))
                                UIColor.white.setFill(); context.fill(CGRect(x: 36, y: 42, width: 288, height: 126))
                                UIColor.systemIndigo.setFill(); context.fill(CGRect(x: 58, y: 64, width: 100, height: 82))
                                UIColor.systemTeal.setFill(); context.fill(CGRect(x: 176, y: 64, width: 126, height: 82))
                            }
                            intent.stagedFiles = [
                                try! StagedFile(id: "composer-photo", name: "Saturday-plan.png", mimeType: "image/png", data: image),
                                try! StagedFile(id: "composer-file", name: "weekend-notes.txt", mimeType: "text/plain", data: Data("Keep Saturday relaxed. Leave Sunday free.".utf8))
                            ]
                        }
                        if composerRestoredPreview {
                            intent.stagedFiles = nil
                            intent.draftAttachmentIds = ["restored-photo", "missing-restored-file"]
                            let restoredImage = UIGraphicsImageRenderer(size: CGSize(width: 360, height: 240)).pngData { context in
                                UIColor.systemOrange.setFill(); context.fill(CGRect(x: 0, y: 0, width: 360, height: 240))
                                UIColor.white.setFill(); context.fill(CGRect(x: 28, y: 28, width: 304, height: 184))
                                UIColor.systemOrange.setFill(); context.fill(CGRect(x: 52, y: 52, width: 108, height: 136))
                                UIColor.systemPink.setFill(); context.fill(CGRect(x: 180, y: 52, width: 128, height: 136))
                            }
                            previewBytes["restored-photo"] = restoredImage
                            let metadata: [[String: Any]] = [
                                ["id": "restored-photo", "name": "Saturday-plan.png", "mimeType": "image/png", "byteSize": restoredImage.count, "sha256": ConversationFile.digest(restoredImage), "state": "available", "updatedAt": "Synthetic fixture" ]
                            ]
                            files["preview"] = metadata.compactMap { value in
                                try? JSONDecoder().decode(ConversationFile.self, from: JSONSerialization.data(withJSONObject: value))
                            }
                        }
                        if messageAttachmentsPreview {
                            func fixtureImage(_ background: UIColor, _ foreground: UIColor, _ title: String) -> Data {
                                UIGraphicsImageRenderer(size: CGSize(width: 360, height: 240)).pngData { context in
                                    background.setFill(); context.fill(CGRect(x: 0, y: 0, width: 360, height: 240))
                                    foreground.setFill(); context.fill(CGRect(x: 28, y: 28, width: 304, height: 184))
                                    (title as NSString).draw(at: CGPoint(x: 48, y: 98), withAttributes: [
                                        .font: UIFont.systemFont(ofSize: 26, weight: .semibold),
                                        .foregroundColor: background
                                    ])
                                }
                            }
                            let samples: [(String, String, Data)] = [
                                ("message-image-1", "reference-one.png", fixtureImage(.systemIndigo, .white, "Reference one")),
                                ("message-image-2", "reference-two.png", fixtureImage(.systemTeal, .white, "Reference two")),
                                ("message-notes", "reference-notes.txt", Data("Keep the two reference images together.".utf8))
                            ]
                            var list: [ConversationFile] = []
                            for (id, name, data) in samples {
                                let mime = name.hasSuffix(".png") ? "image/png" : "text/plain"
                                let value: [String: Any] = ["id": id, "name": name, "mimeType": mime, "byteSize": data.count,
                                    "sha256": ConversationFile.digest(data), "state": "available", "updatedAt": "Synthetic message fixture"]
                                if let file = try? JSONDecoder().decode(ConversationFile.self, from: JSONSerialization.data(withJSONObject: value)) {
                                    list.append(file)
                                    previewBytes[file.id] = data
                                }
                            }
                            files["preview"] = list
                        }
                        composers["preview"] = intent
                    }
                }
            }
            if ProcessInfo.processInfo.arguments.contains("-queue-preview") || composerQueuedPreview {
                let json = """
                [{"id":"one","clientMessageId":"one","body":"Find a walking route for Saturday.","revision":2,"attachmentIds":[]},{"id":"two","clientMessageId":"two","body":"Summarize the plan in the attached notes.","revision":1,"attachmentIds":["Saturday.png","notes.txt"]}]
                """
                queues["preview"] = try? JSONDecoder().decode([QueuedMessage].self,from:Data(json.utf8))
            }
            if ProcessInfo.processInfo.arguments.contains("-async-question-preview") || ProcessInfo.processInfo.arguments.contains("-answered-question-preview") {
                var fixture: [String:Any] = ["id":"async-fixture", "conversationId":"preview", "turnId":"fixture-turn", "itemId":"fixture-item", "questions":[["title":"Which day works best?", "options":["Saturday", "Sunday"]]], "state":"pending", "expiresAtMs":UInt64(Date().timeIntervalSince1970 * 1000) + 300_000]
                if ProcessInfo.processInfo.arguments.contains("-answered-question-preview") {
                    fixture["state"] = "answered"
                    fixture["response"] = ["answers":["Saturday"], "skip":false]
                }
                if let data = try? JSONSerialization.data(withJSONObject: fixture), let value = try? JSONDecoder().decode(AsyncQuestion.self,from:data) { asyncQuestions["preview"] = [value] }
            }
            if arguments.contains("-native-question-history-preview") {
                let reply = #"<send_user_message_question_reply>[{"questionItemId":"[\"request_user_input_async\",\"native-history-question\",0]","question":"Which day works best?","answer":"Saturday"},{"questionItemId":"[\"request_user_input_async\",\"native-history-question\",1]","question":"What should we check?","answer":"Navigation"}]</send_user_message_question_reply>"#
                let fixture: [String: Any] = ["conversationId":"preview", "hostEpoch":"preview", "lastSequence":1,
                    "messages":[], "assistantMessages":[], "thread":["hydrated":true, "turns":[["id":"native-history-turn", "status":"completed", "items":[
                        ["id":"native-history-question", "type":"agentMessage", "state":"completed", "text":"Which day works best?", "createdAt":"1000", "payload":["delivery":"async", "questions":[["title":"Which day works best?", "options":["Saturday", "Sunday"]], ["title":"What should we check?"]]]],
                        ["id":"native-history-answer", "type":"userMessage", "state":"completed", "text":reply, "createdAt":"2000"],
                        ["id":"native-history-final", "type":"agentMessage", "state":"completed", "text":"The saved reply is available.", "createdAt":"3000"]
                    ]]]]]
                if let data = try? JSONSerialization.data(withJSONObject: fixture),
                   let snapshot = try? JSONDecoder().decode(ConversationSnapshot.self, from: data) {
                    snapshots["preview"] = snapshot
                }
            }
            if arguments.contains("-computer-agent-preview") {
                // A running turn that is using two apps through cua_repl.
                let fixture: [String: Any] = ["conversationId":"preview", "hostEpoch":"preview", "lastSequence":1,
                    "messages":[], "assistantMessages":[], "thread":["hydrated":true, "turns":[["id":"computer-agent-turn", "status":"inProgress", "items":[
                        ["id":"computer-agent-ask", "type":"userMessage", "state":"completed", "text":"Draft the trip notes in TextEdit, then check the route in Safari.", "createdAt":"1000"],
                        ["id":"computer-agent-textedit", "type":"mcpToolCall", "state":"completed", "createdAt":String(Int(Date().timeIntervalSince1970 * 1000) - 2000), "payload":["server":"cua_repl", "tool":"js", "arguments":["code":"await notes.getState()"], "result":["_meta":["codex/toolSurface":["kind":"computerUse", "app":["kind":"appId", "appId":"com.apple.TextEdit"]]]]]],
                        ["id":"computer-agent-safari", "type":"mcpToolCall", "state":"streaming", "createdAt":String(Int(Date().timeIntervalSince1970 * 1000)), "payload":["server":"cua_repl", "tool":"js", "arguments":["code":"await safari.getState()"], "result":["_meta":["codex/toolSurface":["kind":"computerUse", "app":["kind":"appId", "appId":"com.apple.Safari"]]]]]]
                    ]]]]]
                if let data = try? JSONSerialization.data(withJSONObject: fixture),
                   let snapshot = try? JSONDecoder().decode(ConversationSnapshot.self, from: data) {
                    snapshots["preview"] = snapshot
                }
            }
            if arguments.contains("-computer-approval-preview") {
                let fixture: [String: Any] = ["approvalId":"fixture-computer", "conversationId":"preview", "method":"item/tool/call", "actionNonce":"fixture", "params":["threadId":"fixture-thread", "turnId":"fixture-turn", "tool":"wonder_computer_use", "arguments":["action":"screenshot"]]]
                if let data = try? JSONSerialization.data(withJSONObject: fixture), let value = try? JSONDecoder().decode(AttentionRequest.self, from: data) { attention = [value] }
            }
            var arrivingBehindQuestion: [AttentionRequest] = []
            if let index = arguments.firstIndex(of: "-phone-approval-preview"), arguments.indices.contains(index + 1) {
                let fixtures: [String: String] = [
                    "command": #"{"approvalId":"fixture-command","conversationId":"preview","method":"item/commandExecution/requestApproval","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","command":"convert original.png output.png","availableDecisions":["accept","acceptForSession",{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["convert"]}},"decline","cancel"]}}"#,
                    "network": #"{"approvalId":"fixture-network","conversationId":"preview","method":"item/commandExecution/requestApproval","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","networkApprovalContext":{"host":"example.com","protocol":"https"},"availableDecisions":["accept",{"applyNetworkPolicyAmendment":{"network_policy_amendment":{"host":"example.com","action":"allow"}}},"decline"]}}"#,
                    "file": #"{"approvalId":"fixture-file","conversationId":"preview","method":"item/fileChange/requestApproval","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","grantRoot":"/Users/example/Movies","reason":"Save the upscaled copy."}}"#,
                    "file-diff": #"{"approvalId":"fixture-file-diff","conversationId":"preview","method":"item/fileChange/requestApproval","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","reason":"Rename the export title.","changes":[{"path":"/Users/example/Movies/notes.md","kind":"update","diff":"@@ -1,3 +1,3 @@\n # Upscale notes\n-Keep the original.\n+Keep the original copy.\n Done."}]}}"#,
                    "file-long": #"{"approvalId":"fixture-file-long","conversationId":"preview","method":"item/fileChange/requestApproval","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","changes":[{"path":"/Users/example/Movies/big.md","kind":"update","diff":"@@ -1,20 +1,20 @@\n-a1\n+b1\n-a2\n+b2\n-a3\n+b3\n-a4\n+b4\n-a5\n+b5\n-a6\n+b6\n-a7\n+b7\n-a8\n+b8\n-a9\n+b9\n-a10\n+b10"}]}}"#,
                    "permissions": #"{"approvalId":"fixture-permissions","conversationId":"preview","method":"item/permissions/requestApproval","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","permissions":{"fileSystem":{"read":["/Users/example/Movies"]},"network":{"enabled":true}}}}"#,
                    "form": #"{"approvalId":"fixture-form","conversationId":"preview","method":"mcpServer/elicitation/request","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","mode":"form","serverName":"Example","message":"Choose export settings.","requestedSchema":{"type":"object","properties":{"quality":{"type":"string","title":"Quality","enum":["Standard","High"]},"copies":{"type":"integer","title":"Copies","minimum":1,"maximum":5},"watermark":{"type":"boolean","title":"Watermark"}},"required":["quality","copies"]}}}"#,
                    "native": #"{"approvalId":"fixture-native","conversationId":"preview","method":"mcpServer/elicitation/request","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","mode":"form","message":"Allow Computer Use to use Calculator?","requestedSchema":{"type":"object","properties":{}},"elicitationContext":{"isComputerUse":true,"riskLevel":"low","details":["App: Calculator"]}}}"#,
                    "url": #"{"approvalId":"fixture-url","conversationId":"preview","method":"mcpServer/elicitation/request","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","mode":"url","message":"Connect the export service.","url":"https://example.com/authorize"}}"#,
                    "tool": ##"{"approvalId":"fixture-tool","conversationId":"preview","method":"item/commandExecution/requestApproval","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","kind":"tool","command":"Write: {\"file_path\":\"/Users/example/Movies/notes.md\",\"content\":\"# Upscale notes\\nKeep the original.\\n\"}","cwd":"/Users/example/Movies","reason":"Claude wants to write notes.md","availableDecisions":["accept","decline","cancel"]}}"##,
                    "secret": #"{"approvalId":"secret:fixture","conversationId":"preview","method":"wonder/requestSecret","actionNonce":"fixture","params":{"secretRequest":{"label":"GitHub token","purpose":"Push the release branch to the private repo.","destination":"In the Keychain on your computer as “github-release-token”","expiresAtMs":4102444800000}}}"#,
                    "unknown": #"{"approvalId":"fixture-unknown","conversationId":"preview","method":"item/tool/call","actionNonce":"fixture","params":{"threadId":"fixture-thread","turnId":"fixture-turn","tool":"unknown_tool","arguments":{"secret":"not exposed"}}}"#,
                ]
                if let json = fixtures[arguments[index + 1]], let value = try? JSONDecoder().decode(AttentionRequest.self, from: Data(json.utf8)) {
                    attention = [value]
                    // A reply that was sent but not confirmed shows the choice it made.
                    if arguments.contains("-phone-approval-saved-preview"), let choice = value.phoneChoices.first {
                        savedDecisions[value.id] = DecisionIntent(request: value, decision: choice.decision,
                            responseJson: try? value.responseJSON(for: choice, answers: [:]), structuredDecision: choice.structuredDecision)
                    }
                }
            }
            if ProcessInfo.processInfo.arguments.contains("-question-preview") {
                let json = """
                {"approvalId":"fixture-question","conversationId":"preview","method":"item/tool/requestUserInput","actionNonce":"fixture","params":{"threadId":"fixture-thread","isBlocking":false,"questions":[{"id":"day","question":"Which day works best?","options":[{"label":"Saturday","description":"Keep Sunday free."},{"label":"Sunday"}]},{"id":"fixtures","question":"Which fixtures should I check?","multiSelect":true,"options":[{"label":"A, B"},{"label":"C"}]}]}}
                """
                if let value = try? JSONDecoder().decode(AttentionRequest.self, from: Data(json.utf8)) {
                    // With both previews and an arrival, the question is open and the permission arrives later.
                    arrivingBehindQuestion = arguments.contains("-attention-arrives-preview") ? attention : []
                    attention = [value]
                }
            }
            if arguments.contains("-attention-arrives-preview") {
                // The request arrives while the conversation is already open.
                let behindQuestion = !arrivingBehindQuestion.isEmpty
                let arriving = attention
                let second = arrivingBehindQuestion
                attention = []
                Task { [weak self] in
                    #if WONDER_DIAGNOSTICS
                    let held = arguments.contains("-diagnostics-attention-arrival-held")
                    while held && !DiagnosticSubagentFixture.signalled("attention-arrive") {
                        do { try await Task.sleep(for: .milliseconds(100)) }
                        catch { return }
                    }
                    if !held { try? await Task.sleep(for: .seconds(3)) }
                    #else
                    try? await Task.sleep(for: .seconds(3))
                    #endif
                    guard !Task.isCancelled else { return }
                    self?.attention = arriving
                    #if WONDER_DIAGNOSTICS
                    // The permission arrives behind the question once the second signal fires.
                    if behindQuestion {
                        while !DiagnosticSubagentFixture.signalled("attention-arrive-second") {
                            do { try await Task.sleep(for: .milliseconds(100)) }
                            catch { return }
                        }
                        self?.attention = arriving + second
                    }
                    #endif
                }
            }
            if arguments.contains("-task-checklist-preview") {
                snapshots["preview"] = DiagnosticTaskChecklistFixture.snapshot(legacyTodoWrite: arguments.contains("-task-checklist-legacy-preview"))
            }
            if arguments.contains("-diagnostics-genui") {
                let index = arguments.firstIndex(of: "-diagnostics-genui-component")
                let component = index.flatMap { arguments.indices.contains($0 + 1) ? arguments[$0 + 1] : nil }
                snapshots["preview"] = DiagnosticNativeUIFixture.snapshot(component: component)
                composers["preview"] = ComposerIntent()
            }
            if arguments.contains("-tool-actions-preview") {
                snapshots["preview"] = DiagnosticToolActionFixture.snapshot(running: arguments.contains("-activity-running-preview"))
            }
            if arguments.contains("-html-render-preview") {
                let fixture = DiagnosticPublishedPageFixture.make()
                snapshots["preview"] = fixture.snapshot
                previewBytes.merge(fixture.bytes) { _, new in new }
            }
            if arguments.contains("-visual-tool-preview") {
                let fixture = DiagnosticShownVisualFixture.make()
                snapshots["preview"] = fixture.snapshot
                previewBytes.merge(fixture.bytes) { _, new in new }
            }
            if ProcessInfo.processInfo.arguments.contains("-activity-preview") {
                let failed = ProcessInfo.processInfo.arguments.contains("-activity-failure-preview")
                let commandFailed = ProcessInfo.processInfo.arguments.contains("-activity-command-failure-preview")
                let running = ProcessInfo.processInfo.arguments.contains("-activity-running-preview")
                let mixed = ProcessInfo.processInfo.arguments.contains("-activity-mixed-preview")
                var finalPayload: [String: Any] = ["phase":"final_answer"]
                var toolPayload: [String: Any] = failed
                    ? ["tool":"fetch_document", "error":"The document service is unavailable. Try again later."]
                    : ["tool":"fetch_document", "arguments":["document":"Project notes"], "result":["text":"Keep the chat compact and accessible."]]
                if mixed {
                    let data = UIGraphicsImageRenderer(size: CGSize(width: 480, height: 240)).pngData { context in
                        UIColor.secondarySystemBackground.setFill(); context.fill(CGRect(x: 0, y: 0, width: 480, height: 240)) // theme-exempt: draws a fixture image, not UI
                        ("Project notes" as NSString).draw(at: CGPoint(x: 24, y: 24), withAttributes: [.font: UIFont.systemFont(ofSize: 24, weight: .semibold), .foregroundColor: UIColor.label]) // theme-exempt: draws a fixture image, not UI
                        for (index, height) in [55, 92, 126, 105, 142].enumerated() {
                            UIColor.systemTeal.setFill(); context.fill(CGRect(x: 32 + index * 84, y: 210 - height, width: 44, height: height))
                        }
                    }
                    let file: [String: Any] = ["id":"tool-preview", "name":"Project notes.png", "mimeType":"image/png", "byteSize":data.count, "sha256":ConversationFile.digest(data), "state":"available", "updatedAt":"Synthetic fixture",
                                               "width":480, "height":240]
                    previewBytes["tool-preview"] = data
                    toolPayload["result"] = ["content":[["type":"text", "text":"Found a chart in the project notes."], ["type":"wonderArtifact", "file":file]]]
                    if ProcessInfo.processInfo.arguments.contains("-activity-gallery-preview") {
                        // Pictures one after another, each with the pixel size the Mac reports.
                        var content: [[String: Any]] = [["type":"text", "text":"Made three layouts."]]
                        for (id, name, size) in [("gallery-wide", "wide-layout.png", CGSize(width: 960, height: 320)),
                                                 ("gallery-tall", "phone-layout.png", CGSize(width: 390, height: 1200)),
                                                 ("gallery-square", "icon.png", CGSize(width: 512, height: 512))] {
                            let format = UIGraphicsImageRendererFormat(); format.scale = 1
                            let bytes = UIGraphicsImageRenderer(size: size, format: format).pngData { context in
                                UIColor.systemTeal.setFill(); context.fill(CGRect(origin: .zero, size: size)) // theme-exempt: draws a fixture image, not UI
                                UIColor.systemIndigo.setFill(); context.fill(CGRect(x: 0, y: 0, width: size.width, height: size.height / 4)) // theme-exempt: draws a fixture image, not UI
                            }
                            previewBytes[id] = bytes
                            content.append(["type":"wonderArtifact", "file":["id":id, "name":name, "mimeType":"image/png", "byteSize":bytes.count,
                                "sha256":ConversationFile.digest(bytes), "state":"available", "updatedAt":"Synthetic fixture",
                                "width":Int(size.width), "height":Int(size.height)] as [String: Any]])
                        }
                        toolPayload["result"] = ["content":content]
                    }
                    if ProcessInfo.processInfo.arguments.contains("-activity-final-image-preview") {
                        var finalFile = file
                        finalFile["id"] = "final-preview"; finalFile["name"] = "Final chart.png"
                        previewBytes["final-preview"] = data
                        finalPayload["contentItems"] = [["type":"wonderArtifact", "file":finalFile]]
                    }
                }
                let markdownReply = """
                ## Summary
                The tests passed. I updated the **chat labels** and checked the `project notes`.

                - Labels read naturally
                - Notes stay in sync
                1. Run the tests
                2. Review the diff

                > Remaining work is optional.

                | File | Change | Why |
                |------|--------|-----|
                | ChatView.swift | Labels | Read naturally with VoiceOver and at every text size |
                """
                let reply = commandFailed ? "The test command failed. I checked the remaining project files and notes." : failed ? "The tests passed, but I couldn’t load the project notes." : ProcessInfo.processInfo.arguments.contains("-markdown-reply-preview") ? markdownReply : "The tests passed. I updated the chat labels and checked the project notes."
                let toolState = failed ? "failed" : running ? "streaming" : "completed"
                var items: [[String: Any]] = [
                    ["id":"echo", "type":"userMessage", "state":"completed", "text":"Runtime input wrapper", "createdAt":"0", "payload":["clientId":"fixture-user"]],
                    ["id":"command", "type":"commandExecution", "state":"completed", "createdAt":"2000", "payload":["command":"swift test", "cwd":"project", "output":"Executed 30 tests, with 0 failures.", "exitCode":0]],
                    ["id":"search", "type":"webSearch", "state":"completed", "createdAt":"3000", "payload":["query":"SwiftUI accessibility labels", "resultCount":3]],
                    ["id":"files", "type":"fileChange", "state":"completed", "createdAt":"4000", "payload":["paths":["Sources/ChatView.swift"], "additions":12, "deletions":4]],
                    ["id":"tool", "type":"mcpToolCall", "state":toolState, "createdAt":"5000", "payload":toolPayload],
                    ["id":"reply", "type":"agentMessage", "state":"completed", "text":reply, "createdAt":"6000", "payload":finalPayload]
                ]
                if commandFailed {
                    items[1] = ["id":"command", "type":"commandExecution", "state":"failed", "createdAt":"2000", "payload":["command":"swift test", "cwd":"project", "output":"The test command could not complete.", "exitCode":1]]
                }
                if ProcessInfo.processInfo.arguments.contains("-response-edits-preview") {
                    let paths = ["Sources/ChatView.swift", "Tests/ChatViewTests.swift", "Documentation/Changes.md", "Sources/Long folder name/Accessible layout.swift"]
                    let diffs = paths.map { path in
                        // The last saved patch is shorter than its counts, like a host-capped diff.
                        ["path": path, "kind": "update", "additions": path == paths.last ? 40 : 2, "deletions": 1,
                         "diff": "diff --git a/\(path) b/\(path)\n--- a/\(path)\n+++ b/\(path)\n@@ -12,2 +12,3 @@\n let title = \"Wonder\"\n-let label = \"Files\"\n+let label = \"Edited files\"\n+let accessible = true\n"] as [String: Any]
                    }
                    items[3]["payload"] = ["paths": paths, "diffs": diffs]
                }
                if mixed {
                    items.insert(["id":"z-commentary", "type":"agentMessage", "state":"completed", "text":"I’ll check the project files and notes.", "createdAt":"1500", "payload":["phase":"commentary"]], at: 1)
                }
                var turn: [String: Any] = ["id":"turn", "status":running ? "inProgress" : "completed", "items":running ? Array(items.dropLast()) : items]
                if ProcessInfo.processInfo.arguments.contains("-response-turn-diff-preview") {
                    // Codex's whole-turn diff also has a page the agent wrote from the shell.
                    let page = "@@ -0,0 +1,3 @@\n+<!doctype html>\n+<title>Comic</title>\n+<main id=\"panels\"></main>"
                    turn["editedFiles"] = ["paths": ["Sources/ChatView.swift", "comic/index.html"], "additions": 5, "deletions": 1, "diffs": [
                        ["path": "Sources/ChatView.swift", "kind": "update", "additions": 2, "deletions": 1,
                         "diff": "@@ -12,2 +12,3 @@\n let title = \"Wonder\"\n-let label = \"Files\"\n+let label = \"Edited files\"\n+let accessible = true"],
                        ["path": "comic/index.html", "kind": "add", "additions": 3, "deletions": 0, "diff": page]]]
                    if ProcessInfo.processInfo.arguments.contains("-response-turn-diff-unnamed-preview"),
                       var saved = turn["editedFiles"] as? [String: Any], var diffs = saved["diffs"] as? [[String: Any]] {
                        // A change whose event carries no `path` field (the host leaves it out when it
                        // can't name the file); only its patch header says which file it was.
                        diffs.append(["kind": "update", "additions": 4, "deletions": 2,
                                      "diff": "diff --git a/Docs/Release notes.md b/Docs/Release notes.md\n--- a/Docs/Release notes.md\n+++ b/Docs/Release notes.md\n@@ -3,3 +3,5 @@\n # Release notes\n-Draft\n-Pending\n+Ready for review\n+Edited by the agent\n+See the changed files list\n+Last updated today"])
                        saved["diffs"] = diffs; saved["additions"] = 9; saved["deletions"] = 3
                        turn["editedFiles"] = saved
                    }
                }
                let fixture: [String: Any] = ["conversationId":"preview", "hostEpoch":"fixture", "lastSequence":1,
                    "messages":[["messageId":"stored", "clientMessageId":"fixture-user", "codexTurnId":"turn", "body":"Check the project and summarize the changes.", "state":running ? "streaming" : "completed", "createdAt":"1000", "attachmentIds":[]]],
                    "assistantMessages":[], "thread":["hydrated":true, "turns":[turn]]]
                if let data = try? JSONSerialization.data(withJSONObject: fixture),
                   let snapshot = try? JSONDecoder().decode(ConversationSnapshot.self, from: data) {
                    snapshots["preview"] = snapshot
                }
            }
            computerActivities = snapshots.mapValues { ComputerAgentActivity.latest(in: $0) }
            return
        }
        #endif
        do {
            if persistConnection != nil {
                connection = saved
                if saved != nil { status = "Checking your computer…" }
            } else if let data = try identity.read("connection") {
                connection = try JSONDecoder().decode(SavedConnection.self, from: data)
                status = "Checking your computer…"
            }
        } catch { self.error = error.localizedDescription }
        if connection?.requiresPairing == true { stopForIdentityRecovery() }
    }

    /// The demo computer's model. Its chats live under `root` (a temporary folder
    /// the library removes with the demo); requests go to `DemoURLProtocol`.
    init(demoStoreRoot root: URL, saved: SavedConnection, api: PairingAPI, signingIdentity: SigningIdentity) {
        defer { homeNetworkTrust.update(connection) }
        self.signingIdentity = signingIdentity
        // The demo is never saved: no Keychain item, no saved computer.
        persistConnection = { _ in }
        self.api = api
        isDemo = true
        connection = saved
        let demoStore = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        store = demoStore
        writer = makeWriter(demoStore)
        intents = makeIntents(demoStore)
        partition = saved.credential.hostInstallationId + ":" + saved.credential.deviceId
        status = "Connected to your computer."
    }

    #if WONDER_DIAGNOSTICS
    /// Creates an offline diagnostics model with the same durable composer
    /// boundary used by a paired connection. No network API is invoked.
    init(cameraFixtureStoreRoot root: URL, saved: SavedConnection, chat: ChatSummary? = nil, initialIntent: ComposerIntent = ComposerIntent(), api: PairingAPI? = nil, replayEnabled: Bool = true, signingIdentity: SigningIdentity = PhoneIdentity.signing) {
        defer { homeNetworkTrust.update(connection) }
        self.signingIdentity = DiagnosticRouteSwitchProtocol.enabled(saved) ? DiagnosticRouteSwitchProtocol.signingIdentity : signingIdentity
        // Synthetic connection checks must never replace the app's pairing.
        persistConnection = { _ in }
        diagnosticReplayEnabled = replayEnabled
        #if WONDER_DIAGNOSTICS
        self.api = api ?? Self.diagnosticAPI(trust: homeNetworkTrust)
        #else
        self.api = api ?? PairingAPI(serverTrustDelegate: homeNetworkTrust)
        #endif
        connection = saved
        let fixtureStore = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        store = fixtureStore
        writer = makeWriter(fixtureStore)
        intents = makeIntents(fixtureStore)
        nativeContextUsage = (try? fixtureStore.loadContextUsage()) ?? [:]
        partition = saved.credential.hostInstallationId + ":" + saved.credential.deviceId
        chats = chat.map { [$0] } ?? []
        selectedChat = chat
        visibleChat = chat
        macConnected = true
        hasConnectedThisLaunch = true
        status = "Connected to your computer."
        // The fixture Mac has published its provider records, as a paired Mac has.
        projects.installPreviewFamilies(AgentFamily.allCases.map {
            ProjectFamilyAvailability(family: $0, available: true, capabilities: .fixtureMac($0))
        })
        if let chat {
            do {
                // Offline composer checks still use the Project's real ownership model.
                let fields: [String: Any] = [
                    "conversationId": chat.id, "projectId": "fixture-project", "projectName": "Todo app",
                    "title": chat.title, "family": "codex", "accessMode": "workspace",
                    "workingFolder": "/fixture", "workingFolderName": "Todo app", "isPinned": false,
                    "hasUnread": false, "hasNativeSession": true, "folderInProject": true
                ]
                let detail = try JSONDecoder().decode(ProjectConversationDetail.self,
                    from: JSONSerialization.data(withJSONObject: fields))
                projects.installPreviewFilesConversation(detail)
                if try fixtureStore.loadIntent(conversation: chat.id) != nil {
                    composers[chat.id] = try fixtureStore.loadComposer(conversation: chat.id)
                } else {
                    try fixtureStore.saveComposer(initialIntent, conversation: chat.id)
                    composers[chat.id] = initialIntent
                }
            } catch {
                composers[chat.id] = initialIntent
                intentLoadFailures.insert(chat.id)
                composerErrors[chat.id] = "The camera fixture draft could not be read."
            }
        }
        reloadOrphanedDrafts()
        if DiagnosticRouteSwitchProtocol.enabled(saved) {
            let configuration = URLSessionConfiguration.ephemeral
            configuration.protocolClasses = [DiagnosticRouteSwitchProtocol.self]
            diagnosticRouteAPI = PairingAPI(configuration: configuration)
            do {
                connection = try SavedConnection(routes: [DiagnosticRouteSwitchProtocol.home,
                    ConnectionRoute(kind: .tailscale, origin: saved.origin)], origin: saved.origin, credential: saved.credential,
                    hostName: saved.hostName, storageDeviceId: saved.storageDeviceId, hostKeyPin: DiagnosticRouteSwitchProtocol.pin)
            } catch { diagnosticRouteCheckpoint = "Synthetic fixture · Failed" }
        }
        if DiagnosticSubagentFixture.chatResolveHeldFixture {
            // `signal mac-link-down` / `signal mac-link-up` flap the link on the open chat view.
            Task { @MainActor [weak self] in
                var down = false
                while let model = self {
                    let wantDown = DiagnosticSubagentFixture.signalled("mac-link-down") && !DiagnosticSubagentFixture.signalled("mac-link-up")
                    if wantDown != down { down = wantDown; model.macConnected = !wantDown }
                    try? await Task.sleep(for: .milliseconds(100))
                }
            }
        }
    }

    func reloadCameraFixtureDraft(_ chat: String) {
        composers[chat] = nil
        loadComposer(chat)
    }
    #endif

    func loadUsage(family: AgentFamily, force: Bool = false, maxAge: TimeInterval = 300) async throws {
        guard let saved = connection, !accessEnded, projects.capabilities(family).usageLimits else { return }
        let scope = assignmentScope
        let origin = saved.origin
        if !force, let cached = usageCache[family]?[scope], Date().timeIntervalSince(cached.fetchedAt) < maxAge {
            return
        }

        #if WONDER_DIAGNOSTICS
        if family == .codex, usageCache[family]?[scope] != nil, ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-refresh-fails") {
            throw PairingFailure.response(503)
        }
        if family == .codex, ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-unsupported") {
            throw PairingFailure.response(501)
        }
        if family == .codex, ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-unavailable") {
            throw PairingFailure.response(503)
        }
        if ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-fixture") {
            let exhausted = ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-exhausted")
            let empty = family == .codex && ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-empty")
            // A Codex Pro plan: one weekly primary window and no secondary.
            let weeklyOnly = family == .codex && ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-weekly-only")
            // `-diagnostics-usage-ahead` / `-live-reset` put the resets in the future so the page can show
            // a countdown and a pace; `ahead` also spends the windows faster than their time passes.
            let ahead = ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-ahead")
            let live = ahead || ProcessInfo.processInfo.arguments.contains("-diagnostics-usage-live-reset")
            let nowMs = UInt64(Date().timeIntervalSince1970 * 1000)
            let fiveHourReset: UInt64 = live ? nowMs + 2 * 3_600_000 : 1_700_018_000_000
            let weeklyReset: UInt64 = live ? nowMs + 3 * 86_400_000 : 1_700_604_800_000
            let fiveUsed: Double = exhausted ? 100 : ahead ? 85 : (family == .claude ? 14 : 27)
            let weekUsed: Double = ahead ? 78 : (family == .claude ? 8 : 41)
            let fixture = CodexUsageResponse(
                agentFamily: family.rawValue,
                checkedAtMs: 1_700_000_000_000,
                windows: empty ? [] : weeklyOnly ? [
                    CodexUsageWindow(id: "primary", label: "Weekly", usedPercent: 34, remainingPercent: 66, windowDurationMins: 10_080, resetsAt: weeklyReset)
                ] : [
                    CodexUsageWindow(id: family == .claude ? "five_hour" : "five-hours", label: "5 hours", usedPercent: fiveUsed, remainingPercent: 100 - fiveUsed, windowDurationMins: 300, resetsAt: fiveHourReset),
                    CodexUsageWindow(id: family == .claude ? "seven_day" : "weekly", label: "Weekly", usedPercent: weekUsed, remainingPercent: 100 - weekUsed, windowDurationMins: 10_080, resetsAt: weeklyReset)
                ],
                planType: family == .claude ? "max" : "pro",
                accountKey: "fixture-\(family.rawValue)",
                accountLabel: family == .claude ? "owner@example.com" : "owner@example.org"
            )
            guard scope == assignmentScope, connection?.origin == origin, !accessEnded else { return }
            usageCache[family, default: [:]][scope] = CodexUsageCacheEntry(response: fixture, fetchedAt: Date())
            return
        }
        #endif

        // Hosts that predate the parameter answer with Codex usage.
        let path = "/api/v1/account/usage" + (family == .codex ? "" : "?agentFamily=\(family.rawValue)") // provider identity
        let response: CodexUsageResponse = try await api.request(path, origin: saved.origin, credential: saved.credential)
        guard scope == assignmentScope, connection?.origin == origin, !accessEnded else { return }
        guard AgentFamily(host: response.agentFamily) == family else { throw ReadFailure.resync }
        usageCache[family, default: [:]][scope] = CodexUsageCacheEntry(response: response, fetchedAt: Date())
    }

    func pair(link: String, address: String, code: String) {
        guard !busy else { return }
        busy = true; error = nil; verification = nil; status = "Contacting your computer…"
        enrollment = Task {
            defer { homeNetworkTrust.update(connection); busy = false; verification = nil }
            do {
                try Task.checkCancellation()
                let origin: String, body: Data, path: String, expectedHost: String?, expectedOffer: String?, hostKeyPin: String?
                // Validate the input before any identity recovery. Preflight the
                // signing operation before consuming the Mac's single-use offer.
                if !link.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { _ = try PairingLink(link) }
                // A home address is trusted only with the QR code's key, so a
                // code there would always fail; say which way works instead.
                else if HomeNetworkTrust.isHomeOrigin(try PairingLink.address(address)) { throw PairingFailure.homeNetworkCode }
                let enrollmentIdentity = try await signingIdentity.prepareForEnrollment { [weak self] in
                    guard let self else { throw CancellationError() }
                    try await self.requireIdentityRepair()
                }
                try Task.checkCancellation()
                let key = enrollmentIdentity.publicKey
                if !link.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                    let parsed = try PairingLink(link)
                    hostKeyPin = parsed.hostKey
                    origin = parsed.origin; expectedHost = parsed.hostID; expectedOffer = parsed.offerID; path = "/api/v1/pairing/claim"
                    struct Request: Encodable { let offerId: String; let secret: String; let publicKey: PublicKey; let label: String; let model: String; let sessionExpiration = "never" }
                    body = try JSONEncoder().encode(Request(offerId: parsed.offerID, secret: parsed.secret, publicKey: key, label: UIDevice.current.name, model: DeviceModel.identifier))
                } else {
                    hostKeyPin = nil
                    origin = try PairingLink.address(address); expectedHost = nil; expectedOffer = nil; path = "/api/v1/pairing/code"
                    struct Request: Encodable { let humanCode: String; let publicKey: PublicKey; let label: String; let model: String; let sessionExpiration = "never" }
                    body = try JSONEncoder().encode(Request(humanCode: code.trimmingCharacters(in: .whitespacesAndNewlines), publicKey: key, label: UIDevice.current.name, model: DeviceModel.identifier))
                }
                try homeNetworkTrust.preparePairing(origin: origin, pin: hostKeyPin)
                if let hostKeyPin {
                    let nonce = UUID().uuidString.replacingOccurrences(of: "-", with: "")
                    let proof: ConnectionRouteProof
                    do { proof = try await api.request("/api/v1/host/proof?nonce=\(nonce)", origin: origin) }
                    // The trust delegate cancels a computer whose key is not the QR code's.
                    catch let error as URLError where error.code == .cancelled && !Task.isCancelled { throw PairingFailure.wrongHost }
                    try Task.checkCancellation()
                    try proof.validate(pin: hostKeyPin, origin: origin, nonce: nonce)
                }
                try await signingIdentity.validateCurrent(enrollmentIdentity)
                try Task.checkCancellation()
                try enrollmentIdentity.checkCurrent()
                let claim: Claim = try await api.request(path, origin: origin, body: body)
                try Task.checkCancellation()
                try claim.challenge.validate(origin: origin, hostID: expectedHost, deviceID: claim.deviceId)
                guard expectedOffer == nil || claim.challenge.offerId == expectedOffer else { throw PairingFailure.wrongHost }
                verification = claim.challenge.verificationCode
                status = "Confirm this device where cupola pair is running on your computer. Check that the verification text matches."
                struct Signed: Encodable { let challengeId: String; let signature: String }
                let signature = try await signingIdentity.sign(claim.challenge.transcript, using: enrollmentIdentity)
                try Task.checkCancellation()
                let signed = try JSONEncoder().encode(Signed(challengeId: claim.challenge.challengeId, signature: signature))
                while !Task.isCancelled {
                    try claim.challenge.validate(origin: origin, hostID: claim.challenge.hostInstallationId, deviceID: claim.deviceId)
                    do {
                        let credential: Credential = try await api.request("/api/v1/pairing/session", origin: origin, body: signed)
                        try Task.checkCancellation()
                        guard credential.deviceId == claim.deviceId, credential.hostInstallationId == claim.challenge.hostInstallationId else { throw PairingFailure.wrongHost }
                        try await signingIdentity.validateCurrent(enrollmentIdentity)
                        try Task.checkCancellation()
                        try enrollmentIdentity.checkCurrent()
                        let saved = try SavedConnection(routes: [ConnectionRoute(kind: HomeNetworkTrust.isHomeOrigin(origin) ? .home : .tailscale, origin: origin)],
                            credential: credential, hostKeyPin: hostKeyPin)
                            .preservingStorage(from: previousPairingConnection?(credential.hostInstallationId))
                        try saveConnection(saved)
                        connection = saved; status = "This device is paired. Checking your computer…"
                        break
                    } catch {
                        // A locked phone or dropped link keeps polling until the offer ends.
                        guard !(error is CancellationError),
                              EnrollmentPoll.decision(after: error, nowMs: SessionRenewal.nowMs, expiresAtMs: claim.challenge.expiresAtMs) == .keepPolling
                        else { throw error }
                        try await Task.sleep(for: .seconds(2))
                    }
                }
            } catch is CancellationError { status = "Pairing stopped. Reject the request on your computer if it is still waiting." }
            catch {
                if SigningIdentityFailure.requiresPairing(error) {
                    try? requireIdentityRepair()
                    self.error = SigningIdentityFailure.invalidated.localizedDescription
                } else { self.error = error.localizedDescription }
                status = "This device is not connected."
            }
        }
    }
    func cancel() { enrollment?.cancel() }

    private func requireIdentityRepair() throws {
        if let identityNeedsRepair { try identityNeedsRepair() }
        else if var saved = connection {
            saved.requiresPairing = true
            stopForIdentityRecovery()
            try saveConnection(saved)
        }
    }

    func stopForIdentityRecovery() {
        stopReading()
        // Network writers capture this namespace before awaiting. Retire it
        // without changing the disk directory or discarding readable drafts.
        partition = "identity-recovery:" + UUID().uuidString
        cancelOutboxDrain()
        connectionCheck?.cancel()
        connectionCheck = nil
        if connection != nil { connection?.requiresPairing = true }
        macConnected = false
        accessEnded = true
        noteLoss(.deviceKeyLost)
        cachedConversationIds = projection.knownConversationIDs
    }

    func retireAfterPairingReplacement() {
        flushTypedDrafts()
        stopForIdentityRecovery()
        retiredAfterPairing = true
        // A replacement model's cache load is queued behind this final write.
        writer?.retire(); writer = nil
        // Queued positions still reach the outgoing store, in order.
        intents = nil; savedPositions = [:]; unsavedPositions = [:]
        store = nil
        reloadOrphanedDrafts(); orphanedDraftError = nil
    }

    func setForeground(_ active: Bool) {
        if !active { flushTypedDrafts() }
        guard !previewMode else { return }
        dictation.foreground(active)
        appInForeground = active
        // Voice keeps talking with the screen locked or another app in front;
        // its tools act through this live connection, so it stays up until
        // voice ends (`voiceSessionEnded`). A failed session's panel needs none.
        if !active, voice.needsBackgroundConnection { return }
        applyForeground(active)
    }

    /// No Cupola window is in front (a system overlay or the app switcher) or
    /// one is again. Typed words are saved and dictation stops, but the
    /// connection stays: only the background suspends it (`setForeground`).
    func setInFront(_ inFront: Bool) {
        if !inFront { flushTypedDrafts() }
        guard !previewMode else { return }
        dictation.foreground(inFront)
    }

    /// Cupola went to the background while voice talked: suspend now.
    func voiceSessionEnded() {
        scheduleResidentTrim()
        guard !previewMode, !appInForeground, foreground else { return }
        applyForeground(false)
    }

    private func applyForeground(_ active: Bool) {
        // Already live (voice kept it through the background, or another
        // window took over): nothing went stale, so nothing reloads.
        if active, foreground { return }
        foreground = active
        if !active {
            stopReading()
            cancelOutboxDrain()
            macConnected = nil
            // Replay stops here, so saved chats are unverified until the next
            // foreground refresh. Persist that with the cursor before suspension.
            projection.invalidateAll()
            cachedConversationIds = projection.dirty
            if let writer {
                writer.schedule(projection)
                let grant = CacheBackgroundTask()
                cacheBackgroundTask = grant
                let intents = intents
                writer.flush { Task { @MainActor in await intents?.flush(); grant.finish() } }
            }
        } else {
            cacheBackgroundTask?.finish(); cacheBackgroundTask = nil
            scheduleRenewal()
            Task { await loadChats(force: true) }
            #if WONDER_DIAGNOSTICS
            startDiagnosticMacWaiting()
            startDiagnosticMacArchive()
            startDiagnosticRouteSwitch()
            #endif
        }
    }

    private func stopReading() {
        stopRouteMonitoring()
        #if WONDER_DIAGNOSTICS
        diagnosticRouteTask?.cancel(); diagnosticRouteTask = nil
        diagnosticReachabilityTask?.cancel(); diagnosticReachabilityTask = nil
        #endif
        cacheBackgroundTask?.finish(); cacheBackgroundTask = nil
        resetNativeReadings(); nativeActivityRequests = [:]
        stopWorkWatchdog()
        nativeWakes.discardPending()
        nativeTerminalActivity.removeAll()
        forgetNativeHolders()
        if !computerWaitReads.isEmpty { computerWaitReads = [:] }
        generation = UUID()
        cancelRescueWork()
        renewalTimer?.cancel(); renewalTimer = nil
        accessRecovery?.task.cancel(); accessRecovery = nil
        replay?.cancel(); replay = nil
        socket?.close(); socket = nil
        // A relay connection is reopened when it is needed again. Only the
        // connections open now close: a later check may open a new one first.
        let ending = api.detachSealed()
        if !ending.isEmpty { Task { for client in ending { await client.close() } } }
        refreshCoordinator.cancel()
        eventListConversations.removeAll(); eventListDiscovery = false; nativeActivityRefreshes.removeAll()
        liveLog = LiveEventLog()
    }

    private static func readStore(for saved: SavedConnection) -> ReadStore {
        ReadStore(root: FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Wonder/Hosts"), host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
    }

    /// Complete file protection locks saved chats with the phone (voice can
    /// keep running on the lock screen). Writers pause before encoding and
    /// write their newest state once the phone is unlocked.
    private static let protectedDataObservers: [NSObjectProtocol] = {
        ProtectedDataGate.shared.update(available: UIApplication.shared.isProtectedDataAvailable)
        return [
            NotificationCenter.default.addObserver(forName: UIApplication.protectedDataWillBecomeUnavailableNotification,
                                                   object: nil, queue: .main) { _ in ProtectedDataGate.shared.update(available: false) },
            NotificationCenter.default.addObserver(forName: UIApplication.protectedDataDidBecomeAvailableNotification,
                                                   object: nil, queue: .main) { _ in ProtectedDataGate.shared.update(available: true) },
        ]
    }()

    private func makeIntents(_ store: ReadStore) -> IntentWriter {
        _ = Self.protectedDataObservers
        return IntentWriter(store: store) { [weak self] key, result in
            guard key.hasPrefix(IntentWriter.positionKey(conversation: "")) else { return }
            let saved: Bool = if case .success = result { true } else { false }
            Task { @MainActor [weak self] in
                guard let self, self.store?.directory == store.directory else { return }
                if saved {
                    for chat in self.unsavedPositions.keys where IntentWriter.positionKey(conversation: chat) == key {
                        self.unsavedPositions[chat] = nil
                    }
                    if self.unsavedPositions.isEmpty, self.storageFailure == .positionNotSaved { self.storageFailure = nil }
                    return
                }
                // A later scroll writes again instead of skipping as unchanged,
                // and Try again writes the failed position again.
                for (chat, position) in self.savedPositions where IntentWriter.positionKey(conversation: chat) == key {
                    self.savedPositions[chat] = nil
                    self.unsavedPositions[chat] = position
                }
                if self.storageFailure == nil { self.storageFailure = .positionNotSaved }
            }
        }
    }

    private func makeWriter(_ store: ReadStore) -> ProjectionWriter {
        _ = Self.protectedDataObservers
        return ProjectionWriter(store: store) { [weak self] result, elapsed in
            #if WONDER_DIAGNOSTICS
            if case .success(let bytes) = result {
                DiagnosticJournal.shared.record(DiagnosticEvent(operation: "persistence.save", durationMs: elapsed * 1000, bytes: UInt64(bytes)))
            }
            #endif
            let saved: Bool = if case .success = result { true } else { false }
            Task { @MainActor [weak self] in
                guard let self else { return }
                if !saved { self.storageFailure = .chatsNotSaved }
                else if self.storageFailure == .chatsNotSaved {
                    // A position that still failed keeps its own notice.
                    self.storageFailure = self.unsavedPositions.isEmpty ? nil : .positionNotSaved
                }
                // Unlock/retry can make a previously unsaved eviction candidate
                // durable even when no new network event arrives afterwards.
                if saved { self.scheduleResidentTrim() }
            }
        }
    }

    /// Restores this host's saved chats off the main thread. Network reads wait
    /// for it through `check()`, so an unloaded cache is never overwritten.
    private func prepare(_ saved: SavedConnection) async {
        guard !retiredAfterPairing else { return }
        let key = saved.credential.hostInstallationId + ":" + saved.credential.deviceId
        guard partition != key else { await preparation?.value; return }
        // A chat restored on launch appears (and is presented) before the
        // first preparation; it is this pairing's, so it stays the shown
        // chat. Dropping it left the open chat deaf to its own events.
        let switchingPairing = partition != nil
        // Typed text is written to the outgoing pairing's store first.
        flushTypedDrafts()
        forgetTypedDrafts()
        stopReading()
        partition = key
        if storageFailure != nil { storageFailure = nil }
        cancelOutboxDrain()
        stoppingProjectSubagents = [:]; projectSubagentStopErrors = [:]
        projectSubagentLoadTokens = [:]
        projectSubagentAvailability = [:]
        pullRequests = [:]; pullRequestErrors = [:]; pullRequestLoadTokens = [:]; refreshingPullRequests = []
        gitSummaries = [:]; gitSummaryLoadTokens = [:]
        threadChangeCounts = [:]; threadChangeLoadTokens = [:]
        devServers = [:]; devServerLoadTokens = [:]
        threadRepositories = [:]; threadRepositoryLoadTokens = [:]
        projectSubagentLookup = [:]
        projectSubagentFreshIDs = [:]
        projectSubagentNextCurrentCursor = [:]
        projectSubagentNextArchivedCursor = [:]
        projectSubagentInitialCurrentCursor = [:]
        projectSubagentInitialArchivedCursor = [:]
        projectSubagentSeenCurrentCursors = [:]
        projectSubagentSeenArchivedCursors = [:]
        projectSubagentExpandedParents = []
        projectSubagentPagingLimited = []
        projectSubagentRefreshPending = []
        projectSubagentPageRevision = [:]
        loadingOlderProjectSubagents = []
        projectSubagents = [:]; projectSubagentErrors = [:]; composers = [:]; composerErrors = [:]; rewindResolutions = [:]; sending = []; preparingSends = []; finishingDictation = []; intentLoadFailures = []; attention = []; asyncQuestions = [:]; retryableAsyncReplies = []; savedAsyncReplies = [:]; attentionErrors = [:]; resolving = []; savedDecisions = [:]; files = [:]; queues = [:]; queuedSends = [:]; awaitingAcceptance = [:]; awaitingSince = [:]; uploadTasks.values.forEach { $0.cancel() }; uploadTasks = [:]; uploading = []; uploadProgress = [:]; uploadFailures = [:]; stopping = []; controlErrors = [:]; loadingConversationIDs = []; loadingTokens = [:]; conversationLoadFailures = [:]
        residentTrimTask?.cancel(); residentTrimTask = nil; residentPressure = false
        let store = Self.readStore(for: saved)
        self.store = store
        nativeContextUsage = (try? store.loadContextUsage()) ?? [:]; nativeContextNote = [:]
        reloadOrphanedDrafts()
        writer?.retire(); writer = nil
        intents = makeIntents(store); savedPositions = [:]; unsavedPositions = [:]
        if switchingPairing { selectedChat = nil }
        let load = Task { [weak self] in
            #if WONDER_DIAGNOSTICS
            let loadStart = ProcessInfo.processInfo.systemUptime
            #endif
            let loaded: Result<ProjectionState, Error>
            do { loaded = .success(try await ProjectionWriter.load(store)) }
            catch { loaded = .failure(error) }
            guard !Task.isCancelled, let self, self.partition == key else { return }
            switch loaded {
            case .success(var state):
                state.invalidateAll()
                self.projection = state
            case .failure:
                self.projection = ProjectionState()
                self.storageFailure = .savedChatsUnreadable
            }
            #if WONDER_DIAGNOSTICS
            DiagnosticJournal.shared.record(DiagnosticEvent(operation: "persistence.load", durationMs: (ProcessInfo.processInfo.systemUptime - loadStart) * 1000, count: UInt64(self.projection.knownConversationIDs.count)))
            #endif
            self.writer = self.makeWriter(store)
            self.publish(.everything)
            self.dictation.restore(force: true)
        }
        preparation = load
        await load.value
    }

    private struct PublishScope: OptionSet {
        let rawValue: Int
        static let list = PublishScope(rawValue: 1)
        static let snapshots = PublishScope(rawValue: 2)
        static let everything: PublishScope = [.list, .snapshots]
    }

    /// Publishes only the parts of the projection a change touched. Replacing
    /// every snapshot on each list refresh or replay event re-projected the
    /// open conversation and re-rendered every observer.
    private func publish(_ scope: PublishScope) {
        #if WONDER_DIAGNOSTICS
        let diagnosticStart = ProcessInfo.processInfo.systemUptime
        defer { DiagnosticJournal.shared.record(DiagnosticEvent(operation: "projection", durationMs: (ProcessInfo.processInfo.systemUptime-diagnosticStart)*1000)) }
        #endif
        if scope.contains(.snapshots) { installSnapshots(projection.snapshots, announce: false) }
        setIfChanged(\.cachedConversationIds, projection.dirty)
    }

    /// Updates memory and the screen now; the disk copy follows off the main
    /// thread (coalesced). Replay resumes from an older durable cursor if the
    /// app ends before a write, so a delayed write never loses host history.
    private func commit(_ next: ProjectionState, publishing scope: PublishScope = .everything) throws {
        guard let writer else { throw ReadFailure.resync }
        projection = next
        writer.schedule(next)
        publish(scope)
        scheduleResidentTrim()
    }

    // Each window protects its reading anchor, including a window that is not
    // the connection's last selected chat. Routing never changes saved anchors.
    private var historyWindows: [UUID: String] = [:]
    func setHistoryVisible(_ conversation: String?, window: UUID) {
        historyWindows[window] = conversation
        if let conversation { projection.touchConversation(conversation) }
        scheduleResidentTrim()
    }
    private var residentTrimTask: Task<Void, Never>?
    private var residentPressure = false
    private var protectedHistory: Set<String> {
        var ids = projects.pinnedConversationIDs.union(historyWindows.values)
        ids.formUnion([visibleChat?.id, visibleChat == nil ? nil : selectedChat?.id].compactMap { $0 })
        ids.formUnion(workingSnapshots)
        ids.formUnion(loadingConversationIDs)
        ids.formUnion(nativeHistoryRefreshing)
        ids.formUnion(sending); ids.formUnion(preparingSends)
        ids.formUnion(awaitingAcceptance.keys)
        // Voice's tools can act on another conversation while its starting
        // chat stays pinned. Keep their current inputs until that owner ends.
        if voice.isActive { ids.formUnion(projection.snapshots.keys) }
        return ids
    }
    #if WONDER_DIAGNOSTICS
    func diagnosticInstallHistory(_ values: [ConversationSnapshot]) throws {
        var next = projection
        for value in values { next.install(value) }
        try commit(next, publishing: .snapshots)
    }
    func diagnosticWaitForHistoryMaintenance() async {
        await residentTrimTask?.value
        await writer?.flush()
    }
    #endif
    private func scheduleResidentTrim(underPressure: Bool = false) {
        residentPressure = residentPressure || underPressure
        guard residentTrimTask == nil, let writer, !previewMode,
              residentPressure || projection.residentBytes > ProjectionState.residentByteLimit else { return }
        let protected = protectedHistory
        // An oversized open/pinned chat is exempt. Do not turn each streamed
        // frame in that chat into an explicit flush when nothing can leave.
        guard projection.snapshots.keys.contains(where: { !protected.contains($0) }) else {
            residentPressure = false
            return
        }
        let key = partition
        residentTrimTask = Task { @MainActor [weak self] in
            let durable = await writer.durableBodies()
            guard let self, !Task.isCancelled, self.partition == key, self.writer === writer else { return }
            defer { self.residentTrimTask = nil }
            let pressure = self.residentPressure
            let evicted = self.projection.evictResidentBodies(keeping: self.protectedHistory, durable: durable,
                byteLimit: pressure ? 0 : ProjectionState.residentByteLimit)
            let protectedNow = self.protectedHistory
            self.residentPressure = pressure && self.projection.snapshots.keys.contains { !protectedNow.contains($0) }
            if !evicted.isEmpty {
                // Replace any queued snapshot as well; eviction must release
                // published, projection and writer references to these bodies.
                writer.schedule(self.projection)
                self.publish(.snapshots)
            }
        }
    }
    /// On-demand decode stays off MainActor and is fenced by pairing and body
    /// identity. It restores offline content before waiting for a connection.
    private func restoreSavedHistory(_ conversation: String) async {
        await preparation?.value
        guard !Task.isCancelled, projection.snapshots[conversation] == nil,
              let record = projection.bodyRecords?[conversation], let store else { return }
        let key = partition
        do {
            let body = try await ProjectionWriter.loadConversation(store, conversation: conversation, record: record)
            guard !Task.isCancelled, partition == key else { return }
            if projection.restoreConversation(body, matching: record) {
                publish(.snapshots)
                scheduleResidentTrim()
            }
        } catch {
            guard !Task.isCancelled, partition == key, projection.snapshots[conversation] == nil,
                  projection.bodyRecords?[conversation]?.bodyID == record.bodyID else { return }
            // A missing/corrupt disposable body can be refetched; preserve the
            // index and every independent intent until a successful read.
            storageFailure = .savedChatsUnreadable
        }
    }

    func loadChats(force: Bool = false) async {
        await loadChats(refreshVisibleConversation: true)
    }

    private func loadChats(refreshVisibleConversation: Bool) async {
        guard !previewMode else { return }
        // Startup and foreground reads must use the session established by the
        // shared check, not race renewal with the credential restored from disk.
        await check()
        guard let saved = connection, !accessEnded else { return }
        guard macConnected == true else {
            // Out of reach: the replay loop owns trying again, with backoff.
            if macConnected == false, replay == nil, foreground, !saved.requiresPairing { startReplay() }
            return
        }
        await prepare(saved)
        if replay == nil && foreground { startReplay() }
        guard !loadingChats else { return }
        loadingChats = true
        defer { loadingChats = false }
        let run = generation
        do {
            try await refreshList(saved, run: run)
            if refreshVisibleConversation, let chat = visibleChat {
                let helpersDue = workspaceReadsDue(chat)
                if let parent = selectedChat, helpersDue {
                    if isProject(parent) { await loadProjectSubagents(parent) }
                }
                // The live stream keeps a current history current; it is read
                // again only when stale (or the stream is down).
                if !eventStreamLive || projection.dirty.contains(chat.id) || projection.snapshots[chat.id] == nil {
                    await refreshConversation(chat)
                }
                if isProject(chat), helpersDue { Task { [weak self] in await self?.loadPullRequests(chat) }; Task { [weak self] in await self?.loadGitSummary(chat) }; Task { [weak self] in await self?.loadDevServers(chat) } }
                await loadAsyncQuestions(chat)
                if isProject(chat) { try? await loadQueue(chat) }
                if composers[chat.id]?.pending != nil, composers[chat.id]?.pending?.receipt == nil {
                    await deliver(chat)
                }
            }
        }
        catch { readFailed(error, run: run, origin: saved.origin, token: saved.credential.sessionToken) }
    }

    /// The open chat's helpers, pull requests, Git state and dev servers
    /// change with its work: the periodic refresh reads them again when the
    /// chat moved since their last read, otherwise every two minutes.
    private var workspaceReads: [String: (sequence: UInt64?, at: ContinuousClock.Instant)] = [:]
    private func workspaceReadsDue(_ chat: ChatSummary) -> Bool {
        let sequence = snapshots[chat.id]?.lastSequence
        if let read = workspaceReads[chat.id], read.sequence == sequence, ContinuousClock.now - read.at < .seconds(120) { return false }
        workspaceReads[chat.id] = (sequence, .now)
        return true
    }

    private func refreshList(_ saved: SavedConnection, run: UUID) async throws {
        // A foreground/resync catalog read cannot be satisfied by a narrower
        // event pass that only refreshed an affected Project's rows.
        try await refreshCoordinator.read(.list, afterCurrent: true) { [self] in try await performListRefresh(saved, run: run) }
    }

    private func performListRefresh(_ saved: SavedConnection, run: UUID) async throws {
        let cursor = projection.lastSequence
        await projects.refresh()
        guard run == generation, !Task.isCancelled else { throw CancellationError() }
        var next = projection
        next.listDirty = cursor != projection.lastSequence || projects.failure != nil
        // Catalog retention and resident history are independent. A chat that
        // falls out of the recent list still owns its offline body on disk.
        try commit(next, publishing: .list)
        // Reachability belongs to the authenticated check/stream, not a
        // catalog refresh that can finish from cached or failed provider reads.
        // The Mac's list replaced what this device could not read.
        if storageFailure == .savedChatsUnreadable { storageFailure = nil }
    }

    func agentFamily(_ chat: ChatSummary) -> AgentFamily? {
        if let detail = projects.details[chat.id] { return detail.family }
        return nil
    }
    /// What the thread's provider offers, from the Mac's capability record;
    /// nil outside a Project thread. Gate features on this, not on the family.
    func capabilities(_ chat: ChatSummary) -> ProviderCapabilities? {
        agentFamily(chat).map(projects.capabilities)
    }

    /// A goal is running in this thread and nothing has been said yet, so
    /// the conversation reads "Goal running" instead of looking idle.
    func goalRunsWithoutMessages(_ chatID: String) -> Bool {
        guard goals[chatID]?.status == "active", let snapshot = snapshots[chatID] else { return false }
        return snapshot.messages.isEmpty && snapshot.assistantMessages.isEmpty
            && (snapshot.thread.turns ?? []).isEmpty && snapshot.thread.nextCursor == nil
    }

    func loadGoal(_ chat: ChatSummary) async {
        guard isProject(chat), capabilities(chat)?.goals == true, !previewMode,
              let saved = connection, !accessEnded else { return }
        let key = partition
        let mutation = goalMutationTokens[chat.id]
        do {
            let response: ConversationGoalResponse = try await api.request(
                "/api/v1/conversations/\(Self.escape(chat.id))/goal",
                origin: saved.origin, credential: saved.credential)
            guard key == partition, mutation == goalMutationTokens[chat.id], !Task.isCancelled else { return }
            if goals[chat.id] != response.goal { goals[chat.id] = response.goal }
            if goalErrors[chat.id] == "Goal status could not be refreshed. Try again." { goalErrors[chat.id] = nil }
        } catch PairingFailure.response(404) {
            guard key == partition else { return }
            if goals[chat.id] != nil { goals[chat.id] = nil }
        } catch {
            guard key == partition else { return }
            if goals[chat.id] != nil { goalErrors[chat.id] = "Goal status could not be refreshed. Try again." }
        }
    }

    @discardableResult private func mutateGoal(_ chat: ChatSummary, body: [String: Any]) async -> Bool {
        guard isProject(chat), capabilities(chat)?.goals == true,
              let saved = connection, !accessEnded else { return false }
        let key = partition
        let token = UUID()
        goalMutationTokens[chat.id] = token
        goalErrors[chat.id] = nil
        do {
            let data = try JSONSerialization.data(withJSONObject: body)
            let response: ConversationGoalResponse = try await api.request(
                "/api/v1/conversations/\(Self.escape(chat.id))/goal",
                origin: saved.origin, body: data, credential: saved.credential, method: "PUT")
            guard key == partition, goalMutationTokens[chat.id] == token else { return false }
            goals[chat.id] = response.goal
            return true
        } catch {
            guard key == partition, goalMutationTokens[chat.id] == token else { return false }
            await loadGoal(chat)
            if key == partition, goalMutationTokens[chat.id] == token {
                goalErrors[chat.id] = "Goal could not be saved. Try again."
            }
            return false
        }
    }

    func updateGoal(_ chat: ChatSummary, objective: String, tokenBudget: Int?, timeBudgetSeconds: Int?) async -> Bool {
        await mutateGoal(chat, body: [
            "objective": objective,
            "tokenBudget": tokenBudget.map { $0 as Any } ?? NSNull(),
            "timeBudgetSeconds": timeBudgetSeconds.map { $0 as Any } ?? NSNull()
        ])
    }

    func pauseGoal(_ chat: ChatSummary) async { await mutateGoal(chat, body: ["status": "paused"]) }
    func resumeGoal(_ chat: ChatSummary) async { await mutateGoal(chat, body: ["status": "active"]) }

    func clearGoal(_ chat: ChatSummary) async {
        guard isProject(chat), capabilities(chat)?.goals == true,
              let saved = connection, !accessEnded else { return }
        let key = partition
        let token = UUID()
        goalMutationTokens[chat.id] = token
        goalErrors[chat.id] = nil
        do {
            let _: EmptyGoalResponse = try await api.request(
                "/api/v1/conversations/\(Self.escape(chat.id))/goal",
                origin: saved.origin, credential: saved.credential, method: "DELETE")
            guard key == partition, goalMutationTokens[chat.id] == token else { return }
            goals[chat.id] = nil
        } catch {
            guard key == partition, goalMutationTokens[chat.id] == token else { return }
            await loadGoal(chat)
            if key == partition, goalMutationTokens[chat.id] == token {
                goalErrors[chat.id] = "Goal could not be removed. Try again."
            }
        }
    }

    /// Reads chats' stored notification choices for the sidebar and header;
    /// each chat is read once per connection, a few at a time.
    /// `refreshing` rereads chats already known (the open chat, which may have changed elsewhere).
    func loadChatNotifications(_ chatIDs: [String], refreshing: Bool = false) async {
        guard !previewMode, let saved = connection, !accessEnded else { return }
        let key = partition
        let wanted = chatIDs.filter { refreshing || (!chatNotifications.isKnown($0) && !chatNotificationReads.contains($0)) }.prefix(40)
        chatNotificationReads.formUnion(wanted)
        for id in wanted {
            guard key == partition, !Task.isCancelled else { chatNotificationReads.remove(id); continue }
            do {
                let response: ChatNotificationsResponse = try await api.request(
                    "/api/v1/conversations/\(Self.escape(id))/notifications", origin: saved.origin, credential: saved.credential)
                guard key == partition else { chatNotificationReads.remove(id); continue }
                chatNotifications.learn(id, response.notificationLevel)
            } catch {
                chatNotificationReads.remove(id)
            }
        }
    }

    /// Saves a chat's choice on its computer; nil when kept, else the plain
    /// message for the failure, with the old choice restored.
    func setChatNotification(_ chat: ChatSummary, to level: ChatNotificationLevel) async -> String? {
        guard !accessEnded, let change = chatNotifications.begin(chat.id, to: level) else { return nil }
        // A preview has no computer to keep it.
        if previewMode { chatNotifications.finish(change, savedAs: level); return nil }
        guard let saved = connection else {
            chatNotifications.fail(change)
            return ChatNotificationChoices.failureMessage(change)
        }
        let key = partition
        do {
            let body = try JSONSerialization.data(withJSONObject: ["notificationLevel": level.rawValue])
            let response: ChatNotificationsResponse = try await api.request(
                "/api/v1/conversations/\(Self.escape(chat.id))/notifications",
                origin: saved.origin, body: body, credential: saved.credential, method: "PUT")
            guard key == partition else { return nil }
            chatNotifications.finish(change, savedAs: response.notificationLevel)
            return nil
        } catch {
            guard key == partition else { return nil }
            chatNotifications.fail(change)
            return ChatNotificationChoices.failureMessage(change)
        }
    }

    private func setProjectSubagents(_ agents: [ProjectSubagentSummary], for conversationID: String) {
        guard projectSubagents[conversationID] != agents else { return }
        projectSubagentLookup[conversationID] = Dictionary(agents.map { ($0.threadId, $0) },
                                                           uniquingKeysWith: { _, newer in newer })
        projectSubagents[conversationID] = agents
    }

    func projectSubagent(threadID: String, parentConversationID: String) -> ProjectSubagentSummary? {
        projectSubagentLookup[parentConversationID]?[threadID]
    }

    func loadProjectSubagents(_ parent: ChatSummary, afterCurrentRead: Bool = false) async {
        try? await refreshCoordinator.read(.subagents(parent.id), afterCurrent: afterCurrentRead) { [self] in await performSubagentRead(parent) }
    }

    private func performSubagentRead(_ parent: ChatSummary) async {
        guard isProject(parent), !previewMode, let saved = connection, !accessEnded else { return }
        if loadingOlderProjectSubagents.contains(parent.id) {
            projectSubagentRefreshPending.insert(parent.id)
            return
        }
        let key = partition
        let token = UUID()
        projectSubagentLoadTokens[parent.id] = token
        let pageRevision = projectSubagentPageRevision[parent.id, default: 0]
        setMember(\.loadingOlderProjectSubagents, parent.id, false)
        do {
            let path = try ProjectSubagentPaths.roster(parentConversationId: parent.id)
            let response: ProjectSubagentList = try await api.request(path,
                origin: saved.origin, credential: saved.credential)
            guard key == partition, projectSubagentLoadTokens[parent.id] == token, !Task.isCancelled else { return }
            if loadingOlderProjectSubagents.contains(parent.id) {
                projectSubagentRefreshPending.insert(parent.id)
                return
            }
            if projectSubagentPageRevision[parent.id, default: 0] != pageRevision {
                Task { [weak self] in await self?.loadProjectSubagents(parent) }
                return
            }
            let firstPage = response.subagents.filter { $0.parentConversationId == parent.id }
            let fresh = Set(firstPage.map(\.threadId))
            let preservePaging = projectSubagentExpandedParents.contains(parent.id)
                && projectSubagentInitialCurrentCursor[parent.id] == response.nextCurrentCursor
                && projectSubagentInitialArchivedCursor[parent.id] == response.nextArchivedCursor
            let older = projectSubagentExpandedParents.contains(parent.id)
                ? (projectSubagents[parent.id] ?? []).filter { !fresh.contains($0.threadId) } : []
            setProjectSubagents(firstPage + older, for: parent.id)
            setIfChanged(\.projectSubagentFreshIDs[parent.id], fresh)
            setIfChanged(\.projectSubagentAvailability[parent.id], response.available)
            if !preservePaging {
                projectSubagentPagingLimited.remove(parent.id)
                setIfChanged(\.projectSubagentNextCurrentCursor[parent.id], response.nextCurrentCursor)
                setIfChanged(\.projectSubagentNextArchivedCursor[parent.id], response.nextArchivedCursor)
                projectSubagentSeenCurrentCursors[parent.id] = []
                projectSubagentSeenArchivedCursors[parent.id] = []
            }
            setIfChanged(\.projectSubagentErrors[parent.id], projectSubagentPagingLimited.contains(parent.id)
                ? "Some older agent tasks are beyond this computer's verified history limit."
                : response.detail)
            projectSubagentInitialCurrentCursor[parent.id] = response.nextCurrentCursor
            projectSubagentInitialArchivedCursor[parent.id] = response.nextArchivedCursor
        } catch PairingFailure.response(let status) where [403, 404, 409].contains(status) {
            guard key == partition, projectSubagentLoadTokens[parent.id] == token, !Task.isCancelled else { return }
            setProjectSubagents([], for: parent.id)
            projectSubagentFreshIDs[parent.id] = []
            projectSubagentExpandedParents.remove(parent.id)
            projectSubagentPagingLimited.remove(parent.id)
            projectSubagentAvailability[parent.id] = false
            projectSubagentNextCurrentCursor[parent.id] = nil
            projectSubagentNextArchivedCursor[parent.id] = nil
            projectSubagentInitialCurrentCursor[parent.id] = nil
            projectSubagentInitialArchivedCursor[parent.id] = nil
            projectSubagentSeenCurrentCursors[parent.id] = nil
            projectSubagentSeenArchivedCursors[parent.id] = nil
            projectSubagentErrors[parent.id] = "Agent tasks are no longer available in this chat. Refresh the Project to try again."
        } catch {
            guard key == partition, projectSubagentLoadTokens[parent.id] == token, !Task.isCancelled else { return }
            projectSubagentAvailability[parent.id] = false
            projectSubagentErrors[parent.id] = "Agent tasks could not be loaded. Refresh this chat to try again."
        }
    }

    /// Reads the thread's pull requests. A Mac without GitHub CLI sign-in, or
    /// without this method, hides the pill; a failed refresh keeps the last list.
    func loadPullRequests(_ chat: ChatSummary, refresh: Bool = false) async {
        guard isProject(chat), !previewMode, let saved = connection, !accessEnded else { return }
        // A background read must not supersede a refresh the person asked for:
        // the refresh brings newer states, and its failure is theirs to see.
        if !refresh, refreshingPullRequests.contains(chat.id) { return }
        let key = partition
        let token = UUID()
        pullRequestLoadTokens[chat.id] = token
        if refresh { refreshingPullRequests.insert(chat.id) }
        defer { if pullRequestLoadTokens[chat.id] == token { refreshingPullRequests.remove(chat.id) } }
        do {
            let path = try ThreadPullRequests.path(conversationId: chat.id, refresh: refresh)
            let response: ThreadPullRequests = try await api.request(path, origin: saved.origin, credential: saved.credential)
            guard key == partition, pullRequestLoadTokens[chat.id] == token, !Task.isCancelled else { return }
            pullRequests[chat.id] = response
            // A background read answers from the Mac's cache, so it must not clear the
            // failure of a refresh the person asked for; the next refresh does.
            let detail = response.detail.flatMap { response.available ? $0 : nil }
            if refresh || detail != nil { pullRequestErrors[chat.id] = detail }
        } catch PairingFailure.response(let status) where [403, 404, 409].contains(status) {
            guard key == partition, pullRequestLoadTokens[chat.id] == token else { return }
            pullRequests[chat.id] = nil
            pullRequestErrors[chat.id] = nil
        } catch is CancellationError {
        } catch {
            guard key == partition, pullRequestLoadTokens[chat.id] == token, !Task.isCancelled else { return }
            pullRequestErrors[chat.id] = "Pull requests could not be loaded from your computer. Try again."
        }
    }

    /// Reads the branch the thread's folder is on, and every repository the
    /// thread worked with. A refused read, a Mac without these methods or a
    /// folder outside Git leaves the Git pill grey with the reason; a failed
    /// read keeps the last summary rather than flickering the pill.
    func loadGitSummary(_ chat: ChatSummary) async {
        guard isProject(chat) else { return }
        async let repositories: Void = loadThreadRepositories(chat)
        async let thread = try? loadThreadChanges(chat)
        await loadFolderGitSummary(chat)
        await repositories
        _ = await thread
    }

    /// "This thread": the files this thread itself changed, over all its turns.
    /// Reading the thread's own folder also updates the Git pill's count. A Mac
    /// without This thread (404, 501) or refusing the folder (403, 409) drops
    /// the count, so the pill and Code changes keep the whole-checkout views; a
    /// failed read keeps the last count.
    func loadThreadChanges(_ chat: ChatSummary, location: ThreadRepositories.Location? = nil) async throws -> WorkspaceGitChanges {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            guard let changes = DiagnosticGitFixture.threadChanges(location: location) else { throw PairingFailure.response(404) }
            if location == nil { threadChangeCounts[chat.id] = ThreadChangeCount(changes) }
            return changes
            #else
            throw PairingFailure.missingIdentity
            #endif
        }
        guard isProject(chat), let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let key = partition
        let token = UUID()
        if location == nil { threadChangeLoadTokens[chat.id] = token }
        func owns() -> Bool { location == nil && key == partition && threadChangeLoadTokens[chat.id] == token }
        do {
            let path = try Self.gitEndpoint(chat, operation: "thread-changes", location: location, query: [])
            let changes: WorkspaceGitChanges = try await api.request(path, origin: saved.origin, credential: saved.credential)
            guard key == partition, !accessEnded, !Task.isCancelled else { throw CancellationError() }
            if owns() { threadChangeCounts[chat.id] = ThreadChangeCount(changes) }
            return changes
        } catch {
            switch error {
            case PairingFailure.response(let status) where [403, 404, 409, 501].contains(status),
                 PairingFailure.hostMessage(let status, _) where [403, 404, 409, 501].contains(status):
                if owns() { threadChangeCounts.removeValue(forKey: chat.id) }
            default: break
            }
            throw error
        }
    }

    /// Reads the thread's repository index. An older Mac answers 404 and keeps
    /// the single-repository layout; a failed read keeps the last index.
    func loadThreadRepositories(_ chat: ChatSummary, refresh: Bool = false) async {
        guard isProject(chat) else { return }
        if previewMode {
            #if WONDER_DIAGNOSTICS
            if let index = DiagnosticGitFixture.repositories { threadRepositories[chat.id] = index }
            #endif
            return
        }
        guard let saved = connection, !accessEnded else { return }
        let key = partition
        let token = UUID()
        threadRepositoryLoadTokens[chat.id] = token
        do {
            let path = try ThreadRepositories.path(conversationId: chat.id, refresh: refresh)
            let response: ThreadRepositories = try await api.request(path, origin: saved.origin, credential: saved.credential)
            guard key == partition, threadRepositoryLoadTokens[chat.id] == token, !Task.isCancelled else { return }
            threadRepositories[chat.id] = response
        } catch PairingFailure.response(let status) where [403, 404, 409, 501].contains(status) {
            guard key == partition, threadRepositoryLoadTokens[chat.id] == token else { return }
            threadRepositories[chat.id] = nil
        } catch {}
    }

    private func loadFolderGitSummary(_ chat: ChatSummary) async {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            if let summary = DiagnosticGitFixture.summary { gitSummaries[chat.id] = summary }
            #endif
            return
        }
        guard let saved = connection, !accessEnded else { return }
        let key = partition
        let token = UUID()
        gitSummaryLoadTokens[chat.id] = token
        do {
            let path = try Self.gitEndpoint(chat, operation: "summary", query: [])
            let response: WorkspaceGitSummary = try await api.request(path, origin: saved.origin, credential: saved.credential)
            guard key == partition, gitSummaryLoadTokens[chat.id] == token, !Task.isCancelled else { return }
            gitSummaries[chat.id] = response
        } catch PairingFailure.hostMessage(let status, let message) where [403, 404, 409].contains(status) {
            guard key == partition, gitSummaryLoadTokens[chat.id] == token else { return }
            gitSummaries[chat.id] = .unreadable(message)
        } catch PairingFailure.response(let status) where [403, 404, 409, 501].contains(status) {
            guard key == partition, gitSummaryLoadTokens[chat.id] == token else { return }
            gitSummaries[chat.id] = .unreadable(WorkspaceGitSummary.refusalReason(status: status))
        } catch {}
    }

    /// The web pages served from this thread's folder (not other threads'
    /// worktrees). A Mac without browser preview answers 404 and hides the
    /// globe pill; a failed read keeps what was shown.
    func devServerPresence(_ chat: ChatSummary) -> DevServerPresence? {
        devServers[chat.id]
    }

    func loadDevServers(_ chat: ChatSummary) async {
        guard isProject(chat), !previewMode, let projectId = projects.details[chat.id]?.projectId,
              let saved = connection, !accessEnded else { return }
        let key = partition
        let token = UUID()
        devServerLoadTokens[chat.id] = token
        do {
            let path = try DevServerList.path(projectId: projectId, conversationId: chat.id)
            let response: DevServerList = try await api.request(path, origin: saved.origin, credential: saved.credential)
            guard key == partition, devServerLoadTokens[chat.id] == token, !Task.isCancelled else { return }
            var presence = devServers[chat.id] ?? DevServerPresence()
            presence.update(response.servers, at: Date())
            if presence != devServers[chat.id] { devServers[chat.id] = presence }
        } catch PairingFailure.response(let status) where [403, 404, 501].contains(status) {
            guard key == partition, devServerLoadTokens[chat.id] == token else { return }
            devServers[chat.id] = nil
        } catch {}
    }

    /// Whether a server runs on `port` for this thread, asked by port so a link
    /// opens servers the picker hides (no page at `/`, past its limit). Nil when
    /// the computer cannot show its pages on the phone; the picker is unchanged.
    func devServerRunning(_ chat: ChatSummary, port: Int) async -> Bool? {
        if previewMode { return devServerPresence(chat).map { $0.servers.contains { $0.port == port } } }
        guard isProject(chat), let projectId = projects.details[chat.id]?.projectId,
              let saved = connection, !accessEnded else { return nil }
        do {
            let path = try DevServerList.path(projectId: projectId, conversationId: chat.id, port: port)
            let response: DevServerList = try await api.request(path, origin: saved.origin, credential: saved.credential)
            return response.servers.contains { $0.port == port }
        } catch PairingFailure.response(let status) where [403, 404, 501].contains(status) {
            return nil
        } catch {
            return false
        }
    }

    /// The live preview endpoint for a Project thread; nil without a paired Mac.
    func previewEndpoint(_ chat: ChatSummary) -> PreviewEndpoint? {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            return DiagnosticDevServerFixture.servers == nil ? nil : .fixture
            #else
            return nil
            #endif
        }
        guard let projectId = projects.details[chat.id]?.projectId, let saved = connection, !accessEnded else { return nil }
        return .live(projectId: projectId, conversationId: chat.id, connection: saved)
    }

    func loadGitChanges(_ chat: ChatSummary, scope: WorkspaceChangeScope,
                        location: ThreadRepositories.Location? = nil) async throws -> WorkspaceGitChanges {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            try await Task.sleep(for: .milliseconds(150))
            return DiagnosticGitFixture.changes(scope, location: location)
            #else
            throw PairingFailure.missingIdentity
            #endif
        }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let path = try Self.gitEndpoint(chat, operation: "changes", location: location, query: [URLQueryItem(name: "scope", value: scope.rawValue)])
        return try await api.request(path, origin: saved.origin, credential: saved.credential)
    }

    func loadGitChangeDiff(_ chat: ChatSummary, file: WorkspaceGitChanges.File, scope: WorkspaceChangeScope,
                           location: ThreadRepositories.Location? = nil) async throws -> WorkspaceDiffResponse {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            return WorkspaceDiffResponse(path: file.path, staged: false, diff: try DiagnosticGitFixture.diff(file, scope: scope), annotationVersion: 1)
            #else
            throw PairingFailure.missingIdentity
            #endif
        }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let key = partition
        var query = [URLQueryItem(name: "path", value: file.path), URLQueryItem(name: "scope", value: scope.rawValue)]
        if let original = file.originalPath { query.append(URLQueryItem(name: "originalPath", value: original)) }
        let path = try Self.gitEndpoint(chat, operation: "diff", location: location, query: query)
        let response: WorkspaceDiffResponse = try await api.request(path, origin: saved.origin, credential: saved.credential)
        guard key == partition, !accessEnded, !Task.isCancelled else { throw CancellationError() }
        return response
    }

    /// Runs one Git action on the Mac with its own git and gh sign-in. A
    /// refusal arrives as an outcome in the Mac's words (HTTP 409).
    func runGitAction(_ chat: ChatSummary, _ request: GitActionRequest) async throws -> GitActionOutcome {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            let outcome = try await DiagnosticGitFixture.actionOutcome(request)
            if outcome.ok { await loadThreadRepositories(chat) }
            return outcome
            #else
            throw PairingFailure.missingIdentity
            #endif
        }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let key = partition
        let outcome: GitActionOutcome = try await api.request(try GitActionRequest.path(conversationId: chat.id), origin: saved.origin,
            body: try JSONEncoder().encode(request), credential: saved.credential, decodingStatuses: [409])
        guard key == partition, !accessEnded else { throw CancellationError() }
        if outcome.ok {
            // The Mac dropped its cached reads; show what changed.
            async let repositories: Void = loadThreadRepositories(chat, refresh: true)
            async let summary: Void = loadFolderGitSummary(chat)
            async let pulls: Void = loadPullRequests(chat, refresh: true)
            _ = await (repositories, summary, pulls)
        }
        return outcome
    }

    /// Whether a pull request can merge now and how, read on the Mac. A
    /// refusal (gh signed out, another repository) says why.
    func loadMergeState(_ chat: ChatSummary, repository: ThreadRepositories.Repository, url: URL) async throws -> PullRequestMergeState {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            try await Task.sleep(for: .milliseconds(150))
            return DiagnosticGitFixture.mergeState(url)
            #else
            throw PairingFailure.missingIdentity
            #endif
        }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let key = partition
        let reply: MergeStateReply = try await api.request(try PullRequestMergeState.path(conversationId: chat.id, repository: repository, url: url),
            origin: saved.origin, credential: saved.credential, decodingStatuses: [409])
        guard key == partition, !accessEnded else { throw CancellationError() }
        switch reply {
        case .state(let state): return state
        case .refused(let message): throw PairingFailure.hostMessage(409, message)
        }
    }

    /// Adds a pull request task after the chat's draft for the owner to edit
    /// and send; nothing runs until they do.
    func stagePullRequestTask(_ prompt: String, chat: ChatSummary) -> Bool {
        loadComposer(chat.id)
        guard !intentLoadFailures.contains(chat.id) else { return false }
        let next = PullRequestAgentTask.append(prompt, to: composers[chat.id]?.draft ?? "")
        editDraft(next, chat: chat.id)
        return composers[chat.id]?.draft == next
    }

    /// The thread's own working folder is the "workspace" root; another
    /// repository the thread worked with names its root and folder.
    private static func gitEndpoint(_ chat: ChatSummary, operation: String, location: ThreadRepositories.Location? = nil,
                                    query: [URLQueryItem]) throws -> String {
        var components = workspaceComponents(conversationID: chat.id, operation: "git/" + operation)
        var scope = [URLQueryItem(name: "root", value: location?.rootId ?? "workspace")]
        if let path = location?.path, !path.isEmpty { scope.append(URLQueryItem(name: "repo", value: path)) }
        components.queryItems = scope + query
        guard let endpoint = components.string else { throw PairingFailure.invalidLink }
        return endpoint
    }

    func hasOlderProjectSubagents(_ conversationID: String) -> Bool {
        projectSubagentNextCurrentCursor[conversationID] != nil
            || projectSubagentNextArchivedCursor[conversationID] != nil
    }

    func loadOlderProjectSubagents(_ parent: ChatSummary) async {
        guard isProject(parent), !previewMode, let saved = connection, !accessEnded,
              projectSubagentAvailability[parent.id] == true,
              !loadingOlderProjectSubagents.contains(parent.id),
              let token = projectSubagentLoadTokens[parent.id] else { return }
        let archived: Bool
        let cursor: String
        if let current = projectSubagentNextCurrentCursor[parent.id] {
            archived = false; cursor = current
        } else if let older = projectSubagentNextArchivedCursor[parent.id] {
            archived = true; cursor = older
        } else { return }
        let seenCursors = archived ? projectSubagentSeenArchivedCursors[parent.id] ?? []
                                   : projectSubagentSeenCurrentCursors[parent.id] ?? []
        guard !seenCursors.contains(cursor) else {
            if archived { projectSubagentNextArchivedCursor[parent.id] = nil }
            else { projectSubagentNextCurrentCursor[parent.id] = nil }
            projectSubagentErrors[parent.id] = "Older agent tasks could not be loaded. Refresh this chat to try again."
            return
        }
        let key = partition
        loadingOlderProjectSubagents.insert(parent.id)
        defer {
            if projectSubagentLoadTokens[parent.id] == token {
                loadingOlderProjectSubagents.remove(parent.id)
                if projectSubagentRefreshPending.remove(parent.id) != nil {
                    Task { [weak self] in await self?.loadProjectSubagents(parent) }
                }
            }
        }
        do {
            let path = try ProjectSubagentPaths.roster(parentConversationId: parent.id,
                                                        archived: archived, cursor: cursor)
            let response: ProjectSubagentList = try await api.request(path,
                origin: saved.origin, credential: saved.credential)
            guard key == partition, projectSubagentLoadTokens[parent.id] == token,
                  !Task.isCancelled,
                  (archived ? projectSubagentNextArchivedCursor[parent.id]
                            : projectSubagentNextCurrentCursor[parent.id]) == cursor else { return }
            let nextCursor = archived ? response.nextArchivedCursor : response.nextCurrentCursor
            guard response.available else { throw ReadFailure.resync }
            let existing = projectSubagents[parent.id] ?? []
            let verified = response.subagents.filter {
                $0.parentConversationId == parent.id && $0.isArchived == archived
            }
            let updates = Dictionary(verified.map { ($0.threadId, $0) },
                                     uniquingKeysWith: { _, newer in newer })
            var seen = Set(existing.map(\.threadId))
            let added = verified.filter { seen.insert($0.threadId).inserted }
            setProjectSubagents(existing.map { updates[$0.threadId] ?? $0 } + added, for: parent.id)
            projectSubagentFreshIDs[parent.id, default: []].formUnion(verified.map(\.threadId))
            projectSubagentExpandedParents.insert(parent.id)
            projectSubagentPageRevision[parent.id, default: 0] += 1
            var consumed = seenCursors
            consumed.insert(cursor)
            let nextIsCycle = nextCursor.map { consumed.contains($0) } ?? false
            let beyondVerifiedLimit = nextCursor != nil && consumed.count >= 9
            if beyondVerifiedLimit { projectSubagentPagingLimited.insert(parent.id) }
            if archived {
                projectSubagentSeenArchivedCursors[parent.id] = consumed
                projectSubagentNextArchivedCursor[parent.id] = nextIsCycle || beyondVerifiedLimit ? nil : nextCursor
            } else {
                projectSubagentSeenCurrentCursors[parent.id] = consumed
                projectSubagentNextCurrentCursor[parent.id] = nextIsCycle || beyondVerifiedLimit ? nil : nextCursor
            }
            projectSubagentErrors[parent.id] = nextIsCycle
                ? "Older agent tasks could not be loaded. Refresh this chat to try again."
                : projectSubagentPagingLimited.contains(parent.id)
                    ? "Some older agent tasks are beyond this computer's verified history limit."
                    : hasOlderProjectSubagents(parent.id) ? "More agent tasks are available." : nil
        } catch {
            guard key == partition, projectSubagentLoadTokens[parent.id] == token,
                  !Task.isCancelled else { return }
            projectSubagentErrors[parent.id] = "Older agent tasks could not be loaded. Try again."
        }
    }

    /// Stops one running Claude agent task or background command. The host
    /// rechecks ownership; the row shows progress, then "Stopped" or why not.
    func stopProjectSubagent(parent: ChatSummary, child: ProjectSubagentSummary) async {
        guard isProject(parent), !previewMode, child.parentConversationId == parent.id, child.isStoppable,
              stoppingProjectSubagents[parent.id]?.contains(child.threadId) != true,
              let saved = connection, !accessEnded else { return }
        let key = partition
        stoppingProjectSubagents[parent.id, default: []].insert(child.threadId)
        projectSubagentStopErrors[parent.id] = nil
        do {
            let path = try ProjectSubagentPaths.stop(parentConversationId: parent.id, threadId: child.threadId)
            let updated: ProjectSubagentSummary = try await api.request(path,
                origin: saved.origin, credential: saved.credential, method: "POST")
            guard key == partition else { return }
            if updated.threadId == child.threadId, updated.parentConversationId == parent.id {
                setProjectSubagents((projectSubagents[parent.id] ?? []).map { $0.threadId == updated.threadId ? updated : $0 },
                                    for: parent.id)
                projectSubagentFreshIDs[parent.id, default: []].insert(updated.threadId)
            }
        } catch {
            guard key == partition else { return }
            if case PairingFailure.hostMessage(_, let detail) = error { projectSubagentStopErrors[parent.id] = detail }
            else { projectSubagentStopErrors[parent.id] = "Couldn't stop “\(child.title)”. Check your connection and try again." }
        }
        stoppingProjectSubagents[parent.id]?.remove(child.threadId)
        // Reconcile with what Claude reports now, including a task that already finished.
        await loadProjectSubagents(parent, afterCurrentRead: true)
    }

    func projectSubagentTranscript(parent: ChatSummary, child: ProjectSubagentSummary,
                                   cursor: String? = nil) async throws -> ProjectSubagentTranscript {
        guard isProject(parent), child.parentConversationId == parent.id,
              projectSubagents[parent.id]?.contains(where: { $0.threadId == child.threadId }) == true,
              let saved = connection, !accessEnded else { throw PairingFailure.invalidLink }
        let key = partition
        let path = try ProjectSubagentPaths.transcript(parentConversationId: parent.id,
                                                       threadId: child.threadId, cursor: cursor,
                                                       kind: child.kind)
        let response: ProjectSubagentTranscript = try await api.request(path,
            origin: saved.origin, credential: saved.credential)
        guard key == partition, !Task.isCancelled else { throw CancellationError() }
        guard response.subagent.parentConversationId == parent.id,
              response.subagent.threadId == child.threadId,
              response.snapshot.thread.threadId == child.threadId else { throw ReadFailure.resync }
        return response
    }

    func connectionReady() async -> Bool {
        await preparation?.value
        if let connectionCheck { await connectionCheck.value }
        else if macConnected != true && !accessEnded && !Task.isCancelled { await check() }
        return macConnected == true && !accessEnded && !Task.isCancelled
    }

    /// Retries a conversation that has no saved content after a failed load.
    /// Quiet first-load retries before a chat with nothing saved shows a failure.
    private var conversationLoadRetries: [String: Int] = [:]
    func retryConversation(_ chat: ChatSummary) async {
        conversationLoadFailures[chat.id] = nil
        conversationLoadRetries[chat.id] = nil
        if macConnected != true { reconnectNow(); await loadChats(force: true) }
        else if chat.id.hasPrefix("history-") { await openReadOnlyHistory(chat, afterCurrentRead: true) }
        else { await refreshConversation(chat, afterCurrentRead: true) }
    }

    private func beginLoading(_ conversation: String) -> UUID {
        let token = UUID()
        loadingTokens[conversation] = token
        loadingConversationIDs.insert(conversation)
        return token
    }

    private func endLoading(_ conversation: String, _ token: UUID) {
        guard loadingTokens[conversation] == token else { return }
        loadingTokens[conversation] = nil
        loadingConversationIDs.remove(conversation)
        scheduleResidentTrim()
    }

    func presentConversation(_ chat: ChatSummary, root: ChatSummary? = nil) {
        if visibleChat?.id != chat.id { flushTypedDrafts() }
        selectedChat = root ?? chat
        visibleChat = chat
        #if WONDER_DIAGNOSTICS
        voice.recordPreviewAudioRoute("chat-presented")
        #endif
    }

    /// What a window open on `conversation` shows: the agent task opened over
    /// it, else the conversation itself. `selectedChat` alone is only the last
    /// chat opened in any window.
    func shownChat(in conversation: String) -> ChatSummary? {
        if selectedChat?.id == conversation, let visibleChat { return visibleChat }
        if let detail = projects.details[conversation] { return projectChat(detail) }
        #if WONDER_DIAGNOSTICS
        return chats.first { $0.id == conversation }
        #else
        return nil
        #endif
    }

    func dismissConversation(_ chat: ChatSummary) {
        flushTypedDraft(chat.id)
        GrowingReplies.removeAll()
        // Outgoing views can disappear after the next route has appeared.
        if visibleChat?.id == chat.id { visibleChat = nil }
    }

    /// Resolve read-only details without draft/send recovery. An explicit retry
    /// must read after an older GET, whose stale refusal cannot answer it.
    private func openReadOnlyHistory(_ chat: ChatSummary, afterCurrentRead: Bool = false) async {
        let scope = assignmentScope
        guard await connectionReady(), assignmentScope == scope, visibleChat?.id == chat.id else { return }
        do {
            if projects.details[chat.id] == nil { try await projects.loadDetail(chat.id) }
            guard !Task.isCancelled, assignmentScope == scope, visibleChat?.id == chat.id else { return }
            await refreshConversation(chat, afterCurrentRead: afterCurrentRead)
        } catch is CancellationError {
        } catch {
            guard !Task.isCancelled, assignmentScope == scope, visibleChat?.id == chat.id else { return }
            conversationLoadFailures[chat.id] = ChatOpenFailure(error, computer: macName)
        }
    }

    func open(_ chat: ChatSummary, root: ChatSummary? = nil, readOnly: Bool = false) async {
        presentConversation(chat, root: root)
        guard !previewMode else { return }
        #if WONDER_DIAGNOSTICS
        LatencyHops.mark("open.restore.start", chat.id)
        #endif
        await restoreSavedHistory(chat.id)
        #if WONDER_DIAGNOSTICS
        LatencyHops.mark("open.restore.done", chat.id, latencyShape(chat.id))
        #endif
        guard !Task.isCancelled, visibleChat?.id == chat.id else { return }
        if chat.id.hasPrefix("history-") || projects.details[chat.id]?.readOnlyReason != nil {
            // An outside-folder history address owns no draft, queue, approvals
            // or writable workspace, even before its detail cache has loaded.
            await openReadOnlyHistory(chat)
            return
        }
        loadComposer(chat.id)
        // Reopening retries a draft write that failed earlier, and a waiting
        // message whose save failed (its notice says to reopen the chat).
        retryOwedComposerWrite(chat.id)
        if macConnected == true, !(composers[chat.id]?.outbox ?? []).isEmpty { scheduleOutboxDrain() }
        if let data = try? store?.loadIntent(conversation: "async-list-" + chat.id), let items = try? JSONDecoder().decode([AsyncQuestion].self, from: data) {
            asyncQuestions[chat.id] = items
            restoreAsyncReplies(items)
        }
        // Saved history is already on screen. Network reads wait for the shared
        // launch/foreground check instead of racing credential renewal.
        guard await connectionReady(), visibleChat?.id == chat.id else { return }
        // A helper route needs its parent's ownership record before its
        // history; a root chat's history does not wait for its helpers.
        if isProject(chat), let root, root.id != chat.id {
            await loadProjectSubagents(chat)
            await refreshConversation(chat)
        } else if isProject(chat) {
            async let roster: Void = loadProjectSubagents(chat)
            await refreshConversation(chat)
            await roster
        } else {
            await refreshConversation(chat)
        }
        guard !projects.unavailable.contains(chat.id) else { return }
        workspaceReads[chat.id] = (snapshots[chat.id]?.lastSequence, .now)
        if isProject(chat) { Task { [weak self] in await self?.loadPullRequests(chat) }; Task { [weak self] in await self?.loadGitSummary(chat) }; Task { [weak self] in await self?.loadDevServers(chat) } }
        await loadAttention()
        try? await loadQueue(chat)
        if !readOnly, !chat.isArchived, composers[chat.id]?.pending?.receipt == nil, composers[chat.id]?.pending != nil {
            await deliver(chat)
        }
    }

    private func loadComposer(_ chat: String) {
        guard composers[chat] == nil, !previewMode else { return }
        do {
            guard let store else { return }
            var composer = try store.loadComposer(conversation: chat)
            #if WONDER_DIAGNOSTICS
            if DiagnosticSubagentFixture.recoveryCase != nil, !DiagnosticSubagentFixture.t3CodeLaunchFixture, chat == DiagnosticSubagentFixture.parentID,
               composer.draft.isEmpty, composer.attachmentCount == 0, composer.pending == nil, composer.outbox?.isEmpty != false {
                composer.draft = "Keep my recovery draft"
                composer.stagedFiles = [try StagedFile(name: "recovery.txt", mimeType: "text/plain",
                    data: Data("Recovery attachment must survive.".utf8))]
                try store.saveComposer(composer, conversation: chat)
            }
            #endif
            if let device = connection?.credential.deviceId, composer.preservePreviousIdentityPending(currentDevice: device) {
                try store.saveComposer(composer, conversation: chat)
            }
            composers[chat] = composer
            // A reopen that reads the draft resolves the earlier read failure.
            if intentLoadFailures.remove(chat) != nil, composerErrors[chat] == Self.intentReadFailure { composerErrors[chat] = nil }
        } catch {
            intentLoadFailures.insert(chat)
            composerErrors[chat] = Self.intentReadFailure
        }
    }
    private static let intentReadFailure = "Your saved message couldn’t be read. Reopen this chat to try again, or start over; a copy is kept."
    /// The saved draft could not be read: the editor is read-only (typing
    /// could not be saved over it) until a reopen reads it or the owner starts over.
    func draftUnreadable(_ chat: String) -> Bool { intentLoadFailures.contains(chat) }
    /// Start over after the saved draft could not be read. Its files are set
    /// aside unread first (`setAsideComposer`); if that fails nothing is removed.
    @discardableResult func startOverUnreadableDraft(_ chat: String) -> Bool {
        guard intentLoadFailures.contains(chat), !previewMode, let store else { return false }
        do { try store.setAsideComposer(conversation: chat) }
        catch {
            composerErrors[chat] = "Your saved message couldn’t be set aside, so it was kept. Free some storage and try again."
            return false
        }
        composers[chat] = nil
        loadComposer(chat)
        guard !intentLoadFailures.contains(chat) else { return false }
        composerErrors[chat] = nil
        return true
    }

    /// Creation and its first message share a durable request identity. Repeating
    /// this after a crash cannot start a second message on the recovered Project thread.
    func prepareCreationMessage(_ chat: ChatSummary, body: String, requestID: String, skills: [CodexSkillSelection]? = nil, fromOutbox: Bool = false) throws {
        guard let saved = connection, !accessEnded else { throw PairingFailure.response(401) }
        loadComposer(chat.id)
        guard !intentLoadFailures.contains(chat.id) else { throw ReadFailure.resync }
        var intent = composers[chat.id] ?? ComposerIntent()
        if let pending = intent.pending {
            guard pending.request.clientMessageId == requestID, pending.request.body == body, (pending.request.codexSkills ?? []) == (skills ?? []) else { throw SendFailure.pending }
            return
        }
        if snapshots[chat.id]?.messages.contains(where: { $0.clientMessageId == requestID }) == true { return }
        guard intent.draft.isEmpty || intent.draft == body else { throw SendFailure.pending }
        intent.draft = body
        intent.codexSkills = skills
        try intent.begin(device: saved.credential.deviceId, clientMessageID: requestID)
        if fromOutbox { intent.markPendingFromOutbox() }
        try saveComposer(intent, chat: chat.id)
    }

    enum FillDraftResult: Equatable {
        case saved
        case occupied
        case saveFailed(String)
    }

    /// Recheck at the durable owner. Only the identical unsaved proposal may
    /// retry a failed write; no caller may replace another draft or intent.
    func fillEmptyDraft(_ text: String, chat: String) -> FillDraftResult {
        loadComposer(chat)
        guard !intentLoadFailures.contains(chat), let intent = composers[chat] else {
            return .saveFailed(composerErrors[chat] ?? "This draft could not be loaded. Reopen the chat before trying again.")
        }
        let retriesFailedSave = intent.draft == text && composerErrors[chat] != nil
        guard (intent.draft.isEmpty || retriesFailedSave), intent.pending == nil, intent.attachmentCount == 0,
              !uploading.contains(chat), !preparingSends.contains(chat) else { return .occupied }
        editDraft(text, chat: chat)
        if let error = composerErrors[chat] { return .saveFailed(error) }
        return composers[chat]?.draft == text ? .saved : .occupied
    }

    /// Typing: the text is in memory (and on screen) at once and written
    /// after a short pause rather than on every key. Send, Guide, leaving the
    /// chat, backgrounding, a pairing change and termination write it first
    /// (`flushTypedDrafts`).
    func typeDraft(_ text: String, chat: String) {
        loadComposer(chat)
        // A chat's first draft creates its composer durably at once.
        guard composers[chat] != nil else { editDraft(text, chat: chat); return }
        guard !intentLoadFailures.contains(chat), !uploading.contains(chat), !preparingSends.contains(chat),
              var next = composers[chat], next.draft != text else { return }
        let facts = ComposerDraftFacts(next.draft)
        next.draft = text
        if ComposerDraftFacts(text) == facts {
            // Nothing outside the editor reads this keystroke.
            composerIntents[chat] = next
            draftTexts[chat]?.text = text
        } else {
            composers[chat] = next
        }
        unsavedDrafts.insert(chat)
        typedDraftSaves[chat]?.cancel()
        typedDraftSaves[chat] = Task { [weak self] in
            try? await Task.sleep(for: Self.typedDraftDelay)
            guard !Task.isCancelled else { return }
            self?.flushTypedDraft(chat)
        }
    }
    static let typedDraftDelay: Duration = .milliseconds(300)
    private var typedDraftSaves: [String: Task<Void, Never>] = [:]
    /// Chats whose typed text is newer than their saved draft. A chat leaves
    /// only when a write of its current text succeeds, so a failed or
    /// postponed write is retried by the next flush.
    private var unsavedDrafts: Set<String> = []
    func flushTypedDrafts() {
        for chat in Array(unsavedDrafts) { flushTypedDraft(chat) }
        for id in Array(typedAnswerDrafts.keys) { flushAnswerDraft(id) }
    }
    /// Writes typed text through the small draft overlay. It never rewrites
    /// the intent (staged files, a pending send), so it also runs while a
    /// send or upload prepares, when the editor itself is locked.
    private func flushTypedDraft(_ chat: String) {
        typedDraftSaves.removeValue(forKey: chat)?.cancel()
        guard unsavedDrafts.contains(chat) else { return }
        guard let text = composers[chat]?.draft else { unsavedDrafts.remove(chat); return }
        // An unreadable saved intent is never overwritten; its text stays on screen.
        guard !intentLoadFailures.contains(chat) else { return }
        // A failed full write is retried whole: the overlay alone would not
        // save the rest of what is on screen.
        if owedComposerWrites[chat]?.write == .intent, !uploading.contains(chat), !preparingSends.contains(chat) {
            retryOwedComposerWrite(chat)
            return
        }
        do {
            if !previewMode {
                guard let store else { throw ReadFailure.resync }
                try store.saveComposerDraft(text, conversation: chat)
            }
            unsavedDrafts.remove(chat)
            // The overlay proves only the text; an owed full write stays owed.
            if owedComposerWrites[chat]?.write == .draft { composerIntentSaved(chat) }
        } catch { composerWriteFailed(chat, .draft, Self.draftSaveFailure) }
    }
    private static let draftSaveFailure = "This draft could not be saved. Free some storage before sending."
    /// The outgoing pairing's typed text was flushed to its own store.
    private func forgetTypedDrafts() {
        for task in typedDraftSaves.values { task.cancel() }
        typedDraftSaves = [:]; unsavedDrafts = []; owedComposerWrites = [:]; owedQueueWrites = [:]
        for task in answerDraftSaves.values { task.cancel() }
        answerDraftSaves = [:]; typedAnswerDrafts = [:]
    }

    /// A durable composer write that failed and is still owed: the typed-text
    /// overlay or the whole intent (draft, files, a refused send). Send, Guide,
    /// typing and reopening the chat retry it; only a write that covers it
    /// clears it and its reason.
    enum ComposerWrite: Equatable { case draft, intent }
    private var owedComposerWrites: [String: (write: ComposerWrite, message: String)] = [:]
    func owedComposerWrite(_ chat: String) -> ComposerWrite? { owedComposerWrites[chat]?.write }
    /// A waiting message's own change (its promotion to the send, or its
    /// refusal) that failed to save. A whole-intent save never covers it: the
    /// change was not kept in memory, so only the outbox drain redoes it, and
    /// only that change's own save clears it.
    private var owedQueueWrites: [String: String] = [:]
    func owesQueueWrite(_ chat: String) -> Bool { owedQueueWrites[chat] != nil }
    private static let queueSaveFailure = "A waiting message couldn’t be saved. Free some storage, then reopen this chat to try again."
    private func queueWriteFailed(_ chat: String) {
        owedQueueWrites[chat] = Self.queueSaveFailure
        composerErrors[chat] = Self.queueSaveFailure
    }
    private func queueWriteSaved(_ chat: String) {
        guard let message = owedQueueWrites.removeValue(forKey: chat) else { return }
        if composerErrors[chat] == message { composerErrors[chat] = owedComposerWrites[chat]?.message }
    }

    private func composerWriteFailed(_ chat: String, _ write: ComposerWrite, _ message: String) {
        // A failed whole-intent write stays owed when a later overlay write fails too.
        let owed: ComposerWrite = owedComposerWrites[chat]?.write == .intent ? .intent : write
        owedComposerWrites[chat] = (owed, message)
        composerErrors[chat] = message
    }
    /// The whole intent is on disk now, so any owed write is done.
    private func composerIntentSaved(_ chat: String) {
        guard let owed = owedComposerWrites.removeValue(forKey: chat) else { return }
        if composerErrors[chat] == owed.message { composerErrors[chat] = owedQueueWrites[chat] }
    }
    /// Another action succeeded. A notice about an action that left nothing
    /// unsaved goes; an owed write or unreadable draft keeps its reason.
    private func clearComposerNotice(_ chat: String) {
        guard !intentLoadFailures.contains(chat) else { return }
        composerErrors[chat] = owedComposerWrites[chat]?.message ?? owedQueueWrites[chat]
    }
    /// Retries the write a failure left owed, so Send never stays blocked by
    /// a write that would now succeed. False keeps its reason showing, again
    /// if it was dismissed.
    @discardableResult func retryOwedComposerWrite(_ chat: String) -> Bool {
        if intentLoadFailures.contains(chat) {
            loadComposer(chat)
            return !intentLoadFailures.contains(chat)
        }
        // A waiting message's failed change is redone by the drain; it does
        // not hold up a new message, which waits behind it in order.
        if owedQueueWrites[chat] != nil {
            composerErrors[chat] = composerErrors[chat] ?? owedQueueWrites[chat]
            scheduleOutboxDrain()
        }
        guard let owed = owedComposerWrites[chat] else { return true }
        guard let intent = composers[chat] else { owedComposerWrites[chat] = nil; return true }
        do {
            try saveComposer(intent, chat: chat)
            return true
        } catch {
            composerErrors[chat] = owed.message
            return false
        }
    }
    /// The owner closed the reason. An owed write stays owed and Send
    /// retries it; a repeat of the same failure shows the reason again.
    func dismissComposerNotice(_ chat: String) { composerErrors[chat] = nil }
    func dismissControlError(_ chat: String) { controlErrors[chat] = nil }

    func editDraft(_ text: String, chat: String) {
        loadComposer(chat)
        guard !intentLoadFailures.contains(chat), !uploading.contains(chat), !preparingSends.contains(chat) else { return }
        typedDraftSaves.removeValue(forKey: chat)?.cancel()
        var next = composers[chat] ?? ComposerIntent()
        next.draft = text
        let skillsChanged = next.codexSkills != composers[chat]?.codexSkills
        // Keep the text in memory even if disk is full; Send saves it first.
        composers[chat] = next
        unsavedDrafts.insert(chat)
        // Existing files remain durable while an import is prepared. Text must
        // not rewrite their bodies or invalidate preparation, unless a failed
        // whole write is owed and has to be retried.
        let overlayOnly = !previewMode && !skillsChanged && owedComposerWrites[chat]?.write != .intent
            && (loadingPhotos.contains(chat) || (next.stagedFiles ?? []).reduce(0, { $0 + $1.data.count }) + next.outboxBytes > 128 * 1024)
        do {
            if overlayOnly {
                guard let store else { throw ReadFailure.resync }
                try store.saveComposerDraft(text, conversation: chat)
                if owedComposerWrites[chat]?.write == .draft { composerIntentSaved(chat) }
            } else {
                try saveComposer(next, chat: chat)
            }
            unsavedDrafts.remove(chat)
            clearComposerNotice(chat)
        }
        catch { composerWriteFailed(chat, overlayOnly ? .draft : .intent, Self.draftSaveFailure) }
    }

    func removeSkill(_ path: String, chat: String) {
        guard var next = composers[chat], !uploading.contains(chat), !preparingSends.contains(chat) else { return }
        var text = next.draft, skills = next.codexSkills
        CodexSkillSelection.remove(path: path, text: &text, skills: &skills)
        next.draft = text; next.codexSkills = skills
        do { try saveComposer(next, chat: chat) }
        catch { composerErrors[chat] = "This skill could not be removed. Free some storage and try again." }
    }

    func prepareCreation(_ chat: ChatSummary, body: String, requestID: String, files: [StagedFile], skills: [CodexSkillSelection]? = nil, fromOutbox: Bool = false) async throws {
        loadComposer(chat.id)
        guard !intentLoadFailures.contains(chat.id) else { throw ReadFailure.resync }
        // The first message may already be reconciled while its New Chat draft
        // is still awaiting retirement. Do not restage accepted bytes on replay.
        if composers[chat.id]?.pending != nil ||
            snapshots[chat.id]?.messages.contains(where: { $0.clientMessageId == requestID }) == true {
            try prepareCreationMessage(chat, body: body, requestID: requestID, skills: skills, fromOutbox: fromOutbox)
            return
        }
        var intent = composers[chat.id] ?? ComposerIntent()
        guard intent.draft.isEmpty || intent.draft == body else { throw SendFailure.pending }
        intent.draft = body
        intent.codexSkills = skills
        let existing = intent.stagedFiles ?? []
        guard existing.allSatisfy({ item in files.contains { $0.id == item.id } }) else { throw SendFailure.pending }
        intent.stagedFiles = files.map { file in existing.first { $0.id == file.id } ?? file }
        let scope = assignmentScope
        guard let store else { throw ReadFailure.resync }
        let revision = try store.intentRevision(conversation: chat.id)
        let prepared = try await ComposerPersistenceWorker.shared.prepare(intent, store: store, conversation: chat.id, expectedRevision: revision)
        guard scope == assignmentScope, !accessEnded, !Task.isCancelled else { throw CancellationError() }
        try prepared.commit()
        composers[chat.id] = intent
        composerIntentSaved(chat.id)
        try await uploadStaged(chat)
        try prepareCreationMessage(chat, body: body, requestID: requestID, skills: skills, fromOutbox: fromOutbox)
    }

    private func saveComposer(_ value: ComposerIntent, chat: String) throws {
        if !previewMode {
            guard let store else { throw ReadFailure.resync }
            try store.saveComposer(value, conversation: chat)
        }
        // What is on screen is now what is saved, typed text included.
        typedDraftSaves.removeValue(forKey: chat)?.cancel(); unsavedDrafts.remove(chat)
        composers[chat] = value
        composerIntentSaved(chat)
    }

    private enum AttachmentPersistenceFailure: Error { case storage, changed }

    /// The candidate stays invisible until durable. Attachment/send changes
    /// invalidate its snapshot; text is rebound through the small draft overlay
    /// without encoding or writing the attachment bodies again.
    private func persistAttachments(_ files: [StagedFile], chat: ChatSummary,
                                    key: String?, scope: String, contextID: UUID, request: UUID) async throws -> Bool {
        loadComposer(chat.id)
        guard !intentLoadFailures.contains(chat.id) else { throw AttachmentPersistenceFailure.storage }
        guard let store else { throw AttachmentPersistenceFailure.storage }
        // Not fenced on the transport generation: going inactive (Control
        // Center, a call banner) restarts reading but must not drop a photo the
        // person already chose. Pairing, access and navigation changes all
        // replace `cameraContextID` or `partition`, which still end the import.
        func ownsImport() -> Bool {
            !Task.isCancelled && key == partition && scope == assignmentScope
                && contextID == cameraContextID && photoImportIDs[chat.id] == request
                && visibleChat?.id == chat.id && canAttach(chat)
        }
        for _ in 0..<3 {
            try Task.checkCancellation()
            guard ownsImport() else { return false }
            var next = composers[chat.id] ?? ComposerIntent()
            let mutation = next.nonTextMutationID
            let stagedIDs = next.attachmentIDs
            guard next.attachmentCount + files.count <= 4 else { throw FileFailure.tooLarge }
            guard !files.contains(where: { stagedIDs.contains($0.id) }) else { throw FileFailure.integrity }
            next.stagedFiles = (next.stagedFiles ?? []) + files
            let prepared: PreparedComposer
            do {
                let revision = try store.intentRevision(conversation: chat.id)
                #if WONDER_DIAGNOSTICS
                attachmentPersistenceWillPrepare?()
                #endif
                prepared = try await ComposerPersistenceWorker.shared.prepare(next, store: store, conversation: chat.id,
                                                                             expectedRevision: revision)
            } catch is CancellationError { throw CancellationError() }
            catch { throw AttachmentPersistenceFailure.storage }
            try Task.checkCancellation()
            guard ownsImport() else { return false }
            guard let current = composers[chat.id], mutation == current.nonTextMutationID,
                  current.attachmentIDs == stagedIDs else { continue }
            do { try prepared.commit(draft: current.draft == next.draft ? nil : current.draft) }
            catch FileFailure.stale { continue }
            catch is CancellationError { throw CancellationError() }
            catch { throw AttachmentPersistenceFailure.storage }
            // No suspension between the final ownership check, atomic rename
            // and publication. Send sees exactly the intent that was committed.
            next.draft = current.draft
            composers[chat.id] = next
            composerIntentSaved(chat.id)
            clearComposerNotice(chat.id)
            return true
        }
        throw AttachmentPersistenceFailure.changed
    }

    private func attachmentFailure(_ error: Error, fallback: String) -> String {
        switch error {
        case FileFailure.tooLarge: return AttachmentUploadFailure.message(for: error)
        case AttachmentPersistenceFailure.storage:
            return "Could not save this media. Free some storage and try attaching them again."
        case AttachmentPersistenceFailure.changed:
            return "Your draft changed while saving this media. Try attaching them again."
        default: return fallback
        }
    }

    private func finishPhotoImport(_ request: UUID, chat: String) {
        guard photoImportIDs[chat] == request else { return }
        photoImportIDs.removeValue(forKey: chat)
        loadingPhotos.remove(chat)
    }

    /// Whether a rewound message could be put back in this chat's draft now.
    /// Checked before asking the Mac, so the message is never lost. An earlier
    /// edit the Mac has not confirmed is checked first, never repeated.
    func canRestoreRewound(_ chat: String) -> Bool {
        loadComposer(chat)
        return !intentLoadFailures.contains(chat) && !uploading.contains(chat) && !preparingSends.contains(chat)
            && composers[chat]?.pending == nil && composers[chat]?.rewindRecovery == nil
    }

    /// An edit the Mac may have done, kept in this chat's draft until history shows.
    func unconfirmedRewind(_ chat: String) -> RewindRecovery? { composers[chat]?.rewindRecovery }

    enum RewindStartFailure: Error { case unresolved, draftBusy, notSaved }

    /// Edit from here, before anything is sent: the selected message and its
    /// attachments are saved with the draft, beside whatever is typed there. If
    /// that cannot be saved the Mac is not asked, so the message cannot be lost.
    func beginRewind(_ chat: ChatSummary, turnID: String, body: String, attachmentIDs: [String], restoreFiles: Bool) throws -> RewindRecovery {
        loadComposer(chat.id)
        guard composers[chat.id]?.rewindRecovery == nil else { throw RewindStartFailure.unresolved }
        guard canRestoreRewound(chat.id) else { throw RewindStartFailure.draftBusy }
        var next = composers[chat.id] ?? ComposerIntent()
        let recovery = RewindRecovery(turnID: turnID, body: body, attachmentIDs: attachmentIDs, restoreFiles: restoreFiles,
                                      historyRevision: snapshots[chat.id]?.historyRevision ?? 0)
        guard next.beginRewind(recovery) else { throw RewindStartFailure.unresolved }
        do { try saveComposer(next, chat: chat.id) } catch { throw RewindStartFailure.notSaved }
        return recovery
    }

    /// The Mac rewound: the saved message returns to the draft (once), and the
    /// chat reloads at the Mac's new history revision.
    func finishRewind(_ chat: ChatSummary, recovery: RewindRecovery) async {
        settleRewind(chat, saveFailure: "Your message is back in the draft but couldn’t be saved. Free some storage before sending.") { $0.finishRewind(id: recovery.id) }
        await refreshConversation(chat, afterCurrentRead: true)
    }

    /// The Mac refused: nothing changed, so the saved copy is dropped and the draft stays as it was.
    func abandonRewind(_ chat: ChatSummary, recovery: RewindRecovery) {
        settleRewind(chat, saveFailure: "Your draft couldn’t be saved. Free some storage before sending.") { $0.abandonRewind(id: recovery.id) }
    }

    /// The Mac answered, but not with the result. Its own answer is what lets a
    /// history that did not move prove the edit did not happen.
    func rewindAnswered(_ chat: ChatSummary, recovery: RewindRecovery) {
        guard var next = composers[chat.id], next.rewindRecovery?.id == recovery.id else { return }
        next.markRewindAnswered(id: recovery.id)
        do { try saveComposer(next, chat: chat.id) } catch { composers[chat.id] = next }
    }

    /// Reads the chat again; an unconfirmed edit resolves against what it shows.
    func checkUnconfirmedRewind(_ chat: ChatSummary) async {
        await refreshConversation(chat, afterCurrentRead: true)
    }

    private func settleRewind(_ chat: ChatSummary, saveFailure: String, _ change: (inout ComposerIntent) -> Bool) {
        loadComposer(chat.id)
        var next = composers[chat.id] ?? ComposerIntent()
        guard change(&next) else { return }
        do { try saveComposer(next, chat: chat.id); clearComposerNotice(chat.id) }
        catch {
            // Keep it on screen; Send saves it first and stops if that fails again. The saved
            // recovery still names the edit, so a restart reconciles it against history.
            composers[chat.id] = next
            composerWriteFailed(chat.id, .intent, saveFailure)
        }
    }

    /// History is in: an edit the phone could not confirm is decided by it. The
    /// message returns to the draft once, or the edit is dropped; undecided stays saved.
    private func reconcileRewind(_ chat: ChatSummary, page: ConversationSnapshot) {
        loadComposer(chat.id)
        guard let recovery = composers[chat.id]?.rewindRecovery else { return }
        let result = ConversationRewind.reconcile(recovery, snapshot: page)
        guard result != .undetermined else { return }
        settleRewind(chat, saveFailure: result == .rewound ? "Your message is back in the draft but couldn’t be saved. Free some storage before sending." : "Your draft couldn’t be saved. Free some storage before sending.") {
            result == .rewound ? $0.finishRewind(id: recovery.id) : $0.abandonRewind(id: recovery.id)
        }
        rewindResolutions[chat.id] = ConversationRewind.resolvedMessage(recovery, result, computer: macName)
    }

    func conversationWorking(_ chat: String) -> Bool {
        threadActivity(chat).isBusy
    }

    func activeTurn(_ chat: String) -> String? {
        snapshots[chat]?.activeTurnID
    }

    /// A Project turn started in Codex or Claude on the Mac. Wonder can't stop
    /// or steer it; a new message waits until it finishes, except where the
    /// Mac stops Claude's reply for it (`nativeStopsMacReply`).
    func turnRunsElsewhere(_ chat: String) -> Bool {
        snapshots[chat]?.activeTurnRunsElsewhere == true
    }

    func activeTurnIDs(_ chat: String) -> Set<String> {
        snapshots[chat]?.activeTurnIDs ?? []
    }

    func turn(_ turnID: String?, in chat: String) -> ReadTurn? {
        guard let turnID else { return nil }
        return snapshots[chat]?.thread.turns?.first(where: { $0.id == turnID })
    }
    /// Guide steers a running reply when the provider takes extra direction
    /// mid-turn (`steer`); otherwise a message sent during a reply is queued.
    /// T3 Code steers its own thread's turn, whatever the provider.
    func offersGuide(_ chat: ChatSummary) -> Bool {
        capabilities(chat)?.steer == true || t3CodeSends(chat.id)
    }
    /// The running reply Send now puts a queued message into, when it can:
    /// one Cupola runs, or, for a message T3 Code holds in its queue, the
    /// reply T3 Code runs in a thread Cupola sends into.
    func sendNowTurn(_ chat: ChatSummary, item: QueuedMessage) -> String? {
        guard offersGuide(chat), let turn = activeTurn(chat.id) else { return nil }
        if item.waitsInT3Code == true { return t3CodeSends(chat.id) ? turn : nil }
        return turnRunsElsewhere(chat.id) ? nil : turn
    }
    /// Reply capability, independent of whether the editor currently has words.
    func canSteerReply(_ chat: ChatSummary) -> Bool {
        offersGuide(chat) && activeTurn(chat.id) != nil && !offersTakeOver(chat)
            && (!turnRunsElsewhere(chat.id) || (t3CodeSends(chat.id) && t3CodeLimit(chat.id) == nil))
    }

    /// Only current, confirmed stream activity suppresses the offline notice;
    /// an open socket alone says nothing about its route or identity.
    var composerReceivesUpdates: Bool { confirmedCurrentStream }

    var composerOfflineNotice: String? {
        ComposerReachability.offlineNotice(macConnected: macConnected, receivingUpdates: composerReceivesUpdates)
    }

    func canGuide(_ chat: ChatSummary) -> Bool {
        !dictation.blocksSending(conversationID: chat.id) && offersGuide(chat) && !preparingSends.contains(chat.id) && !finishingDictation.contains(chat.id) && !savingComposerSettings.contains(chat.id) && !approvalSettingsBlockSending(chat.id) && !chat.isArchived && !uploading.contains(chat.id) && !loadingPhotos.contains(chat.id) && activeTurn(chat.id) != nil && connection != nil && !accessEnded
            && (!turnRunsElsewhere(chat.id) || (t3CodeSends(chat.id) && t3CodeLimit(chat.id) == nil))
            && usageLimitMessage(chat) == nil
            && ((composers[chat.id]?.codexSkills ?? []).isEmpty || capabilities(chat)?.skills == true)
            && !sending.contains(chat.id)
            // A failed draft write does not lock Guide: it retries the write first.
            && composers[chat.id]?.pending == nil
            && (!(composers[chat.id]?.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ?? true)
                || dictation.hasUncommittedWords(conversationID: chat.id)
                || (composers[chat.id]?.attachmentCount ?? 0) > 0)
            && (composers[chat.id]?.draft.utf8.count ?? 0) <= 65536
            && (composers[chat.id]?.attachmentCount ?? 0) <= 4
    }
    func guide(_ chat: ChatSummary) async {
        flushTypedDrafts()
        guard canGuide(chat), retryOwedComposerWrite(chat.id), await finishDictationForSending(chat) else { return }
        guard canGuide(chat), let saved = connection, let turn = activeTurn(chat.id) else { return }
        let scope = assignmentScope
        preparingSends.insert(chat.id)
        defer { if scope == assignmentScope { preparingSends.remove(chat.id) } }
        do {
            await refreshComposerUsage(chat)
            guard usageLimitMessage(chat) == nil else { return }
            var next = composers[chat.id] ?? ComposerIntent()
            try await uploadStaged(chat)
            // Settings being saved show their own progress and failure, and
            // ended access has its own screen.
            guard scope == assignmentScope, !accessEnded, !Task.isCancelled,
                  !savingComposerSettings.contains(chat.id), !approvalSettingsBlockSending(chat.id) else { return }
            if activeTurn(chat.id) != turn {
                // The reply ended while attachments uploaded: send the message
                // normally instead, as one fresh attempt once preparing ends.
                preparingSends.remove(chat.id)
                guard canSend(chat) else {
                    controlErrors[chat.id] = "The reply finished before your message was sent. Your message is still here; send it again."
                    return
                }
                await send(chat)
                return
            }
            guard connection?.origin == saved.origin else {
                controlErrors[chat.id] = "The connection to your computer changed before your message was sent. Your message is still here; send it again."
                return
            }
            guard composers[chat.id]?.pending == nil else {
                controlErrors[chat.id] = "Another message is still being sent. Your message is still here; send it when that one finishes."
                return
            }
            next = composers[chat.id] ?? next
            try next.begin(device: saved.credential.deviceId, expectedTurnId: turn)
            try indexPendingSend(chat.id)
            try saveComposer(next, chat: chat.id)
            if let id = next.pending?.request.clientMessageId { awaitAcceptance(id, chat: chat.id, as: .steering) }
            if await deliver(chat) == .retryLater, scope == assignmentScope { retryPendingSendLater() }
        } catch { controlErrors[chat.id] = "Couldn't send now. Your text is still here." }
    }
    /// Whether Stop is offered: a reply Cupola runs, or one T3 Code runs in a
    /// thread Cupola sends into (T3 Code stops it there).
    func offersStop(_ chat: ChatSummary) -> Bool {
        activeTurn(chat.id) != nil && (!turnRunsElsewhere(chat.id) || t3CodeSends(chat.id))
    }
    func stop(_ chat: ChatSummary) async {
        guard offersStop(chat) else { return }
        guard let turn = activeTurn(chat.id), let saved = connection, !accessEnded,
              isProject(chat), !stopping.contains(chat.id) else { return }
        let key = partition
        stopping.insert(chat.id); controlErrors[chat.id] = nil
        defer { if key == partition { stopping.remove(chat.id) } }
        do {
            // 204 (empty) means the turn already ended; 202 says Stop reached
            // the agent, which ends the turn itself a moment later.
            struct StopReply: Decodable, Sendable { let stopConfirmed: Bool? }
            let reply: StopReply = try await api.request("/api/v1/conversations/\(Self.escape(chat.id))/turns/\(Self.escape(turn))/interrupt",
                origin: saved.origin, body: Data("{}".utf8), credential: saved.credential)
            guard key == partition else { return }
            await refreshConversation(chat, afterCurrentRead: true)
            guard reply.stopConfirmed == false else { return }
            // Keep "Stopping" until this turn is no longer the running one.
            let deadline = ContinuousClock.now + stopConfirmationWait
            var checks = 0
            while key == partition, activeTurn(chat.id) == turn, !Task.isCancelled {
                guard ContinuousClock.now < deadline else {
                    let unconfirmed = "Stop was sent, but the agent hasn’t confirmed it yet. It may still be working."
                    controlErrors[chat.id] = unconfirmed
                    stopping.remove(chat.id)
                    // Keep watching (bounded) so the note goes away once the turn really ends.
                    let watchUntil = ContinuousClock.now + .seconds(600)
                    while key == partition, activeTurn(chat.id) == turn, !Task.isCancelled, ContinuousClock.now < watchUntil {
                        try? await Task.sleep(for: .seconds(2))
                    }
                    if key == partition, activeTurn(chat.id) != turn, controlErrors[chat.id] == unconfirmed {
                        controlErrors[chat.id] = nil
                    }
                    return
                }
                try? await Task.sleep(for: .milliseconds(250))
                checks += 1
                // Live updates normally end the turn; reread now and then in case they lag.
                if checks % 12 == 0, key == partition { await refreshConversation(chat, afterCurrentRead: true) }
            }
        } catch {
            guard key == partition else { return }
            controlErrors[chat.id] = Self.stopFailure(error, macConnected: macConnected)
            await refreshConversation(chat, afterCurrentRead: true)
        }
    }

    /// Why a Stop attempt failed. It never claims the reply stopped; the
    /// computer's reachability may be stale, so Stop stays available to retry.
    static func stopFailure(_ error: Error, macConnected: Bool?) -> String {
        if error is URLError || macConnected == false {
            return "Couldn’t reach your computer, so the reply wasn’t stopped. It may still be running there; try Stop again when your computer is back."
        }
        return "Stop wasn’t confirmed, so the reply may still be running. Try Stop again."
    }

    func usageModel(_ chat: ChatSummary) -> String {
        if let detail = projects.details[chat.id] { return detail.model ?? (detail.family == .claude ? "claude:haiku" : "") } // provider identity: default model
        return ""
    }

    func usageLimitMessage(_ chat: ChatSummary, model: String? = nil) -> String? {
        let selected = model ?? usageModel(chat)
        guard let family = agentFamily(chat), projects.capabilities(family).usageLimits else { return nil }
        guard let cached = usageCache[family]?[assignmentScope],
            let exhausted = cached.exhaustedWindow(model: selected) else { return nil }
        let provider = exhausted.id == "seven_day_sonnet" ? "Claude Sonnet" : exhausted.id == "seven_day_opus" ? "Claude Opus" : family.title
        return "\(provider) usage limit reached. Try another model or wait for usage to reset."
    }

    func refreshComposerUsage(_ chat: ChatSummary) async {
        guard !previewMode, !chat.isArchived else { return }
        guard let family = agentFamily(chat) else { return }
        try? await loadUsage(family: family, maxAge: 60)
        // Drop expired gates even if the refresh is offline. A stale observation
        // cannot permanently disable Send after the provider's reset time.
        if let entry = usageCache[family]?[assignmentScope], Self.usageGateExpired(entry) {
            usageCache[family]?[assignmentScope] = nil
        }
    }

    /// Whether a cached exhausted-usage observation no longer gates Send: it
    /// is old, or every exhausted window has reached its reset time.
    nonisolated static func usageGateExpired(_ entry: CodexUsageCacheEntry, now: Date = Date()) -> Bool {
        entry.response.windows.contains(where: { $0.usedPercent >= 100 })
            && (now.timeIntervalSince(entry.fetchedAt) >= 300
                || !entry.response.windows.contains(where: { $0.usedPercent >= 100 && !$0.hasReset(now: now) }))
    }

    func skillFamily(_ chat: ChatSummary) -> AgentFamily? { projectNextModels[chat.id]?.family ?? agentFamily(chat) }
    /// Whether the provider of the next reply takes typed skills.
    func nextReplyTakesSkills(_ chat: ChatSummary) -> Bool { skillFamily(chat).map { projects.capabilities($0).skills } == true }

    func canSend(_ chat: ChatSummary) -> Bool {
        let draft = composers[chat.id]?.draft ?? ""
        let queues = sendsToOutbox(chat)
        return !dictation.blocksSending(conversationID: chat.id) && !preparingSends.contains(chat.id) && !finishingDictation.contains(chat.id) && !savingComposerSettings.contains(chat.id) && !approvalSettingsBlockSending(chat.id) && !chat.isArchived && projects.details[chat.id]?.readOnlyReason == nil && isProject(chat) && connection != nil && !accessEnded
            // An unreachable Mac does not lock Send: the message waits in the
            // thread's outbox. Every refusal is reported on the message itself.
            // While the Mac app runs this conversation, the Mac holds a sent
            // message in its queue and delivers it when that turn finishes.
            && usageLimitMessage(chat) == nil
            && ((composers[chat.id]?.codexSkills ?? []).isEmpty || nextReplyTakesSkills(chat))
            // Sending has durable host-side acceptance.
            // Replay invalidation does not revoke permission to submit intent.
            && snapshots[chat.id] != nil
            // A waiting message goes after the thread's earlier pending send.
            && (queues || composers[chat.id]?.pending == nil)
            && !sending.contains(chat.id) && !intentLoadFailures.contains(chat.id)
            // A failed draft write does not lock Send (its reason stays visible):
            // Send retries the write first and stops if it fails again.
            && (!draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || dictation.hasUncommittedWords(conversationID: chat.id)
                || !(composers[chat.id]?.stagedFiles ?? []).isEmpty || !(composers[chat.id]?.draftAttachmentIds ?? []).isEmpty) && draft.utf8.count <= 65536
            && (composers[chat.id]?.attachmentCount ?? 0) <= 4
            && (attachmentsSupported(chat) || ((composers[chat.id]?.stagedFiles ?? []).isEmpty && (composers[chat.id]?.draftAttachmentIds ?? []).isEmpty))
            && !uploading.contains(chat.id) && !loadingPhotos.contains(chat.id)
            && (queues || (!nativeHistoryRefreshing.contains(chat.id) && !nativeHistoryFailures.contains(chat.id)))
    }

    /// Send puts the message in the thread's outbox while the Mac is
    /// unreachable, and behind messages this pairing already has waiting
    /// there, so a newer message never overtakes them.
    func sendsToOutbox(_ chat: ChatSummary) -> Bool {
        guard let device = connection?.credential.deviceId else { return false }
        return macConnected == false || composers[chat.id]?.outbox?.contains(where: { $0.deviceId == device }) == true
    }

    /// Send or Guide during dictation, including while the recognizer drains after
    /// the microphone stopped, commits the finished transcript first. The send is
    /// marked as finishing dictation meanwhile, so another tap cannot send twice.
    private func finishDictationForSending(_ chat: ChatSummary) async -> Bool {
        guard dictation.ownsCapture(conversationID: chat.id) else { return true }
        let scope = assignmentScope
        finishingDictation.insert(chat.id)
        defer { if scope == assignmentScope { finishingDictation.remove(chat.id) } }
        return await dictation.finishForSending(conversationID: chat.id) && scope == assignmentScope && !Task.isCancelled
    }

    enum SendRecoveryReason {
        case signedOut(String), modelUnavailable(String)
        var message: String {
            switch self { case .signedOut(let text), .modelUnavailable(let text): text }
        }
        var needsModel: Bool { if case .modelUnavailable = self { true } else { false } }
    }

    /// Check the computer's current provider before changing durable send intent.
    /// A failed read is not evidence of sign-out; transport recovery still owns it.
    func sendRecoveryReason(family: AgentFamily?, selectedModel: String?, refresh: Bool = false) async -> SendRecoveryReason? {
        guard macConnected == true, !previewMode, let family else { return nil }
        let scope = assignmentScope
        var providers: MacProvidersResponse? = try? await manage("/api/v1/providers" + (refresh ? "?refresh=true" : ""))
        guard scope == assignmentScope, !Task.isCancelled else { return nil }
        // A cached sign-out may predate signing in on the computer. Confirm
        // it before refusing this draft; ordinary signed-in reads stay cheap.
        if !refresh, providers?.providers.first(where: { $0.family == family })?.state == "signedOut" {
            let current: MacProvidersResponse? = try? await manage("/api/v1/providers?refresh=true")
            guard scope == assignmentScope, !Task.isCancelled else { return nil }
            if let current { providers = current }
        }
        if providers?.providers.first(where: { $0.family == family })?.state == "signedOut" {
            return .signedOut("\(family.title) is signed out. Sign in to \(family.title) on \(macName), then try again.")
        }
        guard let selectedModel else { return nil }
        let options: AgentOptions? = try? await manage("/api/v1/agent-options")
        guard scope == assignmentScope, !Task.isCancelled else { return nil }
        if let options { projects.options = options }
        if let options, !options.models.contains(where: { !$0.hidden && $0.family == family && $0.id == selectedModel }) {
            return .modelUnavailable("This model is no longer available on \(macName). Choose another model.")
        }
        return nil
    }

    /// `takeOver`: Send now on a conversation another app on the Mac holds;
    /// the Mac stops that app's process for it, then sends.
    func send(_ chat: ChatSummary, takeOver: Bool = false) async {
        #if WONDER_DIAGNOSTICS
        LatencyHops.mark("send.tap", chat.id)
        LatencyHops.sendArmed.insert(chat.id)
        #endif
        flushTypedDrafts()
        guard canSend(chat), retryOwedComposerWrite(chat.id), await finishDictationForSending(chat) else { return }
        guard canSend(chat), let saved = connection else { return }
        if sendsToOutbox(chat) { enqueue(chat, device: saved.credential.deviceId); return }
        let scope = assignmentScope
        preparingSends.insert(chat.id)
        controlErrors[chat.id] = nil
        defer { if scope == assignmentScope { preparingSends.remove(chat.id) } }
        // T3 Code owns its provider/model catalog. Its refusal/queue path
        // remains authoritative for a thread held there or started there
        // (OpenCode/Cursor via T3 use models this computer's own list lacks;
        // a Claude/Codex chat T3 does not hold keeps this computer's check).
        let family = skillFamily(chat)
        let detail = projects.details[chat.id]
        let startedInT3 = detail?.t3CodePermissions != nil && detail?.hasNativeSession == false
        if nativeHolders[chat.id]?.app != "T3 Code", !startedInT3 {
            let selected = projectNextModels[chat.id]?.model ?? projects.details[chat.id]?.model
            let reason = await sendRecoveryReason(family: family, selectedModel: selected)
            guard scope == assignmentScope, !Task.isCancelled, family == skillFamily(chat),
                  selected == (projectNextModels[chat.id]?.model ?? projects.details[chat.id]?.model) else { return }
            if let reason { controlErrors[chat.id] = reason.message; return }
        }
        #if WONDER_DIAGNOSTICS
        LatencyHops.mark("send.checks.done", chat.id)
        #endif
        if isProject(chat) {
            // A full re-read lists every item of the thread; skip it when the
            // cheap activity check shows the screen already ends with the
            // Mac's newest turn, finished or the reply it follows live.
            let history = await nativeHistoryCurrent(chat)
            if !history.current { await reloadNativeHistory(chat, revision: history.revision) }
            guard scope == assignmentScope, !nativeHistoryFailures.contains(chat.id), !Task.isCancelled else { return }
        }
        #if WONDER_DIAGNOSTICS
        LatencyHops.mark("send.history.done", chat.id)
        #endif
        forgetRemovedUploads(chat.id)
        do { try await uploadStaged(chat) }
        catch {
            guard assignmentScope == scope else { return }
            controlErrors[chat.id] = uploadFailures[chat.id] == nil ? AttachmentUploadFailure.message(for: error) : nil
            return
        }
        guard assignmentScope == scope, !accessEnded else { return }
        let selectedModel = usageModel(chat)
        // The limit check uses the usage already on hand (the composer keeps it
        // fresh); reading it again can take the Mac seconds (Claude starts a
        // process to ask), so it refreshes after the send instead of before it.
        // A message past a limit the phone has not seen yet is refused by the
        // provider and says so.
        if let usageFamily = agentFamily(chat) {
            Task { [weak self] in try? await self?.loadUsage(family: usageFamily, maxAge: 60) }
        }
        // Each check after the upload keeps the text and says why it stopped.
        // The composer shows a usage limit, settings being saved show their
        // own progress and failure, and ended access has its own screen.
        guard usageLimitMessage(chat, model: selectedModel) == nil else { return }
        guard selectedModel == usageModel(chat) else {
            controlErrors[chat.id] = "The model changed before your message was sent. Your message is still here; check the model and send again."
            return
        }
        guard scope == assignmentScope, !accessEnded, !Task.isCancelled, !savingComposerSettings.contains(chat.id),
              !approvalSettingsBlockSending(chat.id) else { return }
        guard connection?.origin == saved.origin else {
            controlErrors[chat.id] = "The connection to your computer changed before your message was sent. Your message is still here; send it again."
            return
        }
        do {
            var next = composers[chat.id] ?? ComposerIntent()
            // T3 Code restarts its own turn with the message; nothing on the Mac stops.
            let restartsT3 = takeOver && offersT3TakeOver(chat)
            let takeOver = takeOver && (offersTakeOver(chat) || restartsT3)
            try next.begin(device: saved.credential.deviceId, projectModel: isProject(chat) ? projectNextModels[chat.id] : nil, takeOver: takeOver)
            // Decide where the message shows before it first appears.
            if let id = next.pending?.request.clientMessageId, restartsT3 { awaitAcceptance(id, chat: chat.id, as: .steering) }
            else if let id = next.pending?.request.clientMessageId, takeOver { awaitAcceptance(id, chat: chat.id, as: .takingOver) }
            else if let id = next.pending?.request.clientMessageId, sendWaits(chat, sending: next.pending?.request) { queuedSends[chat.id, default: []].insert(id) }
            else if let id = next.pending?.request.clientMessageId, sendStopsMacReply(chat) { awaitAcceptance(id, chat: chat.id, as: .stoppingMacReply) }
            try indexPendingSend(chat.id)
            try saveComposer(next, chat: chat.id)
        } catch {
            guard assignmentScope == scope else { return }
            composerErrors[chat.id] = managementError(error)
            return
        }
        preparingSends.remove(chat.id)
        if await deliver(chat) == .retryLater, scope == assignmentScope { retryPendingSendLater() }
    }

    /// A pending send is listed with the outbox before it is saved, so a send
    /// that fails (or a crash mid-request) is retried by the outbox drain from
    /// any chat, under its original client message ID and pairing.
    private func indexPendingSend(_ chat: String) throws {
        guard let store, !previewMode else { return }
        try store.indexOutbox(chat)
    }

    /// After a failed attempt the drain retries with backoff; reconnecting
    /// (the route recheck after a network error) drains at once.
    private func retryPendingSendLater() {
        guard !previewMode else { return }
        if outboxDrain != nil { outboxDrainAgain = true } else { scheduleOutboxRetry(key: partition) }
    }

    /// What became of one delivery attempt of the thread's pending send.
    enum DeliveryOutcome { case accepted, refused, retryLater }

    @discardableResult
    func deliver(_ chat: ChatSummary) async -> DeliveryOutcome {
        guard isProject(chat) else { return .retryLater }
        if composers[chat.id]?.pending?.rejected == true {
            do { var intent = composers[chat.id]!; try intent.restoreRejected(); try saveComposer(intent, chat: chat.id); controlErrors[chat.id] = nil }
            catch { controlErrors[chat.id] = "Clear or save your current draft before restoring the unsent message. Both are still saved." }
            return .refused
        }
        guard !sending.contains(chat.id), !accessEnded,
              let saved = connection, let pending = composers[chat.id]?.pending,
              pending.request.deviceId == saved.credential.deviceId else { return .retryLater }
        let key = partition
        let intentStore = store
        sending.insert(chat.id)
        defer { if partition == key { sending.remove(chat.id) } }
        do {
            let path = pending.request.expectedTurnId.map { "/api/v1/conversations/\(Self.escape(chat.id))/turns/\(Self.escape($0))/steer" } ?? "/api/v1/conversations/\(Self.escape(chat.id))/messages"
            #if WONDER_DIAGNOSTICS
            LatencyHops.mark("send.post", chat.id)
            #endif
            let receipt: SendReceipt = try await api.request(path,
                origin: saved.origin, body: JSONEncoder().encode(pending.request), credential: saved.credential)
            #if WONDER_DIAGNOSTICS
            LatencyHops.mark("send.posted", chat.id)
            #endif
            // A lifecycle change does not invalidate a send receipt, but unpairing does.
            guard partition == key, let intentStore, var next = composers[chat.id],
                  next.pending?.request.clientMessageId == pending.request.clientMessageId else { return .retryLater }
            try next.accept(receipt, conversation: chat.id)
            if let queued = receipt.queued, pending.request.expectedTurnId == nil {
                let id = pending.request.clientMessageId
                if queued { queuedSends[chat.id, default: []].insert(id) } else { queuedSends[chat.id]?.remove(id) }
            }
            try intentStore.saveComposer(next, conversation: chat.id)
            composers[chat.id] = next
            // A queued message gets its editable row before the conversation reloads.
            if receipt.queued == true, isProject(chat) { try? await loadQueue(chat, afterCurrentRead: true) }
            // Send now: the Mac first sends the messages that waited for the app it
            // stops. That app no longer holds the thread, so drop the held state
            // here; the holder check that runs beside the resulting turn
            // restores it if the Mac could not stop it. A Send now behind
            // those messages is queued after them, so its own receipt still says
            // queued. Take over in T3 Code's thread restarts its turn there:
            // T3 Code keeps the thread, so nothing changes hands.
            if pending.request.takeOver == true, isProject(chat),
               !(t3CodeSends(chat.id) && t3CodeLimit(chat.id, sending: pending.request) == nil) {
                for id in queuedSends[chat.id] ?? [] { awaitAcceptance(id, chat: chat.id, as: .takingOver) }
                queuedSends[chat.id] = nil
                nativeOpenElsewhere.remove(chat.id)
                nativeHolders[chat.id] = nil
                try? await loadQueue(chat, afterCurrentRead: true)
            }
            // The receipt was saved with the whole intent.
            composerIntentSaved(chat.id)
            clearComposerNotice(chat.id)
            if foreground { await refreshConversation(chat, afterCurrentRead: true) }
            return .accepted
        } catch {
            guard partition == key else { return .retryLater }
            // A refused message never looks sent: a composer send returns to
            // an empty draft, an outbox message to the front of its outbox.
            func refuse(_ reason: String, notice: String? = nil) -> DeliveryOutcome {
                queuedSends[chat.id]?.remove(pending.request.clientMessageId)
                stopAwaitingAcceptance(pending.request.clientMessageId, chat: chat.id)
                guard var intent = composers[chat.id],
                      intent.pending?.request.clientMessageId == pending.request.clientMessageId else { return .refused }
                intent.refuse(reason: reason)
                do { try saveComposer(intent, chat: chat.id) }
                catch {
                    // The returned message stays on screen and its write stays owed.
                    composers[chat.id] = intent
                    composerWriteFailed(chat.id, .intent, "The unsent message couldn’t be saved back to your draft. Free some storage before sending again.")
                }
                if pending.fromOutbox != true { controlErrors[chat.id] = notice ?? reason }
                return .refused
            }
            if case PairingFailure.skillRejected(_, let detail) = error { return refuse(detail) }
            if case PairingFailure.annotationRejected(_, let detail) = error {
                let outcome = refuse(detail, notice: detail + " Reopen the file preview to select its current version, then send again.")
                try? await loadFiles(chat)
                return outcome
            }
            if case PairingFailure.response(412) = error {
                if pending.request.expectedTurnId != nil { return refuse("That reply already finished. Your message is saved for editing.") }
                let outcome = refuse("Model settings changed. Your message was not sent. Check the model and send again.")
                await loadChats(force: true)
                return outcome
            }
            // The host's lasting 4xx (422 model, 409, 403, 404) answers every send, not only outbox ones.
            if case .refused(let reason) = Outbox.outcome(of: error) { return refuse(reason) }
            // The persisted pending request drives the message's retry indicator.
            // Draft-storage errors remain separate and must not be overwritten.
            if case PairingFailure.response(401) = error { readFailed(error, run: generation, origin: saved.origin, token: saved.credential.sessionToken) }
            if failureKind(error) == .route { _ = await recheckRoutes(excluding: saved.origin) }
            return .retryLater
        }
    }

    // MARK: - Outbox

    /// Outbox messages being uploaded or sent now; their Edit and Cancel wait.
    @Published private(set) var outboxInFlight: Set<String> = []
    private var outboxDrain: Task<Void, Never>?
    /// Which drain `outboxDrain` holds: a cancelled drain that finishes late must not clear its successor's handle.
    private var outboxDrainID = UUID()
    private var outboxRetry: Task<Void, Never>?
    private var outboxDrainAgain = false
    private var outboxAttempts = 0
    /// A drain runs or waits to retry.
    var isDrainingOutbox: Bool { outboxDrain != nil || outboxRetry != nil }

    /// Send while the Mac is unreachable (or behind messages already waiting):
    /// the draft becomes the thread's newest outbox message, saved before the
    /// composer clears.
    private func enqueue(_ chat: ChatSummary, device: String) {
        controlErrors[chat.id] = nil
        loadComposer(chat.id)
        guard !intentLoadFailures.contains(chat.id) else { return }
        var next = composers[chat.id] ?? ComposerIntent()
        do {
            try next.enqueue(device: device, projectModel: projectNextModels[chat.id])
            // The index lists the thread before its message exists on disk,
            // so a drain after a crash never misses it.
            if let store, !previewMode { try store.indexOutbox(chat.id) }
            try saveComposer(next, chat: chat.id)
        } catch let failure as Outbox.Failure {
            controlErrors[chat.id] = Outbox.explanation(failure)
            return
        } catch {
            composerErrors[chat.id] = "This message could not be saved. Free some storage and try again."
            return
        }
        if macConnected == true { scheduleOutboxDrain() }
    }

    func editOutboxMessage(_ id: String, chat: ChatSummary) {
        changeOutbox(id, chat: chat) { try $0.editOutbox(id: id) }
    }
    func cancelOutboxMessage(_ id: String, chat: ChatSummary) {
        changeOutbox(id, chat: chat) { $0.removeFromOutbox(id: id) }
    }
    func retryOutboxMessage(_ id: String, chat: ChatSummary) {
        changeOutbox(id, chat: chat) { try $0.retryOutbox(id: id) }
        outboxAttempts = 0
        scheduleOutboxDrain()
    }
    private func changeOutbox(_ id: String, chat: ChatSummary, _ change: (inout ComposerIntent) throws -> Void) {
        guard !outboxInFlight.contains(id), !preparingSends.contains(chat.id), !uploading.contains(chat.id) else { return }
        guard var next = composers[chat.id] else { return }
        do {
            try change(&next)
            try saveComposer(next, chat: chat.id)
            controlErrors[chat.id] = nil
        } catch let failure as Outbox.Failure {
            controlErrors[chat.id] = Outbox.explanation(failure)
        } catch {
            controlErrors[chat.id] = "This change could not be saved. Free some storage and try again."
        }
    }

    /// Drains every thread's outbox while the Mac is reachable: reconnect,
    /// returning to the foreground, a new waiting message and Retry start it.
    func scheduleOutboxDrain() {
        guard !previewMode, macConnected == true, !accessEnded, connection != nil, store != nil else { return }
        guard outboxDrain == nil else { outboxDrainAgain = true; return }
        outboxRetry?.cancel(); outboxRetry = nil
        let key = partition
        let drainID = UUID()
        outboxDrainID = drainID
        outboxDrain = Task { [weak self] in
            await self?.drainOutbox(key: key)
            guard let self, self.partition == key, self.outboxDrainID == drainID else { return }
            self.outboxDrain = nil
        }
    }

    private func cancelOutboxDrain() {
        outboxDrain?.cancel(); outboxDrain = nil
        outboxRetry?.cancel(); outboxRetry = nil
        outboxDrainAgain = false
        outboxInFlight = []
    }

    private enum OutboxProgress { case empty, waiting, retryLater }

    private func drainOutbox(key: String?) async {
        var retry = false
        repeat {
            outboxDrainAgain = false
            guard let store, partition == key else { return }
            let listed = (try? store.loadOutboxIndex()) ?? []
            var drained: Set<String> = []
            // Threads drain one after another; within a thread strictly in order.
            for id in listed {
                guard partition == key, macConnected == true, !accessEnded, !Task.isCancelled else { return }
                switch await drainConversation(id, key: key) {
                case .empty: drained.insert(id)
                case .waiting: break
                case .retryLater: retry = true
                }
            }
            guard partition == key, !Task.isCancelled else { return }
            if !drained.isEmpty, let latest = try? store.loadOutboxIndex() {
                try? store.saveOutboxIndex(latest.filter { !drained.contains($0) })
            }
            if await drainNewChats(key: key) { retry = true }
        } while outboxDrainAgain && !retry && partition == key && !Task.isCancelled
        guard partition == key, !Task.isCancelled else { return }
        if retry { scheduleOutboxRetry(key: key) } else { outboxAttempts = 0 }
    }

    /// Failures that may pass (offline again, busy, unanswered) retry with
    /// backoff a bounded number of times; reconnecting starts over.
    private func scheduleOutboxRetry(key: String?) {
        outboxAttempts += 1
        guard outboxAttempts <= 8 else { return }
        let delay = Outbox.retryDelay(attempt: outboxAttempts)
        outboxRetry?.cancel()
        outboxRetry = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled, let self, self.partition == key else { return }
            self.outboxRetry = nil
            self.scheduleOutboxDrain()
        }
    }

    private func drainConversation(_ id: String, key: String?) async -> OutboxProgress {
        loadComposer(id)
        guard !intentLoadFailures.contains(id), let device = connection?.credential.deviceId else { return .waiting }
        // A composer send that failed in a way that may pass is this
        // pairing's too: it retries here, whichever chat is on screen.
        func own() -> Bool {
            composers[id]?.outbox?.contains(where: { $0.deviceId == device }) == true || composers[id]?.pending?.fromOutbox == true
                || composers[id]?.pending?.request.deviceId == device
        }
        guard own() else { return (composers[id]?.outbox ?? []).isEmpty ? .empty : .waiting }
        // A copy removed from the draft while offline is forgotten too.
        forgetRemovedUploads(id)
        // A refused message holds its thread until Retry, Edit or Cancel.
        guard composers[id]?.pending != nil || composers[id]?.nextOutboxMessage(device: device) != nil else { return .waiting }
        let chat: ChatSummary
        if let detail = projects.details[id] { chat = projectChat(detail) }
        else {
            do { chat = projectChat(try await projects.loadDetail(id)) }
            catch PairingFailure.response(404) {
                if let head = composers[id]?.nextOutboxMessage(device: device) {
                    markOutboxRefused(id, message: head.id, reason: "This chat is no longer on your computer.")
                }
                return .waiting
            } catch { return .retryLater }
        }
        guard partition == key, !Task.isCancelled else { return .retryLater }
        while true {
            guard partition == key, macConnected == true, !accessEnded, !Task.isCancelled else { return .retryLater }
            // An earlier send of this thread goes first. Check the thread
            // before repeating it: a send the Mac already has is not sent
            // again, and a repeat keeps its client message ID.
            if let pending = composers[id]?.pending {
                if pending.rejected == true { return .waiting }
                await refreshConversation(chat, afterCurrentRead: true)
                guard partition == key, !Task.isCancelled else { return .retryLater }
                guard let still = composers[id]?.pending else { continue }
                if still.receipt != nil || still.request.clientMessageId != pending.request.clientMessageId { return .retryLater }
                if still.fromOutbox == true { outboxInFlight.insert(still.request.clientMessageId) }
                let outcome = await deliver(chat)
                outboxInFlight.remove(still.request.clientMessageId)
                switch outcome {
                case .accepted: continue
                case .refused: return .waiting
                case .retryLater: return .retryLater
                }
            }
            guard let next = composers[id]?.nextOutboxMessage(device: device) else {
                // Nothing of this pairing's waits any more, so no change is owed.
                queueWriteSaved(id)
                return own() ? .waiting : ((composers[id]?.outbox ?? []).isEmpty ? .empty : .waiting)
            }
            if let reason = outboxRefusal(chat, message: next) {
                markOutboxRefused(id, message: next.id, reason: reason)
                return .waiting
            }
            outboxInFlight.insert(next.id)
            defer { outboxInFlight.remove(next.id) }
            // As Send does: a new turn never follows a stale view of the Mac's work.
            let history = await nativeHistoryCurrent(chat)
            if !history.current { await reloadNativeHistory(chat, revision: history.revision) }
            guard partition == key, !Task.isCancelled else { return .retryLater }
            if nativeHistoryFailures.contains(id) { return .retryLater }
            do { try await uploadOutbox(chat, message: next.id) }
            catch {
                guard partition == key, !(error is CancellationError) else { return .retryLater }
                if case .refused(let reason) = Outbox.outcome(of: error) {
                    markOutboxRefused(id, message: next.id, reason: reason)
                    return .waiting
                }
                return .retryLater
            }
            guard partition == key, macConnected == true, !accessEnded, !Task.isCancelled,
                  var intent = composers[id], intent.nextOutboxMessage(device: device)?.id == next.id else { continue }
            do {
                let promoted = try intent.promoteOutbox(device: device)
                if sendWaits(chat, sending: promoted.request) { queuedSends[id, default: []].insert(promoted.request.clientMessageId) }
                try saveComposer(intent, chat: id)
                queueWriteSaved(id)
            } catch {
                queueWriteFailed(id)
                return .waiting
            }
            switch await deliver(chat) {
            case .accepted: continue
            case .refused: return .waiting
            case .retryLater: return .retryLater
            }
        }
    }

    /// New chats sent while the Mac was unreachable, oldest first.
    @Published private(set) var queuedNewChats: [NewChatDraft] = []

    func reloadQueuedNewChats() {
        guard !previewMode, let host = connection?.credential.hostInstallationId else {
            if !queuedNewChats.isEmpty { queuedNewChats = [] }
            return
        }
        let next = NewChatDraftStore.queued(host: host)
        if next != queuedNewChats { queuedNewChats = next }
    }

    func cancelQueuedNewChat(_ item: NewChatDraft) {
        guard !outboxInFlight.contains(item.requestID), let host = connection?.credential.hostInstallationId else { return }
        NewChatDraftStore.removePending(host: host, requestID: item.requestID)
        reloadQueuedNewChats()
    }

    func retryQueuedNewChat(_ item: NewChatDraft) {
        guard !outboxInFlight.contains(item.requestID), item.outboxRetryable == true,
              let host = connection?.credential.hostInstallationId else { return }
        var next = item
        next.outboxRefusal = nil; next.outboxRetryable = nil
        NewChatDraftStore.save(next, host: host, separately: true, makeCurrent: false)
        reloadQueuedNewChats()
        outboxAttempts = 0
        scheduleOutboxDrain()
    }

    /// Creates each waiting New chat's thread through New chat's own path,
    /// then hands its first message to the thread's outbox. True when a
    /// failure that may pass needs another drain.
    private func drainNewChats(key: String?) async -> Bool {
        guard let saved = connection else { return false }
        let host = saved.credential.hostInstallationId, device = saved.credential.deviceId
        reloadQueuedNewChats()
        var retry = false
        // A previous pairing's requests are never sent.
        for item in queuedNewChats where item.submittedDeviceID == device && item.outboxRefusal == nil {
            guard partition == key, macConnected == true, !accessEnded, !Task.isCancelled else { return true }
            // Edit or Cancel may have taken it meanwhile.
            guard let current = NewChatDraftStore.queued(host: host).first(where: { $0.requestID == item.requestID }),
                  current.outboxRefusal == nil else { continue }
            outboxInFlight.insert(current.requestID)
            let progress = await sendQueuedNewChat(current, host: host, device: device, key: key)
            outboxInFlight.remove(current.requestID)
            if progress == .retryLater { retry = true }
            reloadQueuedNewChats()
        }
        return retry
    }

    private func sendQueuedNewChat(_ queued: NewChatDraft, host: String, device: String, key: String?) async -> OutboxProgress {
        var draft = queued
        func refuse(_ reason: String, retryable: Bool) -> OutboxProgress {
            draft.refuseQueued(reason, retryable: retryable)
            NewChatDraftStore.save(draft, host: host, separately: true, makeCurrent: false)
            return .waiting
        }
        guard case .project(let projectID) = draft.destination else {
            return refuse("This message has no project. Edit it to choose one.", retryable: false)
        }
        if draft.outboxAttempted != true {
            draft.outboxAttempted = true
            guard NewChatDraftStore.save(draft, host: host, separately: true, makeCurrent: false) else { return .retryLater }
        }
        let chat: ChatSummary
        do {
            guard let created = try await NewChatSubmission.create(draft, projectID: projectID, body: draft.submittedBody ?? draft.text,
                device: device, model: self, fromOutbox: true,
                stillCurrent: { [weak self] in self?.partition == key && self?.accessEnded == false && !Task.isCancelled },
                prepared: { prepared in
                    draft.preparedConversationID = prepared.preparedConversationID
                    NewChatDraftStore.save(draft, host: host, separately: true, makeCurrent: false)
                }) else { return .retryLater }
            chat = created
        } catch NewChatSubmission.Failure.attachmentUnavailable {
            return refuse("A saved attachment is unavailable. Edit the message to attach the file again.", retryable: false)
        } catch {
            guard partition == key, !(error is CancellationError) else { return .retryLater }
            if let refusal = NewChatDraft.queuedRefusal(error) { return refuse(refusal.reason, retryable: refusal.retryable) }
            return .retryLater
        }
        // The first message is now the new thread's durable pending send; the
        // outbox index keeps it draining after a failure that may pass.
        try? store?.indexOutbox(chat.id)
        NewChatDraftStore.removePending(host: host, requestID: draft.requestID)
        reloadQueuedNewChats()
        switch await deliver(chat) {
        case .accepted: return .empty
        case .refused: return .waiting
        case .retryLater: return .retryLater
        }
    }

    private func markOutboxRefused(_ chat: String, message: String, reason: String) {
        guard var next = composers[chat] else { return }
        next.markOutboxRefused(id: message, reason: reason)
        do { try saveComposer(next, chat: chat); queueWriteSaved(chat) }
        catch { queueWriteFailed(chat) }
    }

    /// What the Mac would refuse, said before sending.
    private func outboxRefusal(_ chat: ChatSummary, message: OutboxMessage) -> String? {
        if chat.isArchived || projects.details[chat.id]?.isArchived == true {
            // Hosts without the archived list still restore from the chat's own screen.
            return projects.supportsArchiveList ? "This chat is archived. Restore it from Archived chats, then try again."
                : "This chat is archived. Restore it, then try again."
        }
        if message.attachmentCount > 0, !attachmentsSupported(chat) {
            return "This chat can’t take attachments. Edit the message to remove them."
        }
        if !(message.codexSkills ?? []).isEmpty, !nextReplyTakesSkills(chat) {
            return "This chat’s model doesn’t take skills. Edit the message to remove them."
        }
        return usageLimitMessage(chat, model: message.projectModel?.model ?? usageModel(chat))
    }

    /// Uploads a waiting message's attachments, recording each one so a
    /// retried drain does not upload it again.
    private func uploadOutbox(_ chat: ChatSummary, message: String) async throws {
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let scope = assignmentScope
        let key = partition
        let files = composers[chat.id]?.outbox?.first(where: { $0.id == message })?.stagedFiles ?? []
        for file in files where file.uploaded == nil {
            let uploaded = try await api.uploadAttachment(file,
                conversationPath: "/api/v1/conversations/\(Self.escape(chat.id))",
                origin: saved.origin, credential: saved.credential)
            guard key == partition, scope == assignmentScope, !accessEnded else { throw CancellationError() }
            guard var next = composers[chat.id] else { throw CancellationError() }
            next.noteOutboxUpload(message: message, file: file.id, uploaded: uploaded)
            try saveComposer(next, chat: chat.id)
        }
    }

    /// Sends a message the Mac did not deliver again, as the same message (no
    /// duplicate). Throws a plain-language reason when the Mac refuses.
    func retryUndelivered(_ chat: ChatSummary, messageID: String) async throws {
        guard !accessEnded, !previewMode, let saved = connection else {
            throw PairingFailure.hostMessage(0, "Cupola on your computer isn’t reachable. Retry when it’s back.")
        }
        let key = partition
        do {
            let _: SendReceipt = try await api.request("/api/v1/messages/\(Self.escape(messageID))/retry",
                origin: saved.origin, body: Data("{}".utf8), credential: saved.credential)
        } catch PairingFailure.response(409) {
            // Already sent again (another device or an earlier tap): show where it is.
        }
        guard partition == key else { return }
        await refreshConversation(chat, afterCurrentRead: true)
    }

    /// Read a conversation again (also before Stop from a Live Activity).
    func refreshConversation(_ chat: ChatSummary, afterCurrentRead: Bool = false) async {
        await restoreSavedHistory(chat.id)
        try? await refreshCoordinator.read(.conversation(chat.id), afterCurrent: afterCurrentRead) { [self] in await performConversationRefresh(chat) }
    }

    private func performConversationRefresh(_ chat: ChatSummary) async {
        #if WONDER_DIAGNOSTICS
        let diagnosticStart = ProcessInfo.processInfo.systemUptime
        defer { DiagnosticJournal.shared.record(DiagnosticEvent(operation: "chat.open", durationMs: (ProcessInfo.processInfo.systemUptime-diagnosticStart)*1000)) }
        #endif
        guard var saved = connection, !accessEnded else { return }
        let run = generation
        let scope = assignmentScope
        let stream = eventStreamLive ? eventStreamBreaks : nil
        let loading = beginLoading(chat.id)
        defer { endLoading(chat.id, loading) }
        #if WONDER_DIAGNOSTICS
        LatencyHops.mark("get.request", chat.id)
        defer { LatencyHops.mark("get.done", chat.id, latencyShape(chat.id)) }
        #endif
        func ownsRead() -> Bool {
            run == generation && scope == assignmentScope && loadingTokens[chat.id] == loading
                && !accessEnded && !Task.isCancelled
        }
        do {
            var page: ConversationSnapshot
            (page, saved) = try await readRenewingSession("/api/v1/conversations/\(Self.escape(chat.id))", saved, run: run)
            guard ownsRead() else { return }
            guard page.conversationId == chat.id else { throw ReadFailure.wrongConversation }
            // Reuse settled history. Refresh any loaded unfinished items too:
            // an older tool can complete while newer items are already streaming.
            if let cached = projection.snapshots[chat.id], cached.hostEpoch == page.hostEpoch,
               (cached.historyRevision ?? 0) == (page.historyRevision ?? 0) {
                guard page.lastSequence >= cached.lastSequence else { return }
                func itemIDs(_ snapshot: ConversationSnapshot) -> Set<String> {
                    Set((snapshot.thread.turns ?? []).flatMap { turn in turn.items.map { turn.id + "/" + $0.id } })
                }
                // A repair continues where the last refresh's walk stopped, in
                // this same history; items a whole walk could not find wait
                // for the history to change.
                var repair = savedSettleRepair(chat.id)?.current(epoch: page.hostEpoch, revision: page.historyRevision ?? 0)
                    ?? SettleRepair(hostEpoch: page.hostEpoch, historyRevision: page.historyRevision ?? 0)
                let unsettled = cached.unsettledItemIDs.subtracting(repair.unreachable)
                var found = itemIDs(page)
                if !unsettled.isSubset(of: found) {
                    // Publish fresh messages before waiting on older tools. Keep
                    // cached rows/reading anchors and the native page's cursor:
                    // a failed older read must not hide the unfilled gap.
                    let partial = try page.mergingOlderKeepingCursor(cached)
                    var next = projection
                    guard isProject(chat), next.hostEpoch.isEmpty || next.hostEpoch == page.hostEpoch else { throw ReadFailure.resync }
                    next.install(partial, catchingUp: liveLog)
                    try commit(next, publishing: .snapshots)
                }
                // Bounded apart from the refresh itself: an item still shown
                // running far back settles over later refreshes, each one
                // continuing the walk (its own cursor, apart from paging)
                // rather than reading from the newest page again.
                let repairStart = ContinuousClock.now
                var repairPages = 0
                let resumes = repair.cursor != nil
                var cursor = repair.cursor ?? page.thread.nextCursor
                var repaired: ConversationSnapshot?
                var placeRefused = false
                while !unsettled.isSubset(of: found), let before = cursor, repairPages < Self.settleRepairPages,
                      ContinuousClock.now - repairStart < Self.settleRepairTime {
                    repairPages += 1
                    let older: ConversationSnapshot
                    do {
                        older = try await api.request("/api/v1/conversations/\(Self.escape(chat.id))/history?before=\(Self.escape(before))", origin: saved.origin, credential: saved.credential)
                    } catch let error where resumes && !(error is CancellationError) {
                        // A saved place the Mac no longer takes: start over next time.
                        // A failed read proves nothing about the items it looked for.
                        guard ownsRead() else { return }
                        cursor = nil; placeRefused = true
                        break
                    }
                    guard ownsRead() else { return }
                    guard older.thread.nextCursor != before else { throw ReadFailure.resync }
                    repaired = try repaired.map { try $0.mergingOlder(older) } ?? older
                    found.formUnion(itemIDs(older))
                    cursor = older.thread.nextCursor
                }
                repair = repair.afterWalk(missing: unsettled.subtracting(found), next: cursor, pagesRead: repairPages, placeRefused: placeRefused)
                saveSettleRepair(repair.bounded(to: cached.unsettledItemIDs), chat: chat.id)
                guard page.lastSequence >= cached.lastSequence else { return }
                // Pages read on from the newest one extend it; pages read on
                // from an earlier walk only settle the saved turns they hold.
                if let repaired, !resumes { page = try page.mergingOlder(repaired) }
                page = try page.mergingSaved(cached)
                if let repaired, resumes { page = page.updatingKnownTurns(from: repaired) }
            }
            guard ownsRead() else { return }
            guard page.conversationId == chat.id else { throw ReadFailure.wrongConversation }
            guard isProject(chat) else {
                if projection.snapshots[chat.id] == nil { conversationLoadFailures[chat.id] = ChatOpenFailure(PairingFailure.response(404), computer: macName) }
                return
            }
            var next = projection
            // Snapshot cursor only establishes an epoch at bootstrap. It never
            // skips events in this epoch, including events for other chats.
            guard next.hostEpoch.isEmpty || next.hostEpoch == page.hostEpoch else { throw ReadFailure.resync }
            next.install(page, catchingUp: liveLog)
            try commit(next, publishing: .snapshots)
            conversationCompletedReads[chat.id] = loading
            // The stream that was open for the whole read keeps this current.
            historyReadOnStream[chat.id] = eventStreamLive && stream == eventStreamBreaks ? stream : nil
            // A current chat no longer needs the events this read covers.
            if !next.dirty.contains(chat.id) { liveLog.release(through: page.lastSequence) }
            if isDemo { startDemoReply(chat) }
            #if WONDER_DIAGNOSTICS
            startDiagnosticStream(chat)
            startDiagnosticMacWake(chat)
            startDiagnosticIdleTaskCompletion(chat)
            startDiagnosticBanner(chat)
            #endif
            if var waiting = queuedSends[chat.id] {
                waiting.subtract(page.messages.filter { $0.state != "accepted_by_wonder" }.compactMap(\.clientMessageId))
                queuedSends[chat.id] = waiting.isEmpty ? nil : waiting
            }
            projects.historyLoaded(chat.id)
            conversationLoadFailures[chat.id] = nil
            conversationLoadRetries[chat.id] = nil
            if var intent = composers[chat.id], intent.pending != nil || !(intent.recoveredPending ?? []).isEmpty {
                intent.reconcile(page)
                try saveComposer(intent, chat: chat.id)
                if intent.pending == nil { clearComposerNotice(chat.id) }
            }
            reconcileRewind(chat, page: page)
        } catch {
            guard ownsRead() else { return }
            let issue = ChatOpenFailure(error, computer: macName)
            let missing: Bool = switch error {
            case PairingFailure.response(404), PairingFailure.hostMessage(404, _): true
            default: false
            }
            if missing {
                if case PairingFailure.hostMessage = error { conversationLoadFailures[chat.id] = issue }
                do { try removeDeletedConversation(chat.id) } catch { readFailed(error, run: run, origin: saved.origin, token: saved.credential.sessionToken) }
                projects.historyNotFound(chat.id)
            } else {
                // Saved content stays on screen; only an empty chat shows the failure,
                // and only after quiet retries: a long chat's first page can time out
                // while the Mac is still reading its history.
                if run == generation, !(error is CancellationError),
                   projection.snapshots[chat.id] == nil {
                    let attempt = conversationLoadRetries[chat.id, default: 0]
                    if issue.canRetry, attempt < 2 {
                        conversationLoadRetries[chat.id] = attempt + 1
                        Task { [weak self] in
                            try? await Task.sleep(for: .seconds(attempt == 0 ? 2 : 5))
                            guard let self, !Task.isCancelled, run == self.generation,
                                  self.projection.snapshots[chat.id] == nil else { return }
                            await self.refreshConversation(chat)
                        }
                    } else {
                        conversationLoadFailures[chat.id] = issue
                    }
                }
                readFailed(error, run: run, origin: saved.origin, token: saved.credential.sessionToken)
            }
        }
    }

    /// Older pages (and time) one refresh may read to settle running items.
    static let settleRepairPages = 4
    static let settleRepairTime: Duration = .seconds(4)
    private var settleRepairs: [String: SettleRepair] = [:]
    private var settleRepairsLoaded: Set<String> = []
    private func savedSettleRepair(_ chat: String) -> SettleRepair? {
        if settleRepairsLoaded.insert(chat).inserted, settleRepairs[chat] == nil {
            settleRepairs[chat] = store?.loadSettleRepair(conversation: chat)
        }
        return settleRepairs[chat]
    }
    private func saveSettleRepair(_ repair: SettleRepair?, chat: String) {
        settleRepairsLoaded.insert(chat)
        guard settleRepairs[chat] != repair else { return }
        settleRepairs[chat] = repair
        // Best effort: losing the record only repeats a bounded walk.
        try? store?.saveSettleRepair(repair, conversation: chat)
    }

    func removeDeletedConversation(_ id: String) throws {
        // Unsent work outlives its chat. Keep it before anything is removed;
        // if it cannot be kept, nothing is removed.
        loadComposer(id)
        let unreadable = intentLoadFailures.contains(id)
        if !unreadable { try keepOrphanedDraft(id) }
        typedDraftSaves.removeValue(forKey: id)?.cancel(); unsavedDrafts.remove(id)
        var next = projection
        next.forgetConversation(id)
        savedPositions[id] = nil; unsavedPositions[id] = nil
        try commit(next)
        // An unreadable saved draft is never deleted: its bytes stay where a
        // returning chat (or a later app version) can still read them.
        if !unreadable { try store?.removeComposer(conversation: id) }
        try store?.removeIntent(conversation: "async-list-" + id)
        settleRepairs[id] = nil; try? store?.saveSettleRepair(nil, conversation: id)
        composers.removeValue(forKey: id); composerErrors.removeValue(forKey: id); owedComposerWrites.removeValue(forKey: id); owedQueueWrites.removeValue(forKey: id)
        files.removeValue(forKey: id); queues.removeValue(forKey: id); queuedSends.removeValue(forKey: id); asyncQuestions.removeValue(forKey: id)
        conversationCompletedReads.removeValue(forKey: id); nativeHistoryCompletedReads.removeValue(forKey: id); nativeHistoryReadRevisions.removeValue(forKey: id)
        controlErrors.removeValue(forKey: id); sending.remove(id); uploading.remove(id)
        leaveDeletedConversation(id)
    }

    /// The computer already deleted the chat, so it can't stay open even when
    /// local cleanup stopped early; the next sync clears what that left behind.
    func leaveDeletedConversation(_ id: String) {
        if selectedChat?.id == id { selectedChat = nil }
        if visibleChat?.id == id { visibleChat = nil }
    }

    static let removedChatRefusal = "This chat is no longer on your computer."

    /// A non-empty composer of a chat the computer no longer has becomes an
    /// orphaned draft, fenced from automatic sending. Empty ones just go.
    private func keepOrphanedDraft(_ id: String) throws {
        guard !intentLoadFailures.contains(id), let intent = composers[id], intent.holdsUnsentWork else { return }
        #if WONDER_DIAGNOSTICS
        let listed = chats.first(where: { $0.id == id })?.title
        #else
        let listed: String? = nil
        #endif
        let title = listed ?? projects.details[id]?.title ?? "A removed chat"
        // Fence the chat's own composer first: if the app stops before the
        // composer is removed, what remains can no longer send on its own,
        // and the next "not found" keeps it again.
        var fenced = intent
        fenced.fenceAfterRemoval(reason: Self.removedChatRefusal)
        try saveComposer(fenced, chat: id)
        let kept = orphanedDrafts.filter { $0.conversationId != id }
            + [OrphanedDraft(conversationId: id, title: title, intent: fenced, reason: Self.removedChatRefusal)]
        try saveOrphanedDrafts(kept)
    }

    private func saveOrphanedDrafts(_ drafts: [OrphanedDraft]) throws {
        guard !orphanedDraftsUnreadable else { throw ReadFailure.resync }
        if !previewMode {
            guard let store else { throw ReadFailure.resync }
            try store.saveOrphanedDrafts(drafts)
        }
        orphanedDrafts = drafts
    }

    func reloadOrphanedDrafts() {
        orphanedDraftsUnreadable = false
        guard let store else { orphanedDrafts = []; return }
        do { orphanedDrafts = try store.loadOrphanedDrafts(); orphanedDraftError = nil }
        catch {
            // The file stays as it is; nothing overwrites it while unreadable.
            orphanedDrafts = []; orphanedDraftsUnreadable = true
            orphanedDraftError = "A message kept from a removed chat could not be read. Free some storage and reopen Cupola."
        }
    }

    /// Moves a kept message into this chat's empty draft: its text, and the
    /// files whose bytes are still on this device. Nothing is sent.
    @discardableResult
    func moveOrphanedDraft(_ id: String, to chat: ChatSummary) -> Bool {
        loadComposer(chat.id)
        guard let orphan = orphanedDrafts.first(where: { $0.id == id }) else { return false }
        guard !intentLoadFailures.contains(chat.id) else {
            orphanedDraftError = "Reopen this chat to load its saved draft, then move the message here."
            return false
        }
        var next = composers[chat.id] ?? ComposerIntent()
        guard next.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, next.attachmentCount == 0,
              next.pending == nil, !uploading.contains(chat.id), !preparingSends.contains(chat.id),
              !loadingPhotos.contains(chat.id) else {
            orphanedDraftError = "Send or clear the message you’re writing first, then move the kept message here."
            return false
        }
        let files = orphan.localFiles
        guard files.isEmpty || (canAttach(chat) && files.count <= 4) else {
            orphanedDraftError = "This chat can’t take the kept message’s attachments. Copy its text instead."
            return false
        }
        typedDraftSaves.removeValue(forKey: chat.id)?.cancel()
        next.draft = orphan.text
        next.stagedFiles = files.isEmpty ? nil : files
        do { try saveComposer(next, chat: chat.id) }
        catch {
            orphanedDraftError = "The kept message could not be moved. Free some storage and try again."
            return false
        }
        clearComposerNotice(chat.id)
        // The moved copy is durable; a failed removal only leaves a duplicate.
        do { try saveOrphanedDrafts(orphanedDrafts.filter { $0.id != id }); orphanedDraftError = nil }
        catch { orphanedDraftError = "The message was moved, but its kept copy could not be removed. Discard it." }
        return true
    }

    func discardOrphanedDraft(_ id: String) {
        do { try saveOrphanedDrafts(orphanedDrafts.filter { $0.id != id }); orphanedDraftError = nil }
        catch { orphanedDraftError = "The kept message could not be discarded. Free some storage and try again." }
    }

    func loadOlder(_ chat: ChatSummary) async -> String? {
        #if WONDER_DIAGNOSTICS
        let diagnosticStart = ProcessInfo.processInfo.systemUptime
        defer { DiagnosticJournal.shared.record(DiagnosticEvent(operation: "history.load", durationMs: (ProcessInfo.processInfo.systemUptime-diagnosticStart)*1000)) }
        #endif
        guard !loadingConversationIDs.contains(chat.id), let saved = connection,
              let current = projection.snapshots[chat.id],
              let cursor = current.thread.nextCursor else { return nil }
        let run = generation
        let loading = beginLoading(chat.id)
        defer { endLoading(chat.id, loading) }
        do {
            let (older, _): (ConversationSnapshot, SavedConnection) = try await readRenewingSession(
                "/api/v1/conversations/\(Self.escape(chat.id))/history?before=\(Self.escape(cursor))", saved, run: run)
            guard run == generation else { return nil }
            // History hydration may advance the replay sequence itself. Merge
            // into the latest page so a concurrent refresh wins for shared IDs.
            guard let latest = projection.snapshots[chat.id],
                  latest.hostEpoch == current.hostEpoch, older.hostEpoch == latest.hostEpoch else { return nil }
            guard latest.thread.nextCursor == cursor else { return nil }
            var next = projection
            next.snapshots[chat.id] = try latest.mergingOlder(older)
            guard next.snapshots[chat.id]?.thread.nextCursor != cursor else { return "Couldn’t load earlier messages" }
            try commit(next, publishing: .snapshots)
            return nil
        } catch {
            if Task.isCancelled || run != generation { return nil }
            readFailed(error, run: run, origin: saved.origin, token: saved.credential.sessionToken)
            return "Couldn’t load earlier messages"
        }
    }

    // MARK: Message search

    /// One page of message search on this Mac: Wonder's messages and the
    /// Mac's own Codex threads and Claude Code sessions in included Projects.
    func searchMessages(_ query: String, cursor: String?) async throws -> MessageSearchPage {
        var components = URLComponents()
        components.path = "/api/v1/search"
        components.queryItems = [URLQueryItem(name: "q", value: query), URLQueryItem(name: "limit", value: "20")]
        if let cursor { components.queryItems?.append(URLQueryItem(name: "cursor", value: cursor)) }
        return try await manage(components.percentEncodedQuery.map { "/api/v1/search?" + $0 } ?? "/api/v1/search")
    }

    /// Scrolls an opened conversation to a search result once its row is
    /// loaded, reading earlier history as needed. A whole-thread match asks
    /// the Mac where in the thread the words are. Ends quietly (the chat
    /// stays open at its usual place) when the row cannot be found.
    func focusSearchResult(_ result: MessageSearchResult, conversation: String, query: String) {
        searchFocusTask?.cancel()
        let scope = assignmentScope
        searchFocusTask = Task { [weak self] in
            guard let self else { return }
            let deadline = Date().addingTimeInterval(12)
            var target = result.focus
            var olderPages = 0
            while Date() < deadline, !Task.isCancelled, scope == self.assignmentScope, !self.accessEnded {
                if target == nil, self.snapshots[conversation] != nil {
                    let path = "/api/v1/conversations/\(Self.escape(conversation))/search/occurrences?q="
                        + (query.addingPercentEncoding(withAllowedCharacters: .urlQueryAllowed.subtracting(CharacterSet(charactersIn: "&+=?#"))) ?? "")
                    guard let found: SearchOccurrences = try? await self.manage(path), scope == self.assignmentScope,
                          let item = found.occurrences.last?.itemId else { return }
                    target = .item(item)
                }
                if let target, let detail = self.projects.details[conversation], let snapshot = self.snapshots[conversation] {
                    let chat = self.projectChat(detail)
                    let rows = self.feedRows(for: chat)
                    if let row = target.rowID(snapshot: snapshot, rows: rows) {
                        guard let entry = ChatFeedEntry.grouping(rows, focusedRowID: row).first(where: { $0.rows.contains { $0.id == row } })
                        else { return }
                        self.savePosition(entry.id, chat: conversation)
                        self.searchFocus = SearchMessageFocus(conversationID: conversation, rowID: row, scrollID: entry.id)
                        return
                    }
                    if snapshot.thread.nextCursor != nil, olderPages < 6 {
                        olderPages += 1
                        if await self.loadOlder(chat) != nil { return }
                        continue
                    }
                }
                // The thread was just attached; its history is still arriving.
                try? await Task.sleep(for: .milliseconds(400))
            }
        }
    }

    func loadQueue(_ chat: ChatSummary, afterCurrentRead: Bool = false) async throws {
        try await refreshCoordinator.read(.queue(chat.id), afterCurrent: afterCurrentRead) { [self] in try await performQueueRead(chat) }
    }

    private func performQueueRead(_ chat: ChatSummary) async throws {
        if previewMode { return }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let key = partition, scope = assignmentScope
        let items: [QueuedMessage] = try await api.request("/api/v1/conversations/\(Self.escape(chat.id))/queue", origin: saved.origin, credential: saved.credential)
        guard key == partition, scope == assignmentScope, connection?.origin == saved.origin,
              !accessEnded, !Task.isCancelled else { throw CancellationError() }
        if key == partition {
            // An older GET may finish after a confirmed settings mutation.
            let current = Dictionary((queues[chat.id] ?? []).map { ($0.id, $0) }, uniquingKeysWith: { _, last in last })
            // Polled with every refresh: an unchanged queue must not republish.
            setIfChanged(\.queues[chat.id], items.map { item in
                if let newer = current[item.id], newer.revision > item.revision { return newer }
                return item
            })
        }
    }
    func changeQueue(_ chat: ChatSummary, item: QueuedMessage, body: String? = nil, cancel: Bool = false, turn: String? = nil) async throws {
        guard isProject(chat) else { throw PairingFailure.response(403) }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        var value: [String: Any] = ["expectedRevision": item.revision]
        if let body { value["body"] = body }
        if cancel { value["cancel"] = true }
        if let turn { value["expectedTurnId"] = turn; awaitAcceptance(item.clientMessageId, chat: chat.id, as: .steering) }
        struct Empty: Decodable, Sendable {}
        do {
            let _: Empty = try await api.request("/api/v1/conversations/\(Self.escape(chat.id))/queue/\(Self.escape(item.id))", origin: saved.origin, body: JSONSerialization.data(withJSONObject: value), credential: saved.credential, method: "POST")
        } catch {
            if turn != nil { stopAwaitingAcceptance(item.clientMessageId, chat: chat.id) }
            throw error
        }
        // The Mac applied the change; a failed refresh must not read as refused
        // (a cancelled message would then not be offered back to the draft).
        try? await loadQueue(chat, afterCurrentRead: true)
        await refreshConversation(chat, afterCurrentRead: true)
    }
    /// Whether Move earlier and Move later are offered: Cupola holds two or
    /// more of the chat's waiting messages and none waits in T3 Code's own
    /// queue, whose order Cupola cannot change.
    func canReorderQueue(_ chat: ChatSummary) -> Bool {
        let items = queues[chat.id] ?? []
        return isProject(chat) && items.count > 1 && !items.contains { $0.waitsInT3Code == true }
            && !previewMode && !accessEnded && connection != nil
    }
    /// Moves one waiting message earlier (`offset` -1) or later (+1). The Mac
    /// applies the whole order only if no message changed or started since
    /// the list was read; otherwise it answers 409 and nothing moves.
    func moveQueued(_ chat: ChatSummary, item: QueuedMessage, by offset: Int) async throws {
        guard canReorderQueue(chat) else { throw PairingFailure.response(409) }
        guard let saved = connection else { throw PairingFailure.missingIdentity }
        var items = queues[chat.id] ?? []
        guard let from = items.firstIndex(where: { $0.id == item.id }), items.indices.contains(from + offset) else { return }
        items.swapAt(from, from + offset)
        let order: [[Any]] = items.map { [$0.id, $0.revision] }
        struct Empty: Decodable, Sendable {}
        do {
            let _: Empty = try await api.request("/api/v1/conversations/\(Self.escape(chat.id))/queue", origin: saved.origin, body: JSONSerialization.data(withJSONObject: ["items": order]), credential: saved.credential, method: "POST")
        } catch {
            try? await loadQueue(chat, afterCurrentRead: true)
            throw error
        }
        try await loadQueue(chat, afterCurrentRead: true)
    }
    /// Whether a queued message can go back to the composer without replacing
    /// anything there: its text and files join what is already written.
    func canMoveQueuedToDraft(_ chat: ChatSummary, item: QueuedMessage) -> Bool {
        let composer = composers[chat.id]
        return isProject(chat) && !intentLoadFailures.contains(chat.id) && composer?.pending == nil
            && !preparingSends.contains(chat.id) && !uploading.contains(chat.id) && !loadingPhotos.contains(chat.id)
            && (composer?.attachmentCount ?? 0) + item.attachmentIds.count <= 4
            && !previewMode && !accessEnded
    }
    /// Puts a cancelled queued message's text, files and skills in the
    /// composer after anything already written there, never replacing it
    /// (Move to draft, and Put back in draft after Cancel).
    func restoreQueuedToDraft(_ chat: ChatSummary, item: QueuedMessage) throws {
        loadComposer(chat.id)
        guard canMoveQueuedToDraft(chat, item: item) else { throw PairingFailure.response(409) }
        var next = composers[chat.id] ?? ComposerIntent()
        let existing = next.draft.trimmingCharacters(in: .whitespacesAndNewlines)
        let skills = next.codexSkills ?? []
        next.draft = existing.isEmpty ? item.body : next.draft + "\n\n" + item.body
        let restored = (item.codexSkills ?? []).filter { !skills.contains($0) }
        if !skills.isEmpty || !restored.isEmpty { next.codexSkills = skills + restored }
        let files = item.attachmentIds.filter { !next.attachmentIDs.contains($0) }
        if !files.isEmpty { next.draftAttachmentIds = (next.draftAttachmentIds ?? []) + files }
        try saveComposer(next, chat: chat.id)
        clearComposerNotice(chat.id)
    }

    func attachmentsSupported(_ chat: ChatSummary) -> Bool {
        capabilities(chat)?.attachments == true
    }
    private func attachmentCount(_ chat: String) -> Int {
        composers[chat]?.attachmentCount ?? 0
    }
    func canAttach(_ chat: ChatSummary) -> Bool {
        attachmentsSupported(chat) && !chat.isArchived && !accessEnded && !previewMode
            && connection != nil && isProject(chat)

            && !preparingSends.contains(chat.id) && !uploading.contains(chat.id) && composers[chat.id]?.pending == nil
            && !intentLoadFailures.contains(chat.id) && attachmentCount(chat.id) < 4
    }
    func stagePhoto(_ photo: PhotosPickerItem, chat: ChatSummary) async {
        await stagePhotos([photo], chat: chat)
    }

    /// Photos picked together, in the order picked.
    func stagePhotos(_ photos: [PhotosPickerItem], chat: ChatSummary) async {
        await stageChosenMedia(photos.map { photo in { try await MediaPreparation.chosen(photo) } }, chat: chat)
    }

    /// Loads chosen photos one at a time and saves those that could be read
    /// together; any that could not are counted in one message, so a single
    /// unreadable photo does not cost the others.
    func stageChosenImages(_ loads: [() async throws -> Data], chat: ChatSummary) async {
        await stageChosenMedia(loads.map { load in { try await Self.prepareImageAttachment(load()) } }, chat: chat)
    }

    func stageChosenMedia(_ loads: [() async throws -> StagedFile], chat: ChatSummary) async {
        guard !loads.isEmpty, canAttach(chat), !loadingPhotos.contains(chat.id) else { return }
        let key = partition
        let scope = assignmentScope
        let contextID = cameraContextID
        let request = UUID()
        photoImportIDs[chat.id] = request
        loadingPhotos.insert(chat.id)
        controlErrors.removeValue(forKey: chat.id)
        defer { finishPhotoImport(request, chat: chat.id) }
        func owns() -> Bool {
            contextID == cameraContextID && key == partition && scope == assignmentScope
                && photoImportIDs[chat.id] == request
        }
        var files: [StagedFile] = []
        var lastFailure: Error?
        do {
            guard attachmentCount(chat.id) + loads.count <= 4 else { throw FileFailure.tooLarge }
            for load in loads {
                do {
                    let file = try await load()
                    try Task.checkCancellation()
                    guard owns(), canAttach(chat) else { return }
                    files.append(file)
                } catch is CancellationError { throw CancellationError() }
                catch { lastFailure = error }
                try Task.checkCancellation()
                guard owns(), canAttach(chat) else { return }
            }
            if !files.isEmpty {
                _ = try await persistAttachments(files, chat: chat, key: key, scope: scope, contextID: contextID, request: request)
            }
        } catch is CancellationError {
            // Leaving the conversation cancels the import without changing the draft.
            return
        } catch {
            files = []
            lastFailure = error
        }
        guard let lastFailure, owns(), !Task.isCancelled else { return }
        let failed = loads.count - files.count
        let limits = "Use up to four attachments. Videos must be 1 GB (1,024 MB) or smaller; PNG, transparent and animated images must be 8 MB or smaller."
        controlErrors[chat.id] = attachmentFailure(lastFailure, fallback: loads.count == 1
            ? "Could not attach this item. Select it again and check your connection if it is stored in iCloud. " + limits
            : (files.isEmpty ? "Could not attach these items. " : "\(failed) of \(loads.count) items could not be attached. ")
                + "Select them again and check your connection if they are stored in iCloud. " + limits)
    }

    /// Load a paste sequentially and save it atomically, preserving the draft if
    /// any image is unreadable or the whole paste exceeds the attachment limit.
    func stagePastedImages(_ providers: [NSItemProvider], chat: ChatSummary, scope: String) async {
        guard scope == assignmentScope, canAttach(chat), !loadingPhotos.contains(chat.id), !Task.isCancelled else { return }
        let key = partition
        let contextID = cameraContextID
        let request = UUID()
        photoImportIDs[chat.id] = request
        loadingPhotos.insert(chat.id)
        controlErrors.removeValue(forKey: chat.id)
        defer { finishPhotoImport(request, chat: chat.id) }
        do {
            guard !providers.isEmpty else { throw FileFailure.integrity }
            guard providers.count + attachmentCount(chat.id) <= 4 else { throw FileFailure.tooLarge }
            var files: [StagedFile] = []
            for provider in providers {
                guard let type = provider.registeredTypeIdentifiers.first(where: {
                    UTType($0)?.conforms(to: .image) == true
                }) else { throw FileFailure.integrity }
                let data: Data = try await withCheckedThrowingContinuation { continuation in
                    provider.loadDataRepresentation(forTypeIdentifier: type) { data, error in
                        if let error { continuation.resume(throwing: error) }
                        else if let data { continuation.resume(returning: data) }
                        else { continuation.resume(throwing: FileFailure.integrity) }
                    }
                }
                try Task.checkCancellation()
                guard contextID == cameraContextID, key == partition, scope == assignmentScope,
                  photoImportIDs[chat.id] == request, canAttach(chat) else { return }
                let file = try await Self.prepareImageAttachment(data)
                try Task.checkCancellation()
                guard contextID == cameraContextID, key == partition, scope == assignmentScope,
                      photoImportIDs[chat.id] == request, canAttach(chat) else { return }
                files.append(file)
            }
            try Task.checkCancellation()
            guard contextID == cameraContextID, key == partition, scope == assignmentScope,
                  photoImportIDs[chat.id] == request, canAttach(chat) else { return }
            _ = try await persistAttachments(files, chat: chat, key: key, scope: scope, contextID: contextID, request: request)
        } catch is CancellationError {
            // A paste belongs to the conversation and pairing that started it.
        } catch {
            guard contextID == cameraContextID, key == partition, scope == assignmentScope,
                  photoImportIDs[chat.id] == request, !Task.isCancelled else { return }
            controlErrors[chat.id] = attachmentFailure(error, fallback: "Could not paste these images. Use up to four attachments. Videos must be 1 GB (1,024 MB) or smaller; PNG, transparent and animated images must be 8 MB or smaller.")
        }
    }

    /// Photos and paste follow `CameraPhotoPreparation`'s policy: the original
    /// when it fits, otherwise a smaller JPEG for a still photo.
    static func prepareImageAttachment(_ data: Data) async throws -> StagedFile {
        let prepared: PreparedCameraPhoto
        do { prepared = try await CameraPhotoPreparation.prepareChosen(data) }
        catch CameraPhotoPreparationFailure.tooLarge { throw FileFailure.tooLarge }
        catch CameraPhotoPreparationFailure.invalidImage { throw FileFailure.integrity }
        return try StagedFile(name: "Photo-\(UUID().uuidString.prefix(8)).\(prepared.fileExtension)",
                              mimeType: prepared.mimeType, data: prepared.data)
    }

    /// Camera photos in a chat and in New chat are re-encoded the same way.
    static func prepareCameraAttachment(_ data: Data, namePrefix: String = "Photo") async throws -> StagedFile {
        let prepared = try await CameraPhotoPreparation.prepare(data)
        return try StagedFile(name: "\(namePrefix)-\(UUID().uuidString.prefix(8)).\(prepared.fileExtension)",
                              mimeType: prepared.mimeType, data: prepared.data)
    }

    func stageCameraPhoto(_ data: Data, chat: ChatSummary, scope: String, namePrefix: String = "Photo") async -> CameraAttachmentResult {
        await stageCapturedImage(data, chat: chat, scope: scope, namePrefix: namePrefix, sketch: false)
    }

    func sketchUnavailableReason(_ chat: ChatSummary) -> String? {
        if chat.isArchived || projects.details[chat.id]?.isArchived == true {
            return "Restore this chat to attach a sketch."
        }
        if accessEnded || connection == nil { return "Reconnect to your computer to attach a sketch." }
        if !attachmentsSupported(chat) { return "This agent can’t receive attachments." }
        if previewMode || !isProject(chat) { return "Open a Project chat to attach a sketch." }
        if intentLoadFailures.contains(chat.id) { return "Reopen this chat to load its saved draft before attaching a sketch." }
        if preparingSends.contains(chat.id) || uploading.contains(chat.id) || composers[chat.id]?.pending != nil {
            return "Wait for the current message to finish before attaching a sketch."
        }
        if attachmentCount(chat.id) >= 4 { return "Remove an attachment to add a sketch. You can attach up to four files." }
        if loadingPhotos.contains(chat.id) { return "Wait for the current image to finish attaching." }
        return canAttach(chat) ? nil : "Sketch attachments are unavailable right now. Try again."
    }

    func stageSketch(_ data: Data, chat: ChatSummary, scope: String) async -> SketchAttachmentResult {
        guard !Task.isCancelled, scope == assignmentScope, visibleChat?.id == chat.id else { return .cancelled }
        loadComposer(chat.id)
        if let reason = sketchUnavailableReason(chat) { return .failed(reason) }
        let contextID = cameraContextID
        switch await stageCapturedImage(data, chat: chat, scope: scope, namePrefix: "Sketch", sketch: true) {
        case .attached: return .attached
        case .failed: return .failed(controlErrors[chat.id] ?? "The sketch could not be attached. Try again.")
        case .cancelled:
            guard !Task.isCancelled, contextID == cameraContextID, scope == assignmentScope,
                  visibleChat?.id == chat.id else { return .cancelled }
            return .failed(sketchUnavailableReason(chat) ?? "The sketch could not be attached. Try again.")
        }
    }

    private func stageCapturedImage(_ data: Data, chat: ChatSummary, scope: String,
                                    namePrefix: String, sketch: Bool) async -> CameraAttachmentResult {
        guard !Task.isCancelled, scope == assignmentScope, visibleChat?.id == chat.id else { return .cancelled }
        loadComposer(chat.id)
        guard !intentLoadFailures.contains(chat.id), canAttach(chat), !loadingPhotos.contains(chat.id) else { return .cancelled }
        let key = partition
        let contextID = cameraContextID
        let request = UUID()
        photoImportIDs[chat.id] = request
        loadingPhotos.insert(chat.id)
        controlErrors.removeValue(forKey: chat.id)
        defer { finishPhotoImport(request, chat: chat.id) }
        do {
            let file: StagedFile
            if sketch {
                file = try await SketchExport.attachment(data)
            } else {
                file = try await Self.prepareCameraAttachment(data, namePrefix: namePrefix)
            }
            try Task.checkCancellation()
            guard contextID == cameraContextID, key == partition, scope == assignmentScope, visibleChat?.id == chat.id,
                  photoImportIDs[chat.id] == request, !accessEnded, !previewMode, connection != nil,
                  isProject(chat),
                  !chat.isArchived, (!sketch || projects.details[chat.id]?.isArchived != true),
                  composers[chat.id]?.pending == nil, attachmentCount(chat.id) < 4 else { return .cancelled }
            return try await persistAttachments([file], chat: chat, key: key, scope: scope, contextID: contextID, request: request)
                ? .attached : .cancelled
        } catch is CancellationError {
            return .cancelled
        } catch {
            guard contextID == cameraContextID, key == partition, scope == assignmentScope, visibleChat?.id == chat.id,
                  photoImportIDs[chat.id] == request, !Task.isCancelled else { return .cancelled }
            controlErrors[chat.id] = attachmentFailure(error, fallback: sketch
                ? "Could not attach this sketch. Use up to four attachments, each no larger than 8 MB."
                : "Could not attach this photo. Use up to four attachments, each no larger than 8 MB.")
            return .failed
        }
    }

    /// A picked file is read off the main actor and saved through the same
    /// owner as photos. A chat whose saved draft could not be read refuses it:
    /// a new draft must never replace the only copy of the old one.
    func stage(_ url: URL, chat: ChatSummary, mime: String) async {
        await stageFiles([(url: url, mime: mime)], chat: chat)
    }
    /// Every file picked at once, in the order picked.
    func stage(_ urls: [URL], chat: ChatSummary) async {
        await stageFiles(urls.map { (url: $0, mime: UTType(filenameExtension: $0.pathExtension)?.preferredMIMEType ?? "application/octet-stream") }, chat: chat)
    }
    /// Files are read one at a time and saved together, so a selection that
    /// is over the limit, or has an unreadable file, leaves the draft unchanged.
    private func stageFiles(_ picked: [(url: URL, mime: String)], chat: ChatSummary) async {
        guard !picked.isEmpty else { return }
        loadComposer(chat.id)
        guard !intentLoadFailures.contains(chat.id) else {
            controlErrors[chat.id] = "Reopen this chat to load its saved draft before attaching a file."
            return
        }
        guard canAttach(chat), !loadingPhotos.contains(chat.id) else { return }
        let key = partition, scope = assignmentScope
        let contextID = cameraContextID
        let request = UUID()
        photoImportIDs[chat.id] = request
        loadingPhotos.insert(chat.id)
        controlErrors.removeValue(forKey: chat.id)
        defer { finishPhotoImport(request, chat: chat.id) }
        func owns() -> Bool {
            !Task.isCancelled && contextID == cameraContextID && key == partition && scope == assignmentScope
                && photoImportIDs[chat.id] == request
        }
        let several = picked.count > 1
        do {
            guard attachmentCount(chat.id) + picked.count <= 4 else { throw FileFailure.tooLarge }
            var files: [StagedFile] = []
            for (url, mime) in picked {
                files.append(try await Self.readPickedFile(url, mime: mime))
                try Task.checkCancellation()
                guard owns(), canAttach(chat) else { return }
            }
            _ = try await persistAttachments(files, chat: chat, key: key, scope: scope, contextID: contextID, request: request)
        } catch is CancellationError {
            // Leaving the conversation cancels the import without changing the draft.
        } catch {
            guard owns() else { return }
            controlErrors[chat.id] = switch error {
            case AttachmentPersistenceFailure.storage: several
                ? "Could not save these files. Free some storage and try attaching them again."
                : "Could not save this file. Free some storage and try attaching it again."
            case AttachmentPersistenceFailure.changed: several
                ? "Your draft changed while saving these files. Try attaching them again."
                : "Your draft changed while saving this file. Try attaching it again."
            default: several
                ? "Could not attach these files. Use up to four files, each no larger than 8 MB, and try again."
                : "Could not attach this file. Use up to four files, each no larger than 8 MB, and try again."
            }
        }
    }
    private static func readPickedFile(_ url: URL, mime: String) async throws -> StagedFile {
        let worker = Task.detached(priority: .userInitiated) {
            try Task.checkCancellation()
            let access = url.startAccessingSecurityScopedResource()
            defer { if access { url.stopAccessingSecurityScopedResource() } }
            let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? Int.max
            try AttachmentLimits.validate(byteSize: size, mimeType: mime)
            if mime.hasPrefix("video/") { return try StagedFile.importingVideo(url, name: url.lastPathComponent, mimeType: mime) }
            return try StagedFile(name: url.lastPathComponent, mimeType: mime, data: Data(contentsOf: url, options: .alwaysMapped))
        }
        return try await withTaskCancellationHandler(operation: { try await worker.value }, onCancel: { worker.cancel() })
    }
    func removeStaged(_ id: String, chat: String) {
        guard !uploading.contains(chat), !preparingSends.contains(chat) else { return }
        guard composers[chat]?.pending == nil else { return }
        do {
            var intent = composers[chat] ?? ComposerIntent()
            intent.removeAttachment(id: id)
            try saveComposer(intent, chat: chat)
            staleAnnotationIDs[chat]?.remove(id)
            forgetRemovedUploads(chat)
        }
        catch { controlErrors[chat] = "Could not save the attachment change." }
    }
    /// Removing a file from a draft also removes the copy an earlier, unsent
    /// attempt uploaded to the Mac. The draft shows the removal at once; a copy
    /// the Mac could not be reached for is tried again on the next removal, send,
    /// outbox drain or reconnect.
    private func forgetRemovedUploads(_ chat: String) {
        guard !previewMode, let saved = connection, let key = partition, !accessEnded,
              var intent = composers[chat], !intent.uploadsToForget().isEmpty else { return }
        // Keyed by computer, so a request still out to one computer never holds
        // back cleanup on another.
        let owner = key + "\n" + chat
        guard forgettingUploads.insert(owner).inserted else { return }
        let scope = assignmentScope
        Task { [weak self] in
            defer { self?.forgettingUploads.remove(owner) }
            struct Empty: Decodable, Sendable {}
            var tried: Set<String> = []
            // Each pass rereads the draft, so a removal made while a request
            // was out is sent before this cleanup ends.
            while true {
                guard let self, key == self.partition, scope == self.assignmentScope, !self.accessEnded,
                      var current = self.composers[chat],
                      let id = current.uploadsToForget().first(where: { !tried.contains($0) }) else { return }
                tried.insert(id)
                var removed = true
                do {
                    let _: Empty = try await self.api.request("/api/v1/conversations/\(Self.escape(chat))/files/\(Self.escape(id))",
                        origin: saved.origin, credential: saved.credential, method: "DELETE")
                } catch PairingFailure.response(let status) where status == 404 || status == 409 {
                    // Already gone, or a sent message carries it: nothing to forget.
                    removed = false
                } catch { return }
                guard key == self.partition, scope == self.assignmentScope, !self.accessEnded,
                      var next = self.composers[chat] else { return }
                next.forgotUpload(id)
                try? self.saveComposer(next, chat: chat)
                if removed { self.files[chat]?.removeAll { $0.id == id } }
            }
        }
    }
    func canAnnotate(_ chat: ChatSummary, replacing oldID: String? = nil) -> Bool {
        let replacesAnnotation = oldID.map { id in
            composers[chat.id]?.stagedFiles?.contains(where: { $0.id == id && $0.mimeType == ArtifactAnnotation.mimeType }) == true ||
            ((composers[chat.id]?.draftAttachmentIds ?? []).contains(id) &&
             files[chat.id]?.contains(where: { $0.id == id && $0.mimeType == ArtifactAnnotation.mimeType }) == true)
        } ?? false
        guard let detail = projects.details[chat.id], detail.isArchived != true,
              !chat.isArchived, !accessEnded,
              macConnected == true, composers[chat.id]?.pending == nil,
              (attachmentCount(chat.id) < 4 || replacesAnnotation), !uploading.contains(chat.id),
              !preparingSends.contains(chat.id) else { return false }
        if !previewMode { return connection != nil }
        #if WONDER_DIAGNOSTICS
        return ProcessInfo.processInfo.arguments.contains("-artifact-annotation-preview")
        #else
        return false
        #endif
    }

    func stageAnnotation(_ annotation: ArtifactAnnotation, chat: ChatSummary,
                         expectedScope: String, replacing oldID: String? = nil,
                         preserveStaleOnReplace: Bool = false) throws {
        guard canAnnotate(chat, replacing: oldID),
              expectedScope == assignmentScope,
              let detail = projects.details[chat.id], detail.projectId == annotation.projectId,
              detail.isArchived != true, annotation.conversationId == chat.id,
              !chat.isArchived, !accessEnded,
              (connection != nil || previewMode),
              !uploading.contains(chat.id), !preparingSends.contains(chat.id) else { throw FileFailure.integrity }
        loadComposer(chat.id)
        guard !intentLoadFailures.contains(chat.id) else { throw ReadFailure.resync }
        var intent = composers[chat.id] ?? ComposerIntent()
        guard intent.pending == nil else { throw SendFailure.pending }
        let replacedStale = preserveStaleOnReplace && oldID.map { isAnnotationStale($0, chat: chat.id) } == true
        if let oldID {
            let local = intent.stagedFiles?.contains(where: { $0.id == oldID && $0.mimeType == ArtifactAnnotation.mimeType }) == true
            let remote = (intent.draftAttachmentIds ?? []).contains(oldID) && files[chat.id]?.contains(where: {
                $0.id == oldID && $0.mimeType == ArtifactAnnotation.mimeType
            }) == true
            guard local || remote else { throw FileFailure.integrity }
            intent.removeAttachment(id: oldID)
        }
        guard intent.attachmentCount < 4 else { throw FileFailure.tooLarge }
        let staged = try annotation.stagedFile()
        intent.stagedFiles = (intent.stagedFiles ?? []) + [staged]
        try saveComposer(intent, chat: chat.id)
        forgetRemovedUploads(chat.id)
        if let oldID {
            staleAnnotationIDs[chat.id]?.remove(oldID)
            if replacedStale { staleAnnotationIDs[chat.id, default: []].insert(staged.id) }
        }
        controlErrors[chat.id] = nil
    }

    func isAnnotationStale(_ id: String, chat: String) -> Bool {
        staleAnnotationScope == assignmentScope && staleAnnotationIDs[chat]?.contains(id) == true
    }

    /// The host still validates the source hash at send. This early check gives
    /// an open draft a useful warning as soon as a refreshed preview changes.
    func noteWorkspaceRevision(_ chat: ChatSummary, rootID: String, path: String,
                               currentSha256: String) async {
        let scope = assignmentScope
        let key = AnnotationRevisionKey(scope: scope, chatID: chat.id, rootID: rootID, path: path)
        let requestID = UUID()
        annotationRevisionRequests[key] = requestID
        defer {
            if annotationRevisionRequests[key] == requestID { annotationRevisionRequests.removeValue(forKey: key) }
        }
        let intent = composers[chat.id] ?? ComposerIntent()
        var candidates: [(String, Data)] = (intent.stagedFiles ?? [])
            .filter { $0.mimeType == ArtifactAnnotation.mimeType }
            .map { ($0.id, $0.data) }
        for id in intent.draftAttachmentIds ?? [] {
            guard let file = files[chat.id]?.first(where: { $0.id == id && $0.mimeType == ArtifactAnnotation.mimeType }),
                  let data = try? await download(file, chat: chat) else { continue }
            candidates.append((id, data))
        }
        guard !Task.isCancelled, scope == assignmentScope, !accessEnded,
              annotationRevisionRequests[key] == requestID else { return }
        let currentIDs = Set((composers[chat.id]?.stagedFiles ?? []).map(\.id) +
                             (composers[chat.id]?.draftAttachmentIds ?? []))
        var stale = staleAnnotationScope == scope ? (staleAnnotationIDs[chat.id] ?? []) : []
        stale.formIntersection(currentIDs)
        for (id, data) in candidates {
            guard currentIDs.contains(id) else { continue }
            guard let annotation = try? ArtifactAnnotation.read(data),
                  annotation.conversationId == chat.id,
                  annotation.projectId == projects.details[chat.id]?.projectId,
                  annotation.rootId == rootID, annotation.path == path, !annotation.isDiff else { continue }
            if annotation.sourceSha256 == currentSha256 { stale.remove(id) }
            else { stale.insert(id) }
        }
        staleAnnotationScope = scope
        staleAnnotationIDs[chat.id] = stale
    }
    /// The file Send is uploading now and how many of the message's files are
    /// already on the computer, so the locked composer shows per-file progress.
    struct UploadProgress: Equatable {
        let fileID: String
        let name: String
        let done: Int
        let total: Int
        var receivedBytes: Int = 0
        var totalBytes: Int = 0
        var label: String { total > 1 ? "Uploading \(name) (\(done + 1) of \(total))" : "Uploading \(name)" }
    }
    @Published private(set) var uploadProgress: [String: UploadProgress] = [:]
    @Published private(set) var uploadFailures: [String: String] = [:]
    private var uploadTasks: [String: Task<Void, Error>] = [:]

    func cancelUpload(_ chat: ChatSummary) { cancelUpload(chat.id) }
    func cancelUpload(_ chatID: String) { uploadTasks[chatID]?.cancel() }
    func retryUpload(_ chat: ChatSummary) async {
        controlErrors[chat.id] = nil
        do { try await uploadStaged(chat) }
        catch { if uploadFailures[chat.id] == nil { controlErrors[chat.id] = AttachmentUploadFailure.message(for: error) } }
    }
    private func uploadStaged(_ chat: ChatSummary) async throws {
        if let task = uploadTasks[chat.id] { return try await task.value }
        let scope = assignmentScope
        let task = Task { try await self.performStagedUpload(chat) }
        uploadTasks[chat.id] = task
        defer { if scope == assignmentScope { uploadTasks[chat.id] = nil } }
        try await task.value
    }
    private func performStagedUpload(_ chat: ChatSummary) async throws {
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        #if WONDER_DIAGNOSTICS
        let uploadAPI = ProcessInfo.processInfo.arguments.contains("-diagnostics-video-upload") ? DiagnosticVideoUploadProtocol.api : api
        #else
        let uploadAPI = api
        #endif
        let scope = assignmentScope
        let key = partition
        uploading.insert(chat.id)
        uploadFailures[chat.id] = nil
        defer { if key == partition { uploading.remove(chat.id); uploadProgress[chat.id] = nil } }
        for file in composers[chat.id]?.stagedFiles ?? [] where file.uploaded == nil {
            let staged = composers[chat.id]?.stagedFiles ?? []
            uploadProgress[chat.id] = UploadProgress(fileID: file.id, name: file.name,
                done: staged.filter { $0.uploaded != nil }.count, total: staged.count, totalBytes: file.data.count)
            do {
                try Task.checkCancellation()
                let uploaded = try await uploadAPI.uploadAttachment(file,
                    conversationPath: "/api/v1/conversations/\(Self.escape(chat.id))",
                    origin: saved.origin, credential: saved.credential) { [weak self] received, total in
                        await self?.noteUploadBytes(chat.id, fileID: file.id, scope: scope, received: received, total: total)
                    }
                guard key == partition, scope == assignmentScope, !accessEnded else { throw CancellationError() }
                guard var next = composers[chat.id], let index = next.stagedFiles?.firstIndex(where: { $0.id == file.id }) else { throw CancellationError() }
                next.stagedFiles?[index].uploaded = uploaded
                try saveComposer(next, chat: chat.id)
            } catch {
                guard key == partition, scope == assignmentScope else { throw error }
                if Task.isCancelled {
                    // Host cancellation tombstones the upload UUID. Keep the stable chip ID
                    // and its bytes, but persist a fresh identity for an explicit retry.
                    if var next = composers[chat.id], let index = next.stagedFiles?.firstIndex(where: { $0.id == file.id }) {
                        next.stagedFiles?[index].uploadID = UUID().uuidString
                        try saveComposer(next, chat: chat.id)
                    }
                    uploadFailures[chat.id] = "Upload cancelled. Your media is saved."
                } else { uploadFailures[chat.id] = AttachmentUploadFailure.message(for: error) }
                throw error
            }
        }
    }
    private func noteUploadBytes(_ chat: String, fileID: String, scope: String, received: Int, total: Int) {
        guard scope == assignmentScope, uploadProgress[chat]?.fileID == fileID else { return }
        uploadProgress[chat]?.receivedBytes = received
        uploadProgress[chat]?.totalBytes = total
    }
    func loadFiles(_ chat: ChatSummary) async throws {
        if previewMode { return }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let key = partition
        let scope = assignmentScope
        try Task.checkCancellation()
        let items: [ConversationFile] = try await api.request("/api/v1/conversations/\(Self.escape(chat.id))/files", origin: saved.origin, credential: saved.credential)
        if key == partition, scope == assignmentScope, !Task.isCancelled { files[chat.id] = items }
    }
    func loadFilesIfNeeded(_ chat: ChatSummary, attachmentIDs: [String]) async {
        guard !previewMode, !accessEnded, !attachmentIDs.isEmpty else { return }
        let requested = Set(attachmentIDs)
        let known = Set((files[chat.id] ?? []).map(\.id))
        guard !requested.isSubset(of: known) else { return }
        let key = partition
        let scope = assignmentScope
        do {
            try Task.checkCancellation()
            try await loadFiles(chat)
            guard key == partition, scope == assignmentScope, !Task.isCancelled else { return }
        } catch is CancellationError {
            // The view or host scope changed; keep its fallback attachment chips.
        } catch {
            // Offline and unavailable hosts leave the useful fallback chips in place.
        }
    }
    func loadWorkspaceRoots(_ chat: ChatSummary) async throws -> WorkspaceRootsResponse {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-workspace-delayed-background-roots") {
                previewWorkspaceRootReads += 1
                if previewWorkspaceRootReads > 1 { try await Task.sleep(for: .seconds(5)) }
            }
            // The thread's own folder is a left-out worktree; the Project's
            // folders stay readable.
            if ProcessInfo.processInfo.arguments.contains("-project-files-worktree-outside"), !previewWorktreeAdded,
               !chat.id.hasPrefix("project-files:") {
                throw PairingFailure.worktreeNotInProject("This worktree is not one of the Project's folders. Add it to the Project on your computer to open it here.")
            }
            if chat.id.hasPrefix("project-files:") {
                if ProcessInfo.processInfo.arguments.contains("-project-files-root-revoked") { throw PairingFailure.response(403) }
                if ProcessInfo.processInfo.arguments.contains("-project-files-project-removed") { throw PairingFailure.response(404) }
                if ProcessInfo.processInfo.arguments.contains("-project-files-root-changed") {
                    previewProjectFilesRootReads += 1
                    if previewProjectFilesRootReads == 1 {
                        return WorkspaceRootsResponse(available: true, detail: nil, roots: [
                            WorkspaceRoot(id: "removed-root", label: "Workspace", path: "/preview-old", isDirectory: true, kind: "workingDirectory", readOnly: true)
                        ], attachments: [])
                    }
                }
            }
            if ProcessInfo.processInfo.arguments.contains("-workspace-delayed-roots") {
                try await Task.sleep(for: .seconds(5))
            }
            #endif
            return previewWorkspaceRoots(chat)
        }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let response: WorkspaceRootsResponse = try await api.request(
            "/api/v1/conversations/\(Self.escape(chat.id))/workspace/roots",
            origin: saved.origin,
            credential: saved.credential)
        guard response.available else { throw PairingFailure.response(503) }
        return response
    }
    /// URLComponents.path expects decoded path text. Use percentEncodedPath
    /// here because the conversation segment is already escaped exactly once.
    private static func workspaceComponents(conversationID: String, operation: String) -> URLComponents {
        var components = URLComponents()
        components.percentEncodedPath = "/api/v1/conversations/\(Self.escape(conversationID))/workspace/\(operation)"
        return components
    }
    static func workspaceEndpoint(conversationID: String, operation: String) -> String? {
        Self.workspaceComponents(conversationID: conversationID, operation: operation).string
    }
    func loadWorkspaceDirectory(_ chat: ChatSummary, root: WorkspaceRoot, path: String, showHidden: Bool, offset: Int = 0) async throws -> WorkspaceDirectoryPage {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-workspace-delayed-hidden"),
               path.isEmpty, showHidden {
                try await Task.sleep(for: .seconds(3))
            }
            if root.id == "removed-root" && ProcessInfo.processInfo.arguments.contains("-project-files-root-changed") {
                throw PairingFailure.response(404)
            }
            if path == "Projects" && ProcessInfo.processInfo.arguments.contains("-project-files-directory-moved") {
                throw PairingFailure.response(404)
            }
            #endif
            return previewWorkspaceDirectory(root: root, path: path, showHidden: showHidden, offset: offset)
        }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        var components = Self.workspaceComponents(conversationID: chat.id, operation: "directory")
        components.queryItems = [URLQueryItem(name: "root", value: root.id), URLQueryItem(name: "path", value: path), URLQueryItem(name: "showHidden", value: showHidden ? "true" : "false"), URLQueryItem(name: "offset", value: String(offset))]
        guard let endpoint = components.string else { throw PairingFailure.invalidLink }
        return try await api.request(endpoint, origin: saved.origin, credential: saved.credential)
    }
    /// The whole verified file, for a picture in a Markdown page.
    func downloadWorkspaceFile(_ chat: ChatSummary, root: WorkspaceRoot, entry: WorkspaceEntry,
                               countsAsOpen: Bool = true,
                               progress: (@Sendable (_ received: Int, _ expected: Int?) -> Void)? = nil) async throws -> Data {
        guard case .read(let read) = try await readWorkspaceFile(chat, root: root, entry: entry, countsAsOpen: countsAsOpen,
                                                                  progress: progress),
              !read.isPartial else { throw URLError(.badServerResponse) }
        return read.data
    }
    /// Reads a file for its preview: only its first `prefix` bytes when set
    /// (see `PairingAPI.readWorkspaceFile`), and `.unchanged` without a body
    /// while `validator` still names the file's revision.
    func readWorkspaceFile(_ chat: ChatSummary, root: WorkspaceRoot, entry: WorkspaceEntry,
                           prefix: Int? = nil, validator: String? = nil, countsAsOpen: Bool = true,
                           progress: (@Sendable (_ received: Int, _ expected: Int?) -> Void)? = nil) async throws -> WorkspaceFileReadResult {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            // A large fixture arrives in steps, like a slow Wi-Fi transfer;
            // a preview reads only its start, and an open one finds it unchanged.
            if ProcessInfo.processInfo.arguments.contains("-workspace-large-text-preview"), entry.name == "large.txt" {
                let total = Self.previewLargeTextBytes
                if validator != nil { return .unchanged }
                let count = prefix.map { min($0, total) } ?? total
                for step in 0...8 {
                    progress?(count * step / 8, count)
                    try await Task.sleep(for: .milliseconds(700))
                }
                return .read(WorkspaceFileRead(data: Data(repeating: 65, count: count), totalBytes: total,
                                               validator: "W/\"large-text-fixture\""))
            }
            if entry.path == "Weekend.pdf",
               ProcessInfo.processInfo.arguments.contains("-malformed-pdf-preview") {
                return .read(Self.wholeRead(Data("%PDF-1.7\ninvalid document".utf8)))
            }
            if countsAsOpen, ProcessInfo.processInfo.arguments.contains("-artifact-revision-preview") {
                // Opening always shows the original. The open preview's own
                // checks see the revision once the file has been opened enough
                // times (README needs a second open after its first comment).
                previewWorkspaceFileOpens[root.id + ":" + entry.path, default: 0] += 1
            }
            #endif
            return .read(Self.wholeRead(previewWorkspaceData(entry: entry)))
        }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let scope = assignmentScope
        var components = Self.workspaceComponents(conversationID: chat.id, operation: "file")
        components.queryItems = [URLQueryItem(name: "root", value: root.id), URLQueryItem(name: "path", value: entry.path)]
        guard let endpoint = components.string else { throw PairingFailure.invalidLink }
        let result = try await api.readWorkspaceFile(endpoint, connection: saved, byteSize: entry.byteSize.map(Int.init), sha256: nil,
                                                     mimeType: entry.mimeType, prefix: prefix, ifNoneMatch: validator,
                                                     progress: progress)
        guard scope == assignmentScope, !accessEnded else { throw CancellationError() }
        return result
    }
    private static func wholeRead(_ data: Data) -> WorkspaceFileRead {
        WorkspaceFileRead(data: data, totalBytes: data.count, validator: nil)
    }
    #if WONDER_DIAGNOSTICS
    static let previewLargeTextBytes = 40 * 1024 * 1024
    #endif

    /// The open preview's check for a newer revision of its file.
    func refreshWorkspaceFile(_ chat: ChatSummary, root: WorkspaceRoot, entry: WorkspaceEntry,
                              prefix: Int? = nil, validator: String? = nil) async throws -> WorkspaceFileReadResult {
        // Listing size belongs to the previous revision. Fetch the same path
        // without that expectation, then validate the advertised format again.
        let current = WorkspaceEntry(name: entry.name, path: entry.path, isDirectory: false,
                                     byteSize: nil, mimeType: entry.mimeType)
        #if WONDER_DIAGNOSTICS
        if previewMode, ProcessInfo.processInfo.arguments.contains("-artifact-revision-preview"),
           previewWorkspaceFileOpens[root.id + ":" + entry.path, default: 0] >= (entry.path == "README.md" ? 2 : 1) {
            if entry.path == "Weekend.pdf",
               ProcessInfo.processInfo.arguments.contains("-malformed-pdf-revision-preview") {
                return .read(Self.wholeRead(Data("%PDF-1.7\ninvalid revision".utf8)))
            }
            return .read(Self.wholeRead(previewWorkspaceRevisionData(entry: current)))
        }
        #endif
        let result = try await readWorkspaceFile(chat, root: root, entry: current, prefix: prefix, validator: validator,
                                                 countsAsOpen: false)
        if case .read(let read) = result, !read.isPartial, let mime = current.mimeType {
            try ConversationFile.validateContent(read.data, mime: mime)
        }
        return result
    }
    func workspaceMediaLoader(_ chat: ChatSummary, root: WorkspaceRoot, entry: WorkspaceEntry) throws -> WorkspaceMediaResourceLoader {
        var components = Self.workspaceComponents(conversationID: chat.id, operation: "media")
        components.queryItems = [URLQueryItem(name: "root", value: root.id), URLQueryItem(name: "path", value: entry.path)]
        guard let endpoint = components.string else { throw PairingFailure.invalidLink }
        #if WONDER_DIAGNOSTICS
        if previewMode, ProcessInfo.processInfo.arguments.contains("-workspace-media-preview") {
            let configuration = URLSessionConfiguration.ephemeral
            configuration.protocolClasses = [DiagnosticWorkspaceMediaProtocol.self]
            let credential = try JSONDecoder().decode(Credential.self, from: Data(
                #"{"sessionToken":"fixture-only","deviceId":"fixture","csrfToken":"fixture","hostInstallationId":"fixture"}"#.utf8))
            let fixture = SavedConnection(origin: "https://workspace-media.invalid", credential: credential)
            return WorkspaceMediaResourceLoader(api: PairingAPI(configuration: configuration),
                                                connection: fixture, path: endpoint)
        }
        #endif
        guard let saved = connection, !accessEnded, !previewMode else { throw PairingFailure.missingIdentity }
        return WorkspaceMediaResourceLoader(api: api, connection: saved, path: endpoint)
    }
    func loadWorkspaceGitStatus(_ chat: ChatSummary, root: WorkspaceRoot) async throws -> WorkspaceGitStatusResponse {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-workspace-delayed-git") {
                try await Task.sleep(for: .seconds(3))
                throw PairingFailure.response(503)
            }
            #endif
            return previewWorkspaceGitStatus()
        }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        var components = Self.workspaceComponents(conversationID: chat.id, operation: "git/status")
        components.queryItems = [URLQueryItem(name: "root", value: root.id)]
        guard let endpoint = components.string else { throw PairingFailure.invalidLink }
        return try await api.request(endpoint, origin: saved.origin, credential: saved.credential)
    }
    func loadWorkspaceGitDiff(_ chat: ChatSummary, root: WorkspaceRoot, path: String, staged: Bool) async throws -> WorkspaceDiffResponse {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-workspace-delayed-diff"), path == "README.md" {
                try await Task.sleep(for: .seconds(3))
            }
            #endif
            return WorkspaceDiffResponse(path: path, staged: staged, diff: "diff --git a/\(path) b/\(path)\n--- a/\(path)\n+++ b/\(path)\n@@\n-fixture line\n+updated fixture line\n")
        }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        var components = Self.workspaceComponents(conversationID: chat.id, operation: "git/diff")
        components.queryItems = [URLQueryItem(name: "root", value: root.id), URLQueryItem(name: "path", value: path), URLQueryItem(name: "staged", value: staged ? "true" : "false")]
        guard let endpoint = components.string else { throw PairingFailure.invalidLink }
        return try await api.request(endpoint, origin: saved.origin, credential: saved.credential)
    }
    private func previewWorkspaceRoots(_ chat: ChatSummary) -> WorkspaceRootsResponse {
        // The computer's folders are absolute paths; the home-folder fixture is what a Mac user's real one looks like.
        let folder = ProcessInfo.processInfo.arguments.contains("-workspace-home-path-preview") ? "/Users/maya/Projects/Atlas" : "/preview"
        let root = WorkspaceRoot(id: "workspace", label: "Workspace", path: folder, isDirectory: true, kind: "workingDirectory", readOnly: true)
        let attachments = files[chat.id] ?? []
        return WorkspaceRootsResponse(available: true, detail: nil, roots: [root], attachments: attachments)
    }
    private func previewWorkspaceDirectory(root: WorkspaceRoot, path: String, showHidden: Bool, offset: Int) -> WorkspaceDirectoryPage {
        let values: [WorkspaceEntry]
        #if WONDER_DIAGNOSTICS
        let readmeProject = ProcessInfo.processInfo.arguments.contains("-workspace-viewer-preview")
            ? DiagnosticTextViewerFixtures.projectEntries(in: path) : nil
        #else
        let readmeProject: [WorkspaceEntry]? = nil
        #endif
        if let readmeProject {
            values = readmeProject
        } else if path == "Sources" {
            values = [WorkspaceEntry(name: "Authentication.swift", path: "Sources/Authentication.swift", isDirectory: false, byteSize: nil, mimeType: "text/plain")]
        } else if path == "Projects" {
            values = [WorkspaceEntry(name: "Plan.md", path: "Projects/Plan.md", isDirectory: false, byteSize: 44, mimeType: "text/markdown")]
        } else {
            var rootEntries = [
                WorkspaceEntry(name: "Projects", path: "Projects", isDirectory: true, byteSize: nil, mimeType: nil),
                WorkspaceEntry(name: "README.md", path: "README.md", isDirectory: false, byteSize: 45, mimeType: "text/markdown"),
                WorkspaceEntry(name: "diagram.png", path: "diagram.png", isDirectory: false, byteSize: UInt64(previewBytes["Saturday.png"]?.count ?? 0), mimeType: "image/png")
            ]
            if showHidden { rootEntries.append(WorkspaceEntry(name: ".gitignore", path: ".gitignore", isDirectory: false, byteSize: 12, mimeType: "text/plain")) }
            if ProcessInfo.processInfo.arguments.contains("-artifact-annotation-preview") {
                rootEntries.append(WorkspaceEntry(name: "Weekend.pdf", path: "Weekend.pdf", isDirectory: false,
                                                   byteSize: UInt64(previewBytes["notes.pdf"]?.count ?? 0), mimeType: "application/pdf"))
            }
            #if WONDER_DIAGNOSTICS
            if ProcessInfo.processInfo.arguments.contains("-workspace-media-preview") {
                rootEntries.append(WorkspaceEntry(name: "black.mp4", path: "black.mp4", isDirectory: false,
                                                   byteSize: UInt64(DiagnosticWorkspaceMediaProtocol.video.count), mimeType: "video/mp4"))
            }
            if ProcessInfo.processInfo.arguments.contains("-workspace-document-preview") {
                rootEntries += DiagnosticWorkspaceFileFixtures.entries
            }
            if ProcessInfo.processInfo.arguments.contains("-workspace-viewer-preview") {
                rootEntries += DiagnosticTextViewerFixtures.entries
                rootEntries.append(WorkspaceEntry(name: "tag-mails", path: "tag-mails", isDirectory: true, byteSize: nil, mimeType: nil))
            }
            if ProcessInfo.processInfo.arguments.contains("-workspace-html-scroll-preview") ||
               ProcessInfo.processInfo.arguments.contains("-workspace-html-script-preview") {
                rootEntries.append(WorkspaceEntry(name: "reader.html", path: "reader.html", isDirectory: false,
                                                  byteSize: nil, mimeType: "text/html"))
            }
            if ProcessInfo.processInfo.arguments.contains("-workspace-large-text-preview") {
                rootEntries.append(WorkspaceEntry(name: "large.txt", path: "large.txt", isDirectory: false,
                                                  byteSize: UInt64(Self.previewLargeTextBytes), mimeType: "text/plain"))
            }
            #endif
            values = rootEntries
        }
        #if WONDER_DIAGNOSTICS
        let pageSize = ProcessInfo.processInfo.arguments.contains("-workspace-paged-preview") ? 2 : 200
        #else
        let pageSize = 200
        #endif
        let entries = Array(values.dropFirst(offset).prefix(pageSize))
        let nextOffset = offset + entries.count < values.count ? offset + entries.count : nil
        return WorkspaceDirectoryPage(rootId: root.id, path: path, parentPath: path.isEmpty ? nil : (path as NSString).deletingLastPathComponent,
                                      entries: entries, nextOffset: nextOffset)
    }
    private func previewWorkspaceData(entry: WorkspaceEntry) -> Data {
        #if WONDER_DIAGNOSTICS
        if ProcessInfo.processInfo.arguments.contains("-workspace-large-text-preview"), entry.name == "large.txt" {
            return Data(repeating: 65, count: Self.previewLargeTextBytes)
        }
        if ProcessInfo.processInfo.arguments.contains("-workspace-document-preview"),
           let data = DiagnosticWorkspaceFileFixtures.data(name: entry.name) { return data }
        if ProcessInfo.processInfo.arguments.contains("-workspace-viewer-preview"),
           let data = DiagnosticTextViewerFixtures.project[entry.path]?.data ?? DiagnosticTextViewerFixtures.data(name: entry.name) { return data }
        if ProcessInfo.processInfo.arguments.contains("-workspace-html-script-preview"), entry.name == "reader.html" {
            return Data("<h1>Script safety page</h1><p id='result'>Safe content remains</p><script>document.getElementById('result').textContent='SCRIPT EXECUTED'</script>".utf8)
        }
        if ProcessInfo.processInfo.arguments.contains("-workspace-html-scroll-preview"), entry.name == "reader.html" {
            let paragraphs = (1...30).map { "<p style='min-height:80px'>Reading section \($0)</p>" }.joined()
            return Data("<h1>Original reading page</h1>\(paragraphs)<p>End of original page</p>".utf8)
        }
        #endif
        if entry.mimeType?.hasPrefix("image/") == true, let data = previewBytes["Saturday.png"] { return data }
        if entry.name == "Weekend.pdf", let data = previewBytes["notes.pdf"] { return data }
        if entry.name == ".gitignore" { return Data(".DS_Store\n".utf8) }
        return Data("Fixture workspace file: \(entry.name)\n".utf8)
    }
    #if WONDER_DIAGNOSTICS
    private func previewWorkspaceRevisionData(entry: WorkspaceEntry) -> Data {
        if entry.name == "reader.html" {
            return Data("<h1>Revised reading page</h1><p>Accepted HTML revision</p>".utf8)
        }
        if entry.mimeType?.hasPrefix("image/") == true {
            return UIGraphicsImageRenderer(size: CGSize(width: 300, height: 180)).pngData { context in
                UIColor.systemIndigo.setFill(); context.fill(CGRect(x: 0, y: 0, width: 300, height: 180))
                ("Sunday" as NSString).draw(at: CGPoint(x: 24, y: 70), withAttributes: [.font: UIFont.systemFont(ofSize: 28), .foregroundColor: UIColor.white])
            }
        }
        if entry.name == "Weekend.pdf" {
            return UIGraphicsPDFRenderer(bounds: CGRect(x: 0, y: 0, width: 300, height: 420)).pdfData { context in
                for page in 1...2 {
                    context.beginPage()
                    ("Revised page \(page)\n\nReview the new schedule." as NSString)
                        .draw(in: CGRect(x: 24, y: 24, width: 252, height: 360),
                              withAttributes: [.font: UIFont.systemFont(ofSize: 16)])
                }
            }
        }
        return Data("Revised workspace file: \(entry.name)\nKeep Sunday open.\n".utf8)
    }
    #endif
    private func previewWorkspaceGitStatus() -> WorkspaceGitStatusResponse {
        WorkspaceGitStatusResponse(available: true, detail: nil, repositoryPath: "/preview", changes: [
            WorkspaceGitChange(path: "README.md", originalPath: nil, state: "staged", indexStatus: "M", worktreeStatus: " "),
            WorkspaceGitChange(path: "new-name.md", originalPath: "old-name.md", state: "renamed", indexStatus: "R", worktreeStatus: " "),
            WorkspaceGitChange(path: "scratch.txt", originalPath: nil, state: "untracked", indexStatus: "?", worktreeStatus: "?"),
            WorkspaceGitChange(path: "conflict.txt", originalPath: nil, state: "conflicted", indexStatus: "U", worktreeStatus: "U")
        ])
    }
    func download(_ file: ConversationFile, chat: ChatSummary) async throws -> Data {
        if previewMode, let data = previewBytes[file.id] { try file.verify(data,mime:file.mimeType); return data }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let key = partition
        let data = try await api.download("/api/v1/conversations/\(Self.escape(chat.id))/files/\(Self.escape(file.id))", connection: saved, file: file)
        guard key == partition, !accessEnded else { throw CancellationError() }
        return data
    }

    /// A picture for a conversation row: a JPEG the Mac makes from the
    /// verified original at the size the row draws it (much smaller to send),
    /// or the original. The full picture opens on tap.
    func thumbnail(_ file: ConversationFile, chat: ChatSummary, size: PicturePreviewSize) async throws -> Data {
        #if WONDER_DIAGNOSTICS
        if previewMode, ProcessInfo.processInfo.arguments.contains("-diagnostics-thumbnails-held") {
            // Holds every picture at its placeholder until `wonderctl signal thumbnails`.
            while !DiagnosticSubagentFixture.signalled("thumbnails") { try await Task.sleep(for: .milliseconds(100)) }
        }
        #endif
        if previewMode, let data = previewBytes[file.id] { try file.verify(data,mime:file.mimeType); return data }
        guard let saved = connection, !accessEnded else { throw PairingFailure.missingIdentity }
        let key = partition
        let data = try await api.downloadPreview("/api/v1/conversations/\(Self.escape(chat.id))/files/\(Self.escape(file.id))", connection: saved, file: file, maxPixels: size.maxPixels)
        guard key == partition, !accessEnded else { throw CancellationError() }
        return data
    }

    func loadAttention(afterCurrentRead: Bool = false) async {
        try? await refreshCoordinator.read(.attention, afterCurrent: afterCurrentRead) { [self] in await performAttentionRead() }
    }

    private func performAttentionRead() async {
        guard !previewMode else { return }
        guard let saved = connection, !accessEnded else { return }
        let key = partition
        // Reads can finish out of order: a list older than one already shown
        // must not hide newer requests or remove their saved answers.
        attentionReadsStarted += 1
        let ticket = attentionReadsStarted, readMark = store?.attentionReadMark()
        do {
            let items: [AttentionRequest] = try await api.request("/api/v1/approvals", origin: saved.origin, credential: saved.credential)
            if key == partition, ticket > attentionReadApplied {
                attentionReadApplied = ticket
                // Every 30 s: an unchanged answer must not re-render the Chats list.
                if (try? JSONEncoder().encode(items) != JSONEncoder().encode(attention)) ?? true { attention = items }
                InAppBannerCenter.shared.receiveAttention(items, host: saved.credential.hostInstallationId)
                // Global requests can name chats outside the loaded sidebar page.
                // Resolve their labels through metadata, without reading history.
                let catalogIDs = Set(projects.threads.values.flatMap { $0.threads.compactMap(\.conversationId) })
                for id in Set(items.compactMap(\.conversationId)).sorted() where projects.details[id] == nil && !catalogIDs.contains(id) {
                    _ = try? await projects.loadDetail(id)
                    guard key == partition, !Task.isCancelled else { return }
                }
                // A complete list: saved decisions and drafts for requests no
                // longer pending are done with. One with a reply in flight stays.
                if let store, let readMark {
                    let pending = Set(items.map(\.id))
                    if let gone = try? store.pruneAttentionIntents(pending: pending, keeping: resolving, readMark: readMark) {
                        for id in gone { savedDecisions.removeValue(forKey: id); forgetTypedAnswer(id) }
                    }
                    try? store.noteAttentionIntents(pending)
                }
                for item in items {
                    if let data = try store?.loadIntent(conversation: "decision-" + item.id) {
                        savedDecisions[item.id] = try JSONDecoder().decode(DecisionIntent.self, from: data)
                    }
                }
            }
        } catch { /* Background refresh keeps the last known requests. */ }
        guard key == partition, !Task.isCancelled else { return }
        // Keep background questions current too. This reads only their request
        // endpoint, never their conversation or presence state.
        var questionChats = Set(asyncQuestions.filter { !$0.value.isEmpty }.keys)
        // The host-wide pending list finds chats never opened or paged in on
        // this phone; an older host without it falls back to the catalog below.
        if let saved = connection, !accessEnded {
            let pending: [AsyncQuestion]? = try? await api.request("/api/v1/questions", origin: saved.origin, credential: saved.credential)
            guard key == partition, !Task.isCancelled else { return }
            for id in Set((pending ?? []).map(\.conversationId)).sorted() {
                questionChats.insert(id)
                if projects.details[id] == nil { _ = try? await projects.loadDetail(id) }
                guard key == partition, !Task.isCancelled else { return }
            }
        }
        // Discover requests in attached catalog rows once, including chats never
        // opened on this phone. Subsequent host question events maintain them.
        let catalogIDs = projects.threads.values.flatMap { $0.threads.compactMap(\.conversationId) }
        for id in catalogIDs where questionReads[id] == nil { questionChats.insert(id) }
        if let chat = selectedChat ?? visibleChat { questionChats.insert(chat.id) }
        for id in questionChats.sorted() {
            guard key == partition, !Task.isCancelled else { return }
            if ContinuousClock.now - (questionReads[id] ?? .now - .seconds(60)) >= .seconds(5) {
                await loadAsyncQuestions(attentionChat(id))
            }
        }

    }
    /// Request endpoints need only the host-verified conversation address.
    private func attentionChat(_ id: String) -> ChatSummary {
        ChatSummary(conversationId: id, title: "", lastMessagePreview: nil, lastMessageAt: nil,
                    messageCount: 0, deliveryState: nil, hasUnread: false, isArchived: false, isPinned: false)
    }
    private var questionReads: [String: ContinuousClock.Instant] = [:]
    private var attentionReadsStarted = 0, attentionReadApplied = 0
    /// The pending-requests list has been read from the computer at least once.
    var attentionReadLanded: Bool { previewMode || attentionReadApplied > 0 }
    @discardableResult
    func loadAsyncQuestions(_ chat: ChatSummary, afterCurrentRead: Bool = false) async -> [AsyncQuestion]? {
        struct Unavailable: Error {}
        var result: [AsyncQuestion]?
        do {
            try await refreshCoordinator.read(.questions(chat.id), afterCurrent: afterCurrentRead) { [self] in
                result = await performQuestionRead(chat)
                // Joining callers must observe failure too, rather than
                // treating a previously cached question list as a fresh read.
                guard result != nil else { throw Unavailable() }
            }
        } catch { return nil }
        return result ?? asyncQuestions[chat.id]
    }

    private func performQuestionRead(_ chat: ChatSummary) async -> [AsyncQuestion]? {
        guard let saved = connection, !accessEnded, !previewMode else { return nil }
        let key = partition
        do {
            let items: [AsyncQuestion] = try await api.request("/api/v1/conversations/\(Self.escape(chat.id))/questions", origin: saved.origin, credential: saved.credential)
            guard key == partition else { return nil }
            let currentIDs = Set(items.map(\.id))
            // Keep an unresolved saved reply reachable if the bounded host list
            // no longer includes its question. It can only check status or copy.
            let retained = (asyncQuestions[chat.id] ?? []).filter {
                !currentIDs.contains($0.id) && (try? store?.loadIntent(conversation: "async-reply-" + $0.id)) != nil
            }
            let visible = items + retained
            questionReads[chat.id] = .now
            let encoded = try JSONEncoder().encode(visible)
            // Polled: an unchanged list costs no write and no re-render.
            if try asyncQuestions[chat.id].map({ try JSONEncoder().encode($0) }) != encoded {
                try store?.saveIntent(encoded, conversation: "async-list-" + chat.id)
                asyncQuestions[chat.id] = visible
            }
            restoreAsyncReplies(visible, authoritativeIDs: currentIDs)
            return items
        } catch { return nil } // Keep saved questions until a successful refresh.
    }

    private func restoreAsyncReplies(_ questions: [AsyncQuestion], authoritativeIDs: Set<String>? = nil) {
        guard let store else { return }
        let now = UInt64(Date().timeIntervalSince1970 * 1000)
        for question in questions {
            do {
                guard let data = try store.loadIntent(conversation: "async-reply-" + question.id) else {
                    savedAsyncReplies.removeValue(forKey: question.id)
                    retryableAsyncReplies.remove(question.id)
                    // Answered, skipped or closed on the Mac: its draft is done with.
                    if authoritativeIDs?.contains(question.id) == true, question.state != "pending" {
                        forgetTypedAnswer(question.id)
                        try store.removeIntent(conversation: "answer-" + question.id)
                    }
                    continue
                }
                let answer = try JSONDecoder().decode(AsyncAnswerIntent.self, from: data)
                savedAsyncReplies[question.id] = answer
                let terminal = question.state == "answered" || question.state == "dismissed"
                if authoritativeIDs?.contains(question.id) == true, terminal,
                   let response = question.response,
                   response.answers == answer.answers, response.skip == answer.skip {
                    try store.removeIntent(conversation: "async-reply-" + question.id)
                    forgetTypedAnswer(question.id)
                    try store.removeIntent(conversation: "answer-" + question.id)
                    savedAsyncReplies.removeValue(forKey: question.id)
                    retryableAsyncReplies.remove(question.id)
                    attentionErrors[question.id] = nil
                    continue
                }
                if let authoritativeIDs, !authoritativeIDs.contains(question.id) {
                    retryableAsyncReplies.remove(question.id)
                    attentionErrors[question.id] = "Your computer no longer lists this question. Your saved reply is available to copy."
                } else if question.canAnswer(now: now) {
                    retryableAsyncReplies.insert(question.id)
                    if !resolving.contains(question.id), attentionErrors[question.id] == nil {
                        attentionErrors[question.id] = "Reply not confirmed. Check the saved reply before sending another answer."
                    }
                } else {
                    retryableAsyncReplies.remove(question.id)
                    attentionErrors[question.id] = terminal
                        ? "This question was answered or skipped. Your saved reply is available to copy."
                        : "This question expired. Your saved reply is available to copy."
                }
            } catch {
                // Status checking remains available after a local storage error.
                retryableAsyncReplies.insert(question.id)
                attentionErrors[question.id] = "The saved reply could not be read or updated. Free some storage and check again."
            }
        }
    }
    func replyAsync(_ question: AsyncQuestion, chat: ChatSummary, answers: [String], skip: Bool, retry: Bool = false) async {
        flushAnswerDraft(question.id)
        guard let saved = connection, let store, !accessEnded, !previewMode, !resolving.contains(question.id) else { return }
        let key = partition
        resolving.insert(question.id); attentionErrors[question.id] = nil
        defer { if key == partition { resolving.remove(question.id) } }
        do {
            if retry {
                // Status must be authoritative before replaying uncertain bytes.
                // Terminal or missing questions never receive another POST.
                guard let current = await loadAsyncQuestions(chat, afterCurrentRead: true) else {
                    guard key == partition else { return }
                    attentionErrors[question.id] = "Your computer could not confirm this question's status. Your saved reply is still here."
                    return
                }
                guard key == partition else { return }
                guard let latest = current.first(where: { $0.id == question.id }) else {
                    attentionErrors[question.id] = "Your computer no longer lists this question. Your saved reply is available to copy."
                    return
                }
                guard latest.canAnswer(now: UInt64(Date().timeIntervalSince1970 * 1000)) else { return }
            }
            let latest = asyncQuestions[chat.id]?.first(where: { $0.id == question.id }) ?? question
            guard latest.canAnswer(now: UInt64(Date().timeIntervalSince1970 * 1000)) else {
                restoreAsyncReplies([latest])
                return
            }
            let intentKey = "async-reply-" + question.id
            let payload: Data
            if let existing = try store.loadIntent(conversation: intentKey) { payload = existing }
            else {
                guard !retry else { throw ReadFailure.resync }
                let answer = AsyncAnswerIntent(answers: answers, skip: skip)
                if let error = answer.validationError(for: question) {
                    attentionErrors[question.id] = error.localizedDescription
                    return
                }
                payload = try JSONEncoder().encode(answer)
                try store.saveIntent(payload, conversation: intentKey)
            }
            savedAsyncReplies[question.id] = try JSONDecoder().decode(AsyncAnswerIntent.self, from: payload)
            retryableAsyncReplies.insert(question.id)
            struct Empty: Decodable, Sendable {}
            let _: Empty = try await api.request("/api/v1/conversations/\(Self.escape(chat.id))/questions/\(Self.escape(question.id))", origin: saved.origin, body: payload, credential: saved.credential)
            guard key == partition else { return }
            do {
                try store.removeIntent(conversation: intentKey)
                forgetTypedAnswer(question.id)
                try store.removeIntent(conversation: "answer-" + question.id)
                savedAsyncReplies.removeValue(forKey: question.id)
                retryableAsyncReplies.remove(question.id)
            } catch {
                attentionErrors[question.id] = "Your reply was accepted, but the saved copy could not be cleared. Check status again."
            }
            await loadAsyncQuestions(chat, afterCurrentRead: true)
            await loadChats(force: true)
        } catch PairingFailure.response(let code) where code == 400 || code == 413 {
            guard key == partition else { return }
            // These responses reject the answer before it is recorded. Keep
            // the editable draft, but allow a corrected reply or an explicit Skip.
            do {
                try store.removeIntent(conversation: "async-reply-" + question.id)
                savedAsyncReplies.removeValue(forKey: question.id)
                retryableAsyncReplies.remove(question.id)
                attentionErrors[question.id] = code == 413
                    ? "This reply is too long. Shorten your answers and try again."
                    : "This reply was not accepted. Check your answers and try again, or skip the question."
            } catch {
                attentionErrors[question.id] = "The saved reply could not be cleared. Free some storage and try again."
            }
            await loadAsyncQuestions(chat, afterCurrentRead: true)
        } catch {
            guard key == partition else { return }
            attentionErrors[question.id] = "Reply not confirmed. Check the saved reply; it may have expired or been answered elsewhere."
            await loadAsyncQuestions(chat, afterCurrentRead: true)
        }
    }
    func requests(for chat: ChatSummary) -> [AttentionRequest] {
        attention.filter { $0.belongs(to: chat.id) }
    }
    func answerDraft(_ id: String) -> [String: String] {
        if let typed = typedAnswerDrafts[id] { return typed }
        guard let data = try? store?.loadIntent(conversation: "answer-" + id) else { return [:] }
        return (try? JSONDecoder().decode([String: String].self, from: data)) ?? [:]
    }
    /// Writes at once (a choice, or clearing after Save), replacing any typed
    /// text still waiting for this form.
    func saveAnswerDraft(_ answers: [String: String], id: String) {
        answerDraftSaves.removeValue(forKey: id)?.cancel(); typedAnswerDrafts[id] = nil
        if !writeAnswerDraft(answers, id: id) { typedAnswerDrafts[id] = answers }
    }
    /// Typing in a question answer or a queued-message edit: kept in memory at
    /// once and written after a short pause, with one timer per form so
    /// unrelated forms never delay each other. Replying, leaving the chat,
    /// backgrounding and a pairing change write it first (`flushTypedDrafts`).
    func typeAnswerDraft(_ answers: [String: String], id: String) {
        guard typedAnswerDrafts[id] != answers else { return }
        typedAnswerDrafts[id] = answers
        answerDraftSaves[id]?.cancel()
        answerDraftSaves[id] = Task { [weak self] in
            try? await Task.sleep(for: Self.typedDraftDelay)
            guard !Task.isCancelled else { return }
            self?.flushAnswerDraft(id)
        }
    }
    /// Typed answers newer than their saved copy, by form; they belong to the
    /// current pairing's store and are written or dropped before it changes.
    private var typedAnswerDrafts: [String: [String: String]] = [:]
    private var answerDraftSaves: [String: Task<Void, Never>] = [:]
    func flushAnswerDraft(_ id: String) {
        answerDraftSaves.removeValue(forKey: id)?.cancel()
        guard let answers = typedAnswerDrafts[id] else { return }
        // A failed write keeps the text in memory for the next flush.
        if writeAnswerDraft(answers, id: id) { typedAnswerDrafts[id] = nil }
    }
    /// The request was answered: no typed text may write its draft back.
    private func forgetTypedAnswer(_ id: String) {
        answerDraftSaves.removeValue(forKey: id)?.cancel(); typedAnswerDrafts[id] = nil
    }
    private func writeAnswerDraft(_ answers: [String: String], id: String) -> Bool {
        do {
            // A cleared draft leaves no file behind.
            if answers.values.allSatisfy({ $0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }) {
                try store?.removeIntent(conversation: "answer-" + id)
            } else {
                try store?.saveIntent(JSONEncoder().encode(answers), conversation: "answer-" + id)
            }
            return true
        }
        catch { attentionErrors[id] = "This answer could not be saved. Free some storage before replying."; return false }
    }
    /// Sends a requested secret (`value`) or a refusal (`nil`) straight to the
    /// computer. Unlike other replies nothing is saved first: the value lives
    /// only in this call and the one request, so a failure asks the owner to retype it.
    @discardableResult
    func answerSecret(_ request: AttentionRequest, value: String?) async -> Bool {
        guard let saved = connection, !accessEnded, !resolving.contains(request.id) else { return false }
        let key = partition
        resolving.insert(request.id); attentionErrors[request.id] = nil
        defer { if partition == key { resolving.remove(request.id) } }
        do {
            let decision = SecretDecision(request: request, value: value)
            let target = SecretDecision.target(approvalID: request.id)
            let now = UInt64(Date().timeIntervalSince1970 * 1000)
            let signature = try await signingIdentity.sign(decision.transcript(path: target.signedTarget, connection: saved, issuedAtMs: now))
            guard !accessEnded, !Task.isCancelled, partition == key else { return false }
            struct Empty: Decodable, Sendable {}
            let _: Empty = try await api.request(target.requestPath, origin: saved.origin, body: decision.payload(issuedAtMs: now, signature: signature), credential: saved.credential)
            HandledNotification.clear(.request(host: saved.credential.hostInstallationId, id: request.id, nonce: request.actionNonce))
            attention.removeAll { $0.id == request.id }
            await loadAttention(afterCurrentRead: true)
            return true
        } catch {
            guard partition == key else { return false }
            if case PairingFailure.hostMessage(let status, let message) = error {
                attentionErrors[request.id] = message
                // Expired or already answered: the host closed it, so drop it instead of offering a retry.
                if status == 409 || status == 404 { await loadAttention(afterCurrentRead: true) }
            } else {
                attentionErrors[request.id] = "This could not be sent. Check your connection and try again."
            }
            return false
        }
    }
    @discardableResult
    func resolve(_ request: AttentionRequest, choice: PhoneApprovalChoice, answers: [String: String],
                 refreshAfterResolution: Bool = true, maySend: () -> Bool = { true }) async -> ApprovalResolutionOutcome {
        do {
            let response = try request.responseJSON(for: choice, answers: answers)
            return await resolve(request, decision: choice.decision, responseJSON: response, structuredDecision: choice.structuredDecision, refreshAfterResolution: refreshAfterResolution, maySend: maySend)
        } catch {
            attentionErrors[request.id] = "Check the required answers before replying."
            return .rejected
        }
    }
    @discardableResult
    func resolve(_ request: AttentionRequest, decision: String, answers: [String: String] = [:],
                 responseJSON: String? = nil, structuredDecision: JSONValue? = nil,
                 refreshAfterResolution: Bool = true, maySend: () -> Bool = { true }) async -> ApprovalResolutionOutcome {
        flushAnswerDraft(request.id)
        guard let saved = connection, !accessEnded, !Task.isCancelled, !resolving.contains(request.id), let store else { return .unconfirmed }
        let key = partition
        resolving.insert(request.id); attentionErrors[request.id] = nil
        defer { if partition == key { resolving.remove(request.id) } }
        do {
            var response = responseJSON
            if request.isQuestion && responseJSON == nil {
                // Question ids come from the agent; a repeated id keeps the last question.
                let values = Dictionary((request.params.questions ?? []).map {
                    ($0.id, ["answers": $0.answers(from: answers[$0.id] ?? "")])
                }, uniquingKeysWith: { _, last in last })
                let data = try JSONSerialization.data(withJSONObject: ["answers": values], options: [.sortedKeys, .withoutEscapingSlashes])
                response = String(decoding: data, as: UTF8.self)
            }
            let proposed = DecisionIntent(request: request, decision: decision, responseJson: response, structuredDecision: structuredDecision)
            let intent: DecisionIntent
            if let data = try store.loadIntent(conversation: "decision-" + request.id) {
                intent = try JSONDecoder().decode(DecisionIntent.self, from: data)
                guard intent.actionNonce == proposed.actionNonce else { throw ReadFailure.resync }
                // Only the same saved reply may be resent until it is confirmed:
                // an uncertain Allow must never be replaced with a Decline.
                guard intent.decision == proposed.decision,
                      intent.structuredDecision == proposed.structuredDecision,
                      intent.responseJson == proposed.responseJson else { throw SavedReplyConflict(saved: intent) }
            } else {
                intent = proposed
                try store.noteAttentionIntents([request.id])
                try store.saveIntent(JSONEncoder().encode(intent), conversation: "decision-" + request.id)
            }
            savedDecisions[request.id] = intent
            let target = ApprovalResolutionTarget(approvalID: request.id)
            let now = UInt64(Date().timeIntervalSince1970 * 1000)
            let signature = try await signingIdentity.sign(intent.transcript(path: target.signedTarget, connection: saved, issuedAtMs: now))
            guard !accessEnded, !Task.isCancelled, partition == key, maySend(),
                  connection?.credential.hostInstallationId == saved.credential.hostInstallationId,
                  connection?.credential.deviceId == saved.credential.deviceId else { throw CancellationError() }
            struct Empty: Decodable, Sendable {}
            let _: Empty = try await api.request(target.requestPath, origin: saved.origin, body: intent.payload(issuedAtMs: now, signature: signature), credential: saved.credential)
            // The computer accepted the answer: its notification's buttons are stale.
            HandledNotification.clear(.request(host: saved.credential.hostInstallationId, id: request.id, nonce: request.actionNonce))
            guard partition == key, !Task.isCancelled else { return .unconfirmed }
            // Acknowledged: the answer draft is done with. The signed decision
            // stays as the receipt (a repeated tap reuses its identity) until
            // a complete list of pending requests no longer has it.
            forgetTypedAnswer(request.id)
            try? store.removeIntent(conversation: "answer-" + request.id)
            attention.removeAll { $0.id == request.id }
            if refreshAfterResolution, !Task.isCancelled { await loadAttention(afterCurrentRead: true) }
            // A Mac reports the agent continuing over the event socket; the demo,
            // which has none, reads the thread again to start its reply.
            if isDemo, let chat = visibleChat, request.belongs(to: chat.id) { await refreshConversation(chat) }
            return .confirmed
        } catch PairingFailure.response(400) {
            guard partition == key, !Task.isCancelled else { return .unconfirmed }
            // The host rejects invalid replies before recording or forwarding
            // them. Let the owner correct a form or decline it; uncertain
            // transport failures still retain the exact signed intent below.
            do {
                try store.removeIntent(conversation: "decision-" + request.id)
                savedDecisions.removeValue(forKey: request.id)
                await loadAttention(afterCurrentRead: true)
                attentionErrors[request.id] = "This reply was not accepted. Check the requested details, or decline the request."
            } catch {
                attentionErrors[request.id] = "The saved reply could not be cleared. Free some storage and try again."
            }
            return .rejected
        } catch let conflict as SavedReplyConflict {
            guard partition == key, !Task.isCancelled else { return .unconfirmed }
            savedDecisions[request.id] = conflict.saved
            attentionErrors[request.id] = request.savedReplyConflict(conflict.saved)
        } catch {
            guard partition == key, !Task.isCancelled else { return .unconfirmed }
            if SigningIdentityFailure.requiresPairing(error) {
                try? requireIdentityRepair()
                stopForIdentityRecovery()
                return .unconfirmed
            }
            if !Task.isCancelled { await loadAttention(afterCurrentRead: true) }
            if case PairingFailure.hostMessage(_, let message) = error {
                // The host's own reason (expired when the provider restarted, already in progress).
                attentionErrors[request.id] = message
                return .unconfirmed
            }
            attentionErrors[request.id] = attention.contains(where: { $0.id == request.id })
                ? "Reply not confirmed. Try again to check the same saved answer."
                : "This request is no longer pending. It may have been resolved on another device or expired."
        }
        return .unconfirmed
    }

    /// A different reply than the one saved and not yet confirmed.
    private struct SavedReplyConflict: Error { let saved: DecisionIntent }

    func position(for chat: String) -> String? {
        savedPositions[chat] ?? (try? store?.loadPosition(conversation: chat)) ?? projection.positions[chat]
    }
    func feedRows(for chat: ChatSummary) -> [ReadRow] {
        ChatFeedEntry.visibleRows(rows(for: chat), queuedClientIDs: queuedClientIDs(chat.id))
    }
    /// The one owner of where a sent message shows: in the conversation, or
    /// as Queued below it. The Mac's queue, plus sends decided as queued that
    /// have not started. A send is placed once, from the turn state when it is
    /// made, and moves only when that state really changes.
    func queuedClientIDs(_ chat: String) -> Set<String> {
        var ids = Set((queues[chat] ?? []).map(\.clientMessageId))
        guard let waiting = queuedSends[chat], !waiting.isEmpty else { return ids }
        let started = Set((snapshots[chat]?.messages ?? []).filter { $0.state != "accepted_by_wonder" }.compactMap(\.clientMessageId))
        // A Send now from the queue stays in the conversation as Sending… until
        // the agent takes it or the Mac lists it in the queue again.
        let steering = Set((awaitingAcceptance[chat] ?? [:]).keys)
        ids.formUnion(waiting.subtracting(started).subtracting(steering))
        return ids
    }
    /// Starts showing a send as Sending… until the agent takes it.
    private func awaitAcceptance(_ id: String, chat: String, as kind: SendAcceptance) {
        awaitingAcceptance[chat, default: [:]][id] = kind
        awaitingSince[id] = ContinuousClock.now
    }
    private func stopAwaitingAcceptance(_ id: String, chat: String) {
        awaitingAcceptance[chat]?[id] = nil
        if awaitingAcceptance[chat]?.isEmpty == true { awaitingAcceptance[chat] = nil }
        awaitingSince[id] = nil
    }
    /// The Mac's message state ends Sending…: the agent took the message
    /// (`accepted_by_codex` or later), it failed, or the Mac queued it. A Mac
    /// that never answers (its request to the agent times out after 30 s)
    /// leaves the message's own delivery state to explain it.
    private func reconcileAcceptance(_ chat: String) {
        guard let waiting = awaitingAcceptance[chat], !waiting.isEmpty else { return }
        let unaccepted: Set<String> = ["submitting", "accepted_by_wonder", "dispatching_to_codex", "uncertain"]
        var states: [String: String] = [:]
        for message in snapshots[chat]?.messages ?? [] { if let id = message.clientMessageId { states[id] = message.state } }
        let listed = Set((queues[chat] ?? []).map(\.clientMessageId))
        for id in waiting.keys {
            let expired = awaitingSince[id].map { ContinuousClock.now - $0 > .seconds(35) } ?? true
            if listed.contains(id) || expired || states[id].map({ !unaccepted.contains($0) }) == true {
                stopAwaitingAcceptance(id, chat: chat)
            }
        }
    }
    /// Whether the thread waits for the owner in the app on the Mac, read
    /// from its own row when the sidebar's loaded rows don't include it (a
    /// notification for a Project the sidebar hasn't expanded, or a thread
    /// past its loaded pages). Forgotten when reading stops.
    @Published private(set) var computerWaitReads: [String: Bool] = [:]
    /// The sidebar's read of the thread; nil when no loaded row lists it.
    func sidebarWaitsOnMac(_ chat: String) -> Bool? {
        if let pin = projects.pinned.first(where: { $0.thread.conversationId == chat }) { return pin.thread.waitsOnMac }
        for state in projects.threads.values {
            if let row = state.threads.first(where: { $0.conversationId == chat }) { return row.waitsOnMac }
        }
        return nil
    }
    /// The loaded row's answer, else the thread's own read.
    func threadWaitsOnMac(_ chat: String) -> Bool { sidebarWaitsOnMac(chat) ?? computerWaitReads[chat] ?? false }
    /// Reads the thread's row from its Project on the Mac, independent of
    /// what the sidebar loaded: page by page (a thread waiting there was
    /// active moments ago, so it sorts near the top), at most `pages` pages.
    /// Returns whether it waits there, or nil when the Mac couldn't say.
    @discardableResult func readComputerWait(_ chat: ChatSummary, pages: Int = 4) async -> Bool? {
        guard let project = projectDetail(chat)?.projectId, !previewMode, !accessEnded else { return nil }
        let scope = assignmentScope
        var cursor: String?
        for _ in 0..<pages {
            var path = "/api/v1/projects/\(Self.escape(project))/threads?limit=30"
            if let cursor { path += "&cursor=" + Self.escape(cursor) }
            guard let page: ProjectThreadsPage = try? await manage(path), scope == assignmentScope, !Task.isCancelled else { return nil }
            if let row = page.threads.first(where: { $0.conversationId == chat.id }) {
                if computerWaitReads[chat.id] != row.waitsOnMac { computerWaitReads[chat.id] = row.waitsOnMac }
                return row.waitsOnMac
            }
            guard let next = page.nextCursor else { break }
            cursor = next
        }
        // No row lists it: nothing says it waits on the Mac.
        if computerWaitReads[chat.id] != false { computerWaitReads[chat.id] = false }
        return false
    }
    /// While a chat without a loaded row is open: reads whether it waits on
    /// the Mac, then reads again every `interval` while it does, so the line
    /// and View computer go once the computer was answered. Ends when the
    /// thread no longer waits, a loaded row takes over, or the task ends.
    func followComputerWait(_ chat: ChatSummary, interval: Duration = .seconds(15)) async {
        while !Task.isCancelled, sidebarWaitsOnMac(chat.id) == nil {
            guard await readComputerWait(chat) == true else { return }
            do { try await Task.sleep(for: interval) } catch { return }
        }
    }
    /// The thread waits for the owner in the app on the Mac (a permission
    /// prompt or a question there) and Cupola has nothing to answer for it:
    /// only the computer can. `waitsOnMac` is `threadWaitsOnMac(chat.id)`.
    func waitsOnComputer(_ chat: ChatSummary, waitsOnMac: Bool) -> Bool {
        guard waitsOnMac, isProject(chat), !accessEnded else { return false }
        return !SidebarSessionFilter.awaitsInput(conversation: chat.id, requests: attention, questions: asyncQuestions[chat.id] ?? [],
                                                 now: UInt64(max(0, Date().timeIntervalSince1970 * 1000)))
    }
    /// Why View computer can't show the computer's screen now, in the owner's
    /// words, or nil when it can (or the Mac didn't say: the computer view
    /// explains its own failures).
    static func computerViewUnavailable(connected: Bool?, sources: ComputerAvailableSources?, computer: String) -> String? {
        // Still connecting: the computer view shows that itself.
        if connected == false { return "\(computer) is offline." }
        guard connected == true, let capture = sources?.capture, !capture.available else { return nil }
        if let recovery = sources?.captureRecovery { return recovery }
        return capture.reason == "no_display"
            ? "\(computer)'s screen is asleep or off. Wake it to answer there."
            : "\(computer)'s screen can't be shown right now."
    }
    func computerViewUnavailable() async -> String? {
        guard macConnected == true, !previewMode else {
            return previewMode ? nil : Self.computerViewUnavailable(connected: macConnected, sources: nil, computer: macName)
        }
        let sources: ComputerAvailableSources? = try? await manage("/api/v1/computer/sources/windows")
        guard !Task.isCancelled else { return nil }
        return Self.computerViewUnavailable(connected: macConnected, sources: sources, computer: macName)
    }
    /// A tapped notification for a chat that waits on the computer: the chat
    /// opens its computer view, as View computer does. It lasts while the chat
    /// opens and its waiting state loads.
    struct ComputerRequest: Equatable {
        let conversation: String
        let until: Date
    }
    @Published private(set) var computerRequest: ComputerRequest?
    func requestComputer(for conversation: String) {
        computerRequest = ComputerRequest(conversation: conversation, until: Date().addingTimeInterval(20))
    }
    /// Takes this chat's request: true when it is still current.
    func takeComputerRequest(_ chat: String) -> Bool {
        guard let request = computerRequest, request.conversation == chat else { return false }
        computerRequest = nil
        return request.until > Date()
    }
    /// Queued sends the Mac's queue does not list yet (its reply is on the way).
    func unlistedQueuedSends(_ chat: String) -> [(id: String, body: String, attachmentIds: [String])] {
        let listed = Set((queues[chat] ?? []).map(\.clientMessageId))
        let ids = queuedClientIDs(chat).subtracting(listed)
        guard !ids.isEmpty else { return [] }
        let sends = pendingSends(chat).map { ($0.request.clientMessageId, $0.request.body, $0.request.attachmentIds) }
        let messages = (snapshots[chat]?.messages ?? []).compactMap { m in m.clientMessageId.map { ($0, m.body, m.attachmentIds) } }
        var seen = Set<String>()
        return (sends + messages).filter { ids.contains($0.0) && seen.insert($0.0).inserted }
            .map { (id: $0.0, body: $0.1, attachmentIds: $0.2) }
    }
    /// Whether a message sent now waits, from the turn state on screen: a
    /// running turn (here or on the Mac), messages already waiting, or a chat
    /// held on the Mac. Steered messages never queue here.
    /// A message sent now stops Claude's reply on the Mac and continues the conversation here.
    /// Send now can take this conversation over from the app on the Mac that
    /// holds it: a message would wait for it, and the Mac can stop it there.
    func offersTakeOver(_ chat: ChatSummary) -> Bool {
        (nativeOpenElsewhere.contains(chat.id) || t3CodeLimit(chat.id) != nil) && nativeHolders[chat.id]?.canTakeOver == true
    }
    /// What T3 Code's send can't carry ("attachments" past its size limits,
    /// "skills" or "model") in a message to a thread it has open: the waiting
    /// message's, as the Mac reports it, or the one being sent (by default the
    /// draft). Such a message waits for the thread there as without T3 Code.
    func t3CodeLimit(_ chat: String, sending request: SendRequest? = nil) -> String? {
        switch nativeHolders[chat]?.t3Code {
        case "cantSend": return nativeHolders[chat]?.t3CodeLimit ?? "content"
        case "sends":
            let skills = request.map(\.codexSkills) ?? composers[chat]?.codexSkills
            let model = request.map(\.projectModel) ?? projectNextModels[chat]
            return !(skills ?? []).isEmpty ? "skills" : model != nil ? "model" : nil
        default: return nil
        }
    }
    /// Messages to this thread go into T3 Code's thread on the Mac.
    func t3CodeSends(_ chat: String) -> Bool {
        nativeHolders[chat]?.t3Code == "sends"
    }
    /// Take over on a thread whose turn runs in T3 Code: T3 Code stops that
    /// turn and starts one with the message, in T3 Code itself.
    func offersT3TakeOver(_ chat: ChatSummary) -> Bool {
        t3CodeSends(chat.id) && turnRunsElsewhere(chat.id) && t3CodeLimit(chat.id) == nil
    }
    func sendStopsMacReply(_ chat: ChatSummary) -> Bool {
        turnRunsElsewhere(chat.id) && nativeStopsMacReply.contains(chat.id)
    }
    func sendWaits(_ chat: ChatSummary, sending request: SendRequest? = nil) -> Bool {
        let queued = !(queues[chat.id] ?? []).isEmpty
        // T3 Code takes the message into its thread there, whatever runs in it,
        // unless its send can't carry it.
        // An idle thread has nothing to wait for, so the limit alone (a chosen
        // model, skills) never reads as Queue.
        if nativeHolders[chat.id]?.t3Code == "sends" {
            let running = conversationWorking(chat.id) || turnRunsElsewhere(chat.id)
            return queued || (running && t3CodeLimit(chat.id, sending: request) != nil)
        }
        // A reply on the Mac that the message stops does not make it wait.
        let takesOver = sendStopsMacReply(chat)
        return ((conversationWorking(chat.id) && !takesOver) || (turnRunsElsewhere(chat.id) && !takesOver)
            || nativeOpenElsewhere.contains(chat.id) || queued)
    }
    private func prepareRows(_ snapshot: ConversationSnapshot, author: String, changes: [LiveChange] = []) -> [ReadRow] {
        #if WONDER_DIAGNOSTICS
        let projectStart = ProcessInfo.processInfo.systemUptime
        #endif
        let rows = rowProjection.rows(for: snapshot, author: author, changes: changes)
        #if WONDER_DIAGNOSTICS
        DiagnosticJournal.shared.record(DiagnosticEvent(operation: "rows.project", durationMs: (ProcessInfo.processInfo.systemUptime - projectStart) * 1000, count: UInt64(rows.count)))
        #endif
        return rows
    }
    /// The open conversation keeps its rows; projection reuse and catch-up
    /// events go. A read that started before this cannot catch up and is
    /// read again, rather than replaying from a log that lost events.
    func releaseUnderMemoryPressure() {
        // Queue the current projection before asking which bodies are durable.
        // Failed/locked writes keep their only in-memory copy for retry.
        writer?.schedule(projection)
        scheduleResidentTrim(underPressure: true)
        rowProjection = ConversationRowProjection()
        liveLog.release(through: projection.lastSequence)
        if visibleChat == nil { rowCache = nil; timelineCache = nil }
    }
    private func projectedRows(for chat: ChatSummary) -> [ReadRow] {
        let family = agentFamily(chat)
        if rowCache?.id != chat.id || rowCache?.title != chat.title || rowCache?.family != family {
            rowCache = (chat.id, chat.title, family, snapshots[chat.id].map { prepareRows($0, author: family?.title ?? chat.title) } ?? [])
        }
        return rowCache?.rows ?? []
    }
    private func pendingSends(_ chat: String) -> [PendingSend] {
        (composers[chat]?.recoveredPending ?? []) + [composers[chat]?.pending].compactMap { $0 }
    }
    func rows(for chat: ChatSummary) -> [ReadRow] {
        var rows = projectedRows(for: chat)
        for pending in pendingSends(chat.id) where !rows.contains(where: { $0.id == "user-" + pending.request.clientMessageId }) {
            rows.append(ReadRow(id: "user-" + pending.request.clientMessageId, author: "You",
                text: pending.request.body, isUser: true, timestamp: pending.createdAt, attachmentIds: pending.request.attachmentIds))
        }
        return rows
    }

    /// The open conversation's grouped timeline. Prepared once per content,
    /// queue, pending-send, active-turn or search-focus change; typing, scroll
    /// and unrelated model updates reuse it instead of regrouping all history.
    func timeline(for chat: ChatSummary, focusedRowID: String?) -> ConversationTimeline {
        _ = projectedRows(for: chat)
        let queued = queuedClientIDs(chat.id)
        let key = ConversationTimeline.Key(chatID: chat.id, title: chat.title, rows: rowRevision, queued: queued,
            pending: pendingSends(chat.id).map(\.request.clientMessageId),
            activeTurns: activeTurnIDs(chat.id), activeTurn: activeTurn(chat.id), focusedRowID: focusedRowID)
        if let timelineCache, timelineCache.key == key { return timelineCache.value }
        #if WONDER_DIAGNOSTICS
        let start = ProcessInfo.processInfo.systemUptime
        #endif
        var value = ConversationTimeline(chatID: chat.id,
            rows: ChatFeedEntry.visibleRows(rows(for: chat), queuedClientIDs: queued),
            activeTurnIDs: key.activeTurns, activeTurnID: key.activeTurn, focusedRowID: focusedRowID,
            turns: snapshots[chat.id]?.thread.turns)
        value.key = key
        #if WONDER_DIAGNOSTICS
        DiagnosticJournal.shared.record(DiagnosticEvent(operation: "timeline.group", durationMs: (ProcessInfo.processInfo.systemUptime - start) * 1000, count: UInt64(value.rows.count)))
        #endif
        timelineCache = (key, value)
        return value
    }
    func readReceipt(for conversation: String) -> VisibleReadReceipt? {
        return snapshots[conversation].map(VisibleReadReceipt.init(snapshot:))
    }
    func hasUnread(_ conversation: String) -> Bool {
        projects.hasUnread(conversation)
    }
    func acknowledgeVisibleRead(_ visible: VisibleReadReceipt) async {
        guard !previewMode, foreground, !accessEnded, macConnected == true,
            visibleChat?.id == visible.conversationId,
            let saved = connection, readReceipt(for: visible.conversationId) == visible,
            !projection.dirty.contains(visible.conversationId) else { return }
        let run = generation
        guard projects.details[visible.conversationId] != nil,
              projects.hasUnread(visible.conversationId), !projects.manuallyUnread.contains(visible.conversationId) else { return }
        let readRevision = projects.readRevision(visible.conversationId)
        struct Empty: Decodable, Sendable {}
        do {
            let _: Empty = try await api.request("/api/v1/conversations/" + Self.escape(visible.conversationId),
                origin: saved.origin, body: visible.requestBody(), credential: saved.credential, method: "PATCH")
            // Replies at or before this position are read on the computer too.
            HandledNotification.clear(.read(host: saved.credential.hostInstallationId, conversation: visible.conversationId,
                                            hostEpoch: visible.hostEpoch, sequence: visible.readThroughSequence))
            guard !Task.isCancelled, foreground, run == generation, visibleChat?.id == visible.conversationId,
                readReceipt(for: visible.conversationId) == visible, !projection.dirty.contains(visible.conversationId),
                projects.readRevision(visible.conversationId) == readRevision,
                connection?.credential.hostInstallationId == saved.credential.hostInstallationId,
                connection?.credential.deviceId == saved.credential.deviceId, !accessEnded else { return }
            projects.markRead(visible.conversationId)
        } catch {
            if case PairingFailure.response(401) = error { readFailed(error, run: run, origin: saved.origin, token: saved.credential.sessionToken) }
        }
    }

    /// Try again on a storage notice: write the failed reading positions and
    /// the chats again, and reload chats from the computer. A failure that
    /// recurs shows the notice again.
    func retryStorage() {
        guard let failure = storageFailure, !previewMode else { return }
        storageFailure = nil
        for (chat, position) in unsavedPositions { savePosition(position, chat: chat) }
        guard failure != .positionNotSaved else { return }
        writer?.schedule(projection)
        Task { [weak self] in await self?.check(userInitiated: true) }
    }

    func savePosition(_ position: String?, chat: String) {
        guard let position, !previewMode, savedPositions[chat] != position, let intents else { return }
        // A scroll anchor must not re-encode and replace the entire chat history,
        // nor write a file on the main thread. Replay cursor/invalidation
        // persistence remains atomic in ReadStore.save.
        intents.savePosition(position, conversation: chat)
        savedPositions[chat] = position; projection.positions[chat] = position
    }

    // Path monitoring and minute rechecks exist only while this host is in
    // the foreground. Rechecking a healthy route never changes normal UI state.
    private func startRouteMonitoring() {
        #if WONDER_DIAGNOSTICS
        // This scenario selects its own local-network state. Actual path
        // notifications and TLS remain separate integration checks.
        guard diagnosticRouteAPI == nil else { return }
        #endif
        guard foreground, !isDemo, !previewMode, !accessEnded, routeMonitor == nil else { return }
        let monitor = NWPathMonitor()
        let identity = UUID()
        routeMonitorID = identity
        routeMonitor = monitor
        updateHomeNetworkAccess()
        monitor.pathUpdateHandler = { [weak self] path in
            let satisfied = path.status == .satisfied
            let local = satisfied && (path.usesInterfaceType(.wifi) || path.usesInterfaceType(.wiredEthernet))
            Task { @MainActor [weak self] in
                guard let self, self.foreground, self.routeMonitorID == identity else { return }
                let joinedOrLeftWiFi = self.routeLocalNetworkAvailable != local
                self.routeLocalNetworkAvailable = local
                // No network: misses are not retried until it returns. Another
                // network may reach the computer, so the next attempt runs now.
                let networkReturned = satisfied && !self.recovery.pathAvailable
                self.recovery.setPath(available: satisfied)
                if joinedOrLeftWiFi || networkReturned { self.reconnectNow() }
                self.updateHomeNetworkAccess()
                if self.macConnected == true { _ = await self.recheckRoutes(earlierOnly: true) }
            }
        }
        monitor.start(queue: DispatchQueue(label: "wonder.connection-path", qos: .utility))
        routeTimer = Task { [weak self] in
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(60)) } catch { return }
                guard let self, self.foreground, self.routeMonitorID == identity else { return }
                // Out of reach, the replay loop owns trying again.
                guard self.macConnected == true || self.replay == nil else { continue }
                _ = await self.recheckRoutes(earlierOnly: self.macConnected == true)
            }
        }
    }

    private func updateHomeNetworkAccess() {
        guard foreground, !isDemo, !previewMode, !accessEnded,
              routeLocalNetworkAvailable, connection?.routes.contains(where: { $0.kind == .home }) == true else {
            homeNetworkAccess.stop(); return
        }
        homeNetworkAccess.start { [weak self] denied in
            guard let self, self.homeNetworkDenied != denied else { return }
            self.homeNetworkDenied = denied
            guard !denied else { return }
            self.reconnectNow()
            Task { _ = await self.recheckRoutes(earlierOnly: self.macConnected == true) }
        }
    }

    private func stopRouteMonitoring() {
        homeNetworkAccess.stop()
        routeMonitorID = UUID()
        routeMonitor?.cancel(); routeMonitor = nil
        routeTimer?.cancel(); routeTimer = nil
        routeChecks.cancel()
        recovery.reset()
    }

    /// Identical callers share a result; different purposes wait, then capture
    /// the latest route before running their own check. Only one handoff runs.
    private func recheckRoutes(excluding failedOrigin: String? = nil, earlierOnly: Bool = false) async -> Bool {
        guard !isDemo, !previewMode, let saved = connection, !saved.requiresPairing else { return false }
        guard saved.routes.count > 1 || saved.hostKeyPin != nil || saved.routes.contains(where: { $0.kind == .home || $0.learned }) else { return true }
        return await routeChecks.run(request: { [weak self] in
            guard let self, let saved = self.connection, !saved.requiresPairing else { return nil }
            return ConnectionRouteChecks.Request(generation: self.generation, routes: saved.routes, origin: saved.origin,
                hostKeyPin: saved.hostKeyPin, sessionToken: saved.credential.sessionToken,
                localNetworkAvailable: self.routeLocalNetworkAvailable && !self.homeNetworkDenied, earlierOnly: earlierOnly, excluding: failedOrigin)
        }, check: { [weak self] request in
            guard let saved = self?.connection, let api = self?.api else { return false }
            #if WONDER_DIAGNOSTICS
            let probeAPI = self?.diagnosticRouteAPI ?? api
            #else
            let probeAPI = api
            #endif
            let run = request.generation
            let keyChanged = RouteKeyMismatch()
            let trust = self?.homeNetworkTrust
            let sign = self?.signingIdentity.sealedSign
            let selector = ConnectionRouteSelector(clock: ContinuousClock()) { route in
                do { try await ConnectionRouteProber.check(route, saved: saved, api: probeAPI, keyMismatches: trust, sign: sign) }
                catch PairingFailure.wrongHost where saved.hostKeyPin != nil {
                    // A pinned route answered and proved another key.
                    keyChanged.note()
                    throw PairingFailure.wrongHost
                }
            }
            do {
                let result: ConnectionRouteSelector<ContinuousClock>.Selection?
                if request.earlierOnly {
                    result = try await selector.switchBack(saved.routes, currentOrigin: saved.origin,
                        localNetworkAvailable: request.localNetworkAvailable)
                } else {
                    result = try await selector.select(saved.routes, localNetworkAvailable: request.localNetworkAvailable,
                        excluding: request.excluding)
                }
                if !request.earlierOnly, !Task.isCancelled, let self, self.generation == run {
                    self.routeKeyChanged = result == nil && keyChanged.seen
                    // No way in: a narrower check of the same routes would only
                    // repeat these probes before the recovery owner's next attempt.
                    if result == nil { self.routeChecks.rememberMiss(request) }
                }
                guard let result, !Task.isCancelled, let self, self.generation == run,
                      let latest = self.connection,
                      latest.credential.hostInstallationId == saved.credential.hostInstallationId,
                      latest.credential.deviceId == saved.credential.deviceId,
                      latest.credential.sessionToken == saved.credential.sessionToken else { return false }
                guard result.route.origin != latest.origin else { return true }
                let selected = try latest.usingRoute(origin: result.route.origin)
                if self.foreground, self.socket != nil {
                    do { try await self.moveEventSocket(to: selected, run: run) }
                    catch PairingFailure.response(401) where api.isRelay(selected.origin) {
                        // The relay answered with the pinned key but its session
                        // ended: take the route; the check renews the session,
                        // and the live stream reopens on it.
                        try self.saveConnection(selected)
                        self.connection = selected
                    }
                } else {
                    try self.saveConnection(selected)
                    self.connection = selected
                }
                // Back home or on Tailscale: the relay's connection isn't needed.
                if api.isRelay(latest.origin) { await api.closeSealed(origin: latest.origin) }
                return true
            } catch {
                // A failed speculative route must leave the working route alone.
                return false
            }
        })
    }

    private func openEventSocket(_ saved: SavedConnection, run: UUID) async throws -> any LiveEventStream {
        #if WONDER_DIAGNOSTICS
        if let diagnosticEventStreamOpener {
            let opened = try await diagnosticEventStreamOpener(saved)
            guard run == generation, !Task.isCancelled else { opened.close(); throw CancellationError() }
            return opened
        }
        #endif
        if api.isRelay(saved.origin) {
            // On the relay, events come through the sealed connection.
            guard let pin = saved.hostKeyPin else { throw PairingFailure.wrongHost }
            let client = try await api.sealedConnection(origin: saved.origin, credential: saved.credential, pin: pin,
                                                        sign: signingIdentity.sealedSign)
            guard run == generation, !Task.isCancelled else { throw CancellationError() }
            let (id, events) = try await client.subscribe(hostEpoch: projection.hostEpoch, after: projection.lastSequence)
            return SealedEventStream(client: client, subscription: id, events: events)
        }
        let challenge: Challenge = try await api.request("/api/v1/events/challenge", origin: saved.origin, credential: saved.credential)
        guard run == generation, !Task.isCancelled else { throw CancellationError() }
        try challenge.validate(origin: saved.origin, hostID: saved.credential.hostInstallationId, deviceID: saved.credential.deviceId)
        let signature = try await signingIdentity.sign(challenge.transcript)
        guard run == generation, !Task.isCancelled else { throw CancellationError() }
        let candidate = try api.eventSocket(connection: saved)
        candidate.resume()
        do {
            struct Subscription: Encodable {
                let deviceId: String; let hostEpoch: String; let lastSequence: UInt64
                let csrfToken: String; let challengeId: String; let signature: String; let batch = true
            }
            // Read the cursor after challenge/signing, while the old socket can
            // still advance it. Both streams feed the same deduplicating projection.
            let payload = try JSONEncoder().encode(Subscription(deviceId: saved.credential.deviceId,
                hostEpoch: projection.hostEpoch, lastSequence: projection.lastSequence, csrfToken: saved.credential.csrfToken,
                challengeId: challenge.challengeId, signature: signature))
            try await candidate.send(.string(String(decoding: payload, as: UTF8.self)))
            guard run == generation, !Task.isCancelled else { throw CancellationError() }
            return SocketEventStream(task: candidate)
        } catch { candidate.cancel(with: .goingAway, reason: nil); throw error }
    }

    private nonisolated static func receiveEventFrame(_ socket: any LiveEventStream) async throws -> Data {
        try await socket.receiveFrame()
    }

    private func acknowledgeEvents(_ socket: any LiveEventStream) async throws {
        try await socket.acknowledge(hostEpoch: projection.hostEpoch, sequence: projection.lastSequence)
    }

    private func moveEventSocket(to selected: SavedConnection, run: UUID) async throws {
        try await ConnectionRouteHandoff.move(clock: ContinuousClock(), open: {
            try await self.openEventSocket(selected, run: run)
        }, firstFrame: { candidate in
            // An idle host sends no initial event data. Its first Pong confirms
            // the subscribed stream is alive; queued data stays for replay.
            try await candidate.confirmAlive()
            return nil
        }, commit: { candidate, data in
            guard self.generation == run, !Task.isCancelled, self.foreground, !self.accessEnded, !self.sessionLapsed,
                  self.refusedStreamSession != selected.credential.sessionToken, let latest = self.connection,
                  latest.credential.hostInstallationId == selected.credential.hostInstallationId,
                  latest.credential.deviceId == selected.credential.deviceId,
                  latest.credential.sessionToken == selected.credential.sessionToken else { throw CancellationError() }
            let updated = try latest.usingRoute(origin: selected.origin)
            if let data {
                let frame = try ReplayEvent.frame(data)
                try self.applyFrame(frame.events, batched: frame.batched, run: run)
            }
            try self.saveConnection(updated)
            let old = self.socket
            self.socket = candidate
            self.connection = updated
            self.streamOwner = (run, updated)
            self.streamConfirmedAt = nil
            self.confirmStreamActivity(candidate, run: run)
            old?.close()
        }, close: { $0.close() })
        if let socket { try? await acknowledgeEvents(socket) }
    }

    #if WONDER_DIAGNOSTICS
    /// Unit fixtures: `stream` stands in for the computer's live stream and
    /// replacements open through `open`, so a stream handoff runs without a socket.
    func installDiagnosticEventStream(_ stream: any LiveEventStream,
                                      open: @escaping @MainActor (SavedConnection) async throws -> any LiveEventStream) {
        foreground = true
        socket = stream
        if let saved = connection { streamOwner = (generation, saved) }
        streamConfirmedAt = .now
        diagnosticEventStreamOpener = open
    }

    /// Unit fixtures: runs the live-stream loop at `hostEpoch`, opening its
    /// streams through `open`.
    func startDiagnosticReplay(hostEpoch: String,
                               open: @escaping @MainActor (SavedConnection) async throws -> any LiveEventStream) {
        foreground = true
        diagnosticReplayEnabled = true
        diagnosticEventStreamOpener = open
        projection.hostEpoch = hostEpoch
        startReplay()
    }
    #endif

    private func startReplay() {
        // The demo has no event socket; its replies stream from `startDemoReply`.
        guard !isDemo else { return }
        #if WONDER_DIAGNOSTICS
        guard diagnosticReplayEnabled else { return }
        #endif
        let run = generation
        replay = Task { [weak self] in
            guard let self else { return }
            defer {
                if self.generation == run {
                    self.socket?.close(); self.socket = nil
                    self.replay = nil
                }
            }
            while !Task.isCancelled && self.generation == run && self.foreground && !self.accessEnded {
                do {
                    if self.socket == nil {
                        await self.check()
                        guard run == self.generation, !Task.isCancelled, self.foreground,
                              let saved = self.connection, !self.accessEnded else { return }
                        // The check already tried every way in.
                        guard self.macConnected == true else { throw ConnectionRecoveryMiss() }
                        if self.projection.hostEpoch.isEmpty { try await self.resnapshot(saved, run: run) }
                        let opened = try await self.openEventSocket(saved, run: run)
                        // The route or the session changed while it opened: a
                        // stream on a renewed-away session ends with the old
                        // session, so open again with the current one.
                        guard self.connection?.origin == saved.origin,
                              self.connection?.credential.sessionToken == saved.credential.sessionToken else {
                            opened.close()
                            continue
                        }
                        // A stream that opens after misses may have skipped a reply that
                        // finished meanwhile (no event for it is guaranteed to replay).
                        let recovered = self.recovery.failures > 0
                        self.socket = opened
                        self.streamOwner = (run, saved)
                        self.streamConfirmedAt = nil
                        self.recovery.succeeded()
                        if recovered { self.catchUpOpenChat(run: run) }
                        Task { await PushNotifications.shared.refreshMacDialogs(host: saved.credential.hostInstallationId) }
                    }
                    guard let active = self.socket else { continue }
                    do {
                        // A quiet stream is checked, and closed when the computer stops answering.
                        let data = try await ConnectionLiveness.receive(clock: ContinuousClock(),
                            receive: { try await ConnectionModel.receiveEventFrame(active) },
                            confirmAlive: {
                                try await active.confirmAlive()
                                guard await self.confirmStreamActivity(active, run: run) else { throw CancellationError() }
                            }, close: { await active.abandon() })
                        guard run == self.generation, !Task.isCancelled else { return }
                        // The replacement is live. Its subscription uses the
                        // shared cursor, which rejects overlapping events.
                        guard active === self.socket else { continue }
                        guard self.currentStreamEligible else {
                            active.close(); self.socket = nil
                            continue
                        }
                        #if WONDER_DIAGNOSTICS
                        let applyStart = ProcessInfo.processInfo.systemUptime
                        #endif
                        let frame = try ReplayEvent.frame(data)
                        let previousSequence = self.projection.lastSequence
                        do { try self.applyFrame(frame.events, batched: frame.batched, run: run) }
                        catch ReadFailure.resync {
                            active.close(); self.socket = nil
                            if let saved = self.connection { try await self.resnapshot(saved, run: run) }
                            continue
                        }
                        #if WONDER_DIAGNOSTICS
                        DiagnosticJournal.shared.record(DiagnosticEvent(operation: "replay.apply", durationMs: (ProcessInfo.processInfo.systemUptime - applyStart) * 1000, bytes: UInt64(data.count)))
                        #endif
                        if self.projection.lastSequence > previousSequence {
                            self.confirmStreamActivity(active, run: run)
                        }
                        try await self.acknowledgeEvents(active)
                    } catch ReadFailure.resync where active === self.socket {
                        // The relay's subscription ended: its cursor can't be replayed.
                        active.close(); self.socket = nil
                        if let saved = self.connection { try await self.resnapshot(saved, run: run) }
                        continue
                    } catch {
                        // Closing the superseded socket wakes its pending receive;
                        // that is a successful handoff, not a connection failure.
                        if active !== self.socket { continue }
                        active.close(); self.socket = nil
                        throw error
                    }
                } catch {
                    if Task.isCancelled || run != self.generation { return }
                    if !(error is ConnectionRecoveryMiss) {
                        let failedOrigin = self.connection?.origin
                        let kind = self.failureKind(error)
                        self.readFailed(error, run: run, origin: failedOrigin)
                        // Only an address that stopped answering is left for
                        // another way in; an ended session renews on this one.
                        if kind == .route { _ = await self.recheckRoutes(excluding: failedOrigin) }
                    }
                    guard run == self.generation, !self.accessEnded else { return }
                    self.recovery.failed()
                    guard await self.recovery.waitUntilDue(), run == self.generation else { return }
                    self.routeChecks.forgetMiss()
                }
            }
        }
    }

    private func resnapshot(_ saved: SavedConnection, run: UUID) async throws {
        struct Checkpoint: Decodable, Sendable { let hostEpoch: String; let lastSequence: UInt64; let hostInstallationId: String }
        let head: Checkpoint = try await api.request("/api/v1/sync/checkpoint", origin: saved.origin, credential: saved.credential)
        guard run == generation else { throw CancellationError() }
        guard head.hostInstallationId == saved.credential.hostInstallationId else { throw PairingFailure.wrongHost }
        var next = projection
        next.hostEpoch = head.hostEpoch
        next.lastSequence = head.lastSequence
        next.invalidatedThrough = nil
        liveLog = LiveEventLog()
        next.invalidateAll()
        // Commit invalidations and head together; all stale scopes stay marked
        // until a successful authoritative read replaces them.
        try commit(next, publishing: [])
        try await refreshList(saved, run: run)
        // Approvals retired or raised across the gap are not in the journal we just discarded.
        await loadAttention(afterCurrentRead: true)
        if let chat = visibleChat {
            await refreshConversation(chat, afterCurrentRead: true)
            await loadAsyncQuestions(chat)
            if isProject(chat) { try? await loadQueue(chat) }
        }
    }

    /// Applies one socket frame. Streamed text and items for a current
    /// snapshot apply in place (one publish per frame; the host sends at most
    /// ~20 a second); anything else marks its scope stale for the coalesced
    /// authoritative read. Throws `ReadFailure.resync` on a gap or epoch change.
    #if WONDER_DIAGNOSTICS
    func diagnosticConsumeRefreshEvents(_ events: [ReplayEvent], startingAt snapshot: ConversationSnapshot? = nil) throws {
        if let snapshot { projection.install(snapshot) }
        try applyFrame(events, batched: true, run: generation)
    }
    #endif

    /// Completed replies from the live stream; a catch-up replay of old
    /// events (after the app was away) is not news.
    private func announceFinishedReplies(_ events: [ReplayEvent]) {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        for event in events where event.event.type == "message_state" && event.event.state == "completed" {
            guard let conversation = event.conversationId else { continue }
            if let occurred = formatter.date(from: event.occurredAt) ?? ISO8601DateFormatter().date(from: event.occurredAt),
               Date().timeIntervalSince(occurred) > 120 { continue }
            replyFinished.send((conversation, event.messageId ?? event.turnId ?? event.eventId))
        }
    }

    #if WONDER_DIAGNOSTICS
    /// One interval per applied frame, for Instruments (PERF-5: the cost of a live delta).
    private static let liveSignposter = OSSignposter(subsystem: "com.swaymun.wonder", category: .pointsOfInterest)
    #endif

    /// Whether `events` continue the stream without a gap, an epoch change or a resync request.
    /// Checked before any of them changes the projection, so a frame applies whole or not at all.
    private func continues(_ events: [ReplayEvent]) -> Bool {
        var last = projection.lastSequence
        for event in events {
            guard event.event.type != "resync_required", event.hostEpoch == projection.hostEpoch else { return false }
            if event.sequence <= last { continue }
            guard last < UInt64.max, event.sequence == last + 1 else { return false }
            last = event.sequence
        }
        return true
    }

    private func applyFrame(_ events: [ReplayEvent], batched: Bool, run: UUID) throws {
        #if WONDER_DIAGNOSTICS
        let signpost = Self.liveSignposter.beginInterval("LiveApply", id: Self.liveSignposter.makeSignpostID(), "events=\(events.count)")
        let applyStart = ProcessInfo.processInfo.systemUptime
        defer {
            Self.liveSignposter.endInterval("LiveApply", signpost)
            DiagnosticJournal.shared.record(DiagnosticEvent(operation: "live.apply", durationMs: (ProcessInfo.processInfo.systemUptime - applyStart) * 1000, count: UInt64(events.count)))
        }
        #endif
        // Other chats' finished and failed replies become in-app banners.
        if let host = connection?.credential.hostInstallationId { InAppBannerCenter.shared.receive(events, host: host) }
        announceFinishedReplies(events)
        if liveLog.conversation != visibleChat?.id { liveLog.watch(visibleChat?.id, from: projection.lastSequence) }
        guard writer != nil, continues(events) else { throw ReadFailure.resync }
        var applied = Set<String>()
        // Each chat these events reach: what its row showed before, and whether
        // the model dropped its own copy so the projection owns the history's
        // storage and changes it in place instead of copying it.
        var touched = Set<String>()
        var before: [String: SnapshotSummary] = [:]
        var refreshEvents: [ReplayEvent] = []
        var dialogEvents: [ReplayEvent] = []
        for event in events {
            // Computer attention changes no conversation data. Advance the same
            // contiguous cursor without invalidating every chat on this host.
            if event.event.type == "activity", event.event.category == "mac_dialogs" {
                guard event.hostEpoch == projection.hostEpoch else { throw ReadFailure.resync }
                if event.sequence <= projection.lastSequence { continue }
                guard projection.lastSequence < UInt64.max, event.sequence == projection.lastSequence + 1 else { throw ReadFailure.resync }
                projection.lastSequence = event.sequence
                dialogEvents.append(event)
                continue
            }
            let unseen = event.sequence > projection.lastSequence
            let outcome: LiveOutcome
            if batched {
                if unseen, let conversation = event.conversationId, !conversation.isEmpty, touched.insert(conversation).inserted {
                    before[conversation] = summary(conversation)
                    if conversation == rowCache?.id { timelineCache = nil }
                    storedSnapshots[conversation] = nil
                }
                outcome = try projection.consumeLive(event)
            } else {
                try projection.consume(event)
                outcome = event.conversationId != nil || !event.event.isHostRuntimeNotice ? .invalidated(event.conversationId) : .skipped
            }
            guard unseen else { continue }
            if let id = event.conversationId, workingSnapshots.contains(id) { workTouched[id] = .now }
            #if WONDER_DIAGNOSTICS
            if let id = event.conversationId, LatencyHops.sendArmed.contains(id) {
                LatencyHops.mark("live.event", id, "type=\(event.event.type) category=\(event.event.category ?? "-")")
            }
            #endif
            liveLog.record(event)
            // A thread's work ending (native_status) wakes the open chat's check as a
            // transcript write does: a chat started in T3 Code can finish before the Mac
            // can say which session it is, and then only this wake (not the 15 s fallback) reads its reply.
            if event.event.type == "activity", event.event.category == "native_history" || event.event.category == "native_status",
               let chat = event.conversationId {
                nativeWakes.record(chat)
            }
            if case .applied(let conversation) = outcome { applied.insert(conversation) }
            if outcome != .skipped { refreshEvents.append(event) }
        }
        scheduleProjectionWrite()
        if let host = connection?.credential.hostInstallationId {
            PushNotifications.shared.receiveMacDialogEvents(dialogEvents, host: host, name: macName)
        }
        if applied.isEmpty { restoreMirror(touched) } else { publishLive(applied, touched: touched, before: before) }
        if applied.isEmpty, cachedConversationIds != projection.dirty { cachedConversationIds = projection.dirty }
        scheduleResidentTrim()
        for event in refreshEvents { scheduleEventRefresh(event, run: run) }
    }

    /// The disk copy follows a streamed frame at most about once a second. Every
    /// write keeps the writer's pending state, which shares storage with the
    /// open chat, so writing per frame made each frame's change copy the chat.
    /// Replay resumes from an older durable cursor if the app ends first.
    private func scheduleProjectionWrite() {
        guard projectionWrite == nil, let writer else { return }
        projectionWrite = Task { @MainActor [weak self] in
            try? await Task.sleep(for: .milliseconds(750))
            guard let self, !Task.isCancelled else { return }
            self.projectionWrite = nil
            if self.writer === writer { writer.schedule(self.projection) }
        }
    }
    private var projectionWrite: Task<Void, Never>?

    /// Puts the model's own copy of each chat back after events were applied.
    private func restoreMirror(_ ids: Set<String>) {
        for id in ids { storedSnapshots[id] = projection.snapshots[id] }
    }

    /// `installSnapshots` for chats whose changes are known exactly, without
    /// comparing whole histories: every event that applied moved the sequence.
    private func publishLive(_ ids: Set<String>, touched: Set<String>, before: [String: SnapshotSummary]) {
        let watched = Set([visibleChat?.id, selectedChat?.id, rowCache?.id].compactMap { $0 })
        let announceNow = ids.contains { watched.contains($0) }
        if announceNow { objectWillChange.send() }
        var changes: [String: [LiveChange]] = [:]
        for id in touched { changes[id] = projection.takeLiveChanges(id) }
        restoreMirror(touched)
        refreshLive(ids, changes: changes)
        let announceAfter = !announceNow && ids.contains { before[$0] != summary($0) }
        if announceAfter { objectWillChange.send() }
        #if WONDER_DIAGNOSTICS
        if announceNow || announceAfter { snapshotAnnouncements.announced += 1 } else { snapshotAnnouncements.silent += 1 }
        #endif
        for id in ids {
            if let snapshot = projection.snapshots[id], snapshot.activeTurnID != nil || snapshot.hasUnassignedPreTurnWork { workingSnapshots.insert(id) }
            else { workingSnapshots.remove(id) }
        }
        if hasSnapshotWork != !workingSnapshots.isEmpty { hasSnapshotWork = !workingSnapshots.isEmpty }
        armWorkWatchdog()
        setIfChanged(\.cachedConversationIds, projection.dirty)
    }

    /// What `snapshotsChanged(from:)` derives, for the chats a frame changed:
    /// the open chat's rows from the changes made to it, and the activity
    /// summaries only where an item changed (streamed text changes neither).
    private func refreshLive(_ ids: Set<String>, changes: [String: [LiveChange]]) {
        if let id = rowCache?.id, ids.contains(id), let title = rowCache?.title {
            let family = rowCache?.family
            if let snapshot = projection.snapshots[id] {
                // The projection changes its rows in place: let go of this copy first.
                rowCache?.rows = []
                rowCache = (id, title, family, prepareRows(snapshot, author: family?.title ?? title, changes: changes[id] ?? []))
            } else {
                rowCache = nil
                timelineCache = nil
                rowProjection = ConversationRowProjection()
            }
        }
        for chat in awaitingAcceptance.keys where ids.contains(chat) { reconcileAcceptance(chat) }
        for id in ids {
            guard changes[id]?.contains(where: \.touchesItems) != false, let snapshot = projection.snapshots[id] else { continue }
            currentActivities[id] = CurrentActivity.latest(in: snapshot)
            computerActivities[id] = ComputerAgentActivity.latest(in: snapshot)
        }
    }

    private var demoStreamingTurns: Set<String> = []
    /// The demo's scripted reply to a message sent (or a request answered) on the
    /// phone. It arrives as a batching Mac sends one, through the same frame path
    /// as the event socket: a tool step that starts and finishes, then the text in
    /// parts, then the completed text. The demo computer then reports the turn as
    /// finished, and an authoritative read settles it. Nothing leaves the phone.
    private func startDemoReply(_ chat: ChatSummary) {
        guard isDemo, let turn = projection.snapshots[chat.id]?.thread.turns?.last(where: \.isInProgress)?.id,
              let script = DemoServer.shared.pendingScript(conversation: chat.id, turn: turn),
              demoStreamingTurns.insert(turn).inserted else { return }
        let run = generation
        Task { [weak self] in
            var finished = false
            defer { if !finished { self?.demoStreamingTurns.remove(turn) } }
            let reply = turn + "-reply", command = turn + "-command"
            func item(_ id: String, _ type: String, _ state: String, _ fields: [String: Any]) -> EventBody {
                var item = fields; item["id"] = id; item["type"] = type
                let detail: [String: Any] = ["turnId": turn, "itemId": id, "state": state, "lifecycleAuthority": 2,
                                             "itemType": type, "item": item]
                let data = (try? JSONSerialization.data(withJSONObject: detail)) ?? Data()
                return EventBody(type: "activity", category: "thread_item_upsert", state: "updated", detail: String(decoding: data, as: UTF8.self))
            }
            var steps: [(Duration, [(EventBody, String?)])] = []
            if let line = script.command {
                steps.append((.milliseconds(500), [(item(command, "commandExecution", "started", ["command": line, "status": "inProgress"]), command)]))
                steps.append((.milliseconds(900), [(item(command, "commandExecution", "completed", ["command": line, "status": "completed", "exitCode": 0]), command)]))
            }
            steps.append((.milliseconds(400), [(item(reply, "agentMessage", "started", ["status": "inProgress"]), reply)]))
            for part in script.parts {
                steps.append((.milliseconds(260), [
                    (EventBody(type: "activity", category: "projection_changed", state: "updated", detail: #"{"resource":"assistant_messages","change":"update"}"#), reply),
                    (EventBody(type: "assistant_delta", text: part), reply)]))
            }
            steps.append((.milliseconds(300), [(EventBody(type: "assistant_completed", text: script.text), reply)]))
            for (delay, bodies) in steps {
                try? await Task.sleep(for: delay)
                guard let self, run == self.generation, !Task.isCancelled else { return }
                // Stopped from the phone: the demo computer already ended the turn.
                guard DemoServer.shared.pendingScript(conversation: chat.id, turn: turn) != nil else { break }
                let base = self.projection.lastSequence, epoch = self.projection.hostEpoch
                let now = ISO8601DateFormatter().string(from: Date())
                let frame = bodies.enumerated().map { index, entry in
                    ReplayEvent(eventId: "demo-\(base + UInt64(index) + 1)", hostEpoch: epoch, sequence: base + UInt64(index) + 1,
                                occurredAt: now, conversationId: chat.id, event: entry.0, turnId: turn, itemId: entry.1)
                }
                do { try self.applyFrame(frame, batched: true, run: run) } catch { return }
            }
            guard let self, run == self.generation else { return }
            finished = true
            DemoServer.shared.finish(conversation: chat.id, turn: turn)
            DemoServer.shared.advance(to: self.projection.lastSequence)
            await self.refreshConversation(chat)
            self.demoStreamingTurns.remove(turn)
            await self.projects.refresh()
        }
    }

    #if WONDER_DIAGNOSTICS
    /// The visible checkpoints are published only after the real model has
    /// committed the expected address. The receipt names addresses privately;
    /// the on-screen connection state stays in ordinary language.
    private func startDiagnosticRouteSwitch() {
        guard diagnosticRouteAPI != nil, diagnosticRouteTask == nil else { return }
        let run = generation
        diagnosticRouteTask = Task { [weak self] in
            guard let self, let original = self.connection,
                  let backup = original.routes.first(where: { $0.kind == .tailscale })?.origin else { return }
            defer { if self.generation == run { self.diagnosticRouteTask = nil } }
            let home = DiagnosticRouteSwitchProtocol.home.origin
            let receipt = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
                .appendingPathComponent("routes-switch.json")
            var stages: [[String: Any]] = []
            @MainActor func record(_ stage: String, expected: String, succeeded: Bool) throws {
                try Task.checkCancellation()
                let passed = succeeded && self.generation == run && self.macConnected == true && self.connection?.origin == expected
                stages.append(["stage": stage, "passed": passed, "origin": self.connection?.origin ?? "",
                    "connected": self.macConnected == true, "sequence": self.projection.lastSequence,
                    "realSocket": self.socket != nil])
                try JSONSerialization.data(withJSONObject: stages, options: [.prettyPrinted, .sortedKeys]).write(to: receipt, options: .atomic)
                guard passed else { throw URLError(.cannotConnectToHost) }
                self.diagnosticRouteCheckpoint = "Synthetic fixture · " + stage
            }
            func waitForSignal(_ signal: String) async throws {
                while !DiagnosticSubagentFixture.signalled(signal) {
                    try await Task.sleep(for: .milliseconds(50))
                }
                try Task.checkCancellation()
            }
            do {
                await self.check()
                self.routeLocalNetworkAvailable = true
                let initial = await self.recheckRoutes()
                try record("Ready", expected: home, succeeded: initial)
                try await waitForSignal("routes-block-home")
                let fallback = await self.recheckRoutes(excluding: home)
                try record("Backup ready", expected: backup, succeeded: fallback)
                try await waitForSignal("routes-check")
                let count = DiagnosticRouteSwitchProtocol.homeChecks
                let quiet = Task { await self.recheckRoutes(earlierOnly: true) }
                defer { quiet.cancel() }
                let deadline = ContinuousClock().now.advanced(by: .seconds(2))
                while DiagnosticRouteSwitchProtocol.homeChecks == count {
                    guard ContinuousClock().now < deadline else { throw URLError(.timedOut) }
                    try await Task.sleep(for: .milliseconds(5))
                }
                // Exercise the actual Check connection caller while the
                // failed earlier-route check is in flight.
                await self.check(renew: true, userInitiated: true)
                let quietResult = await quiet.value
                try record("Checked", expected: backup, succeeded: !quietResult)
                try await waitForSignal("routes-restore-home")
                let before = DiagnosticRouteSwitchProtocol.homeChecks
                let restored = await self.recheckRoutes(earlierOnly: true)
                try record("Restored", expected: home,
                    succeeded: restored && DiagnosticRouteSwitchProtocol.homeChecks == before + 1)
            } catch is CancellationError { }
            catch { self.diagnosticRouteCheckpoint = "Synthetic fixture · Failed" }
        }
    }

    private var diagnosticReasoningStarted = false
    /// Offline event-socket fixture. Both phases wait for the scenario, so no
    /// assertion depends on catching a short-lived streaming state.
    private func startDiagnosticReasoningStream(_ chat: ChatSummary) {
        guard DiagnosticSubagentFixture.streamReasoningFixture, !diagnosticReasoningStarted,
              chat.id == DiagnosticSubagentFixture.parentID else { return }
        diagnosticReasoningStarted = true
        let run = generation
        Task { [weak self] in
            for finished in [false, true] {
                let signal = finished ? "reasoning-finish" : "reasoning-text"
                while !DiagnosticSubagentFixture.signalled(signal) {
                    do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
                    guard let self, run == self.generation else { return }
                }
                guard let self, !Task.isCancelled, run == self.generation else { return }
                let base = self.projection.lastSequence, epoch = self.projection.hostEpoch
                let state = finished ? "completed" : "started"
                let marker = finished ? "The streamed reasoning finished." : "The streamed reasoning update arrived."
                // Deliberately no summary: older hosts can send just display text.
                let thought = #"{"turnId":"thinking-turn","itemId":"measured-thinking","itemType":"reasoning","state":"\#(state)","lifecycleAuthority":2,"text":"Streamed private thinking must stay hidden.","item":{"id":"measured-thinking","type":"reasoning","status":"\#(finished ? "completed" : "inProgress")"}}"#
                let reply = #"{"turnId":"thinking-turn","itemId":"stream-marker","itemType":"agentMessage","state":"completed","lifecycleAuthority":2,"text":"\#(marker)","item":{"id":"stream-marker","type":"agentMessage","status":"completed"}}"#
                let events = [thought, reply].enumerated().map { index, detail in
                    ReplayEvent(eventId: "reasoning-\(base + UInt64(index) + 1)", hostEpoch: epoch,
                                sequence: base + UInt64(index) + 1,
                                occurredAt: String(Int64(Date().timeIntervalSince1970 * 1000)), conversationId: chat.id,
                                event: EventBody(type: "activity", category: "thread_item_upsert", state: "updated", detail: detail),
                                turnId: "thinking-turn", itemId: index == 0 ? "measured-thinking" : "stream-marker")
                }
                do { try self.applyFrame(events, batched: true, run: run) } catch { return }
            }
        }
    }

    private var diagnosticReachabilityTask: Task<Void, Never>?
    private var diagnosticReachabilityStartSequence: UInt64?
    private var diagnosticProbeRecovered = false
    var diagnosticReachabilityState: [String: Any] {
        ["connected": macConnected.map { $0 as Any } ?? NSNull(),
         "streamOpen": eventStreamLive, "receivingUpdates": composerReceivesUpdates,
         "advanced": diagnosticReachabilityStartSequence.map { projection.lastSequence > $0 } ?? false,
         "outboxCount": composers.values.reduce(0) { $0 + ($1.outbox?.count ?? 0) },
         "probeRecovered": diagnosticProbeRecovered]
    }

    /// One failed auxiliary probe and a still-delivering event stream. The
    /// scenario holds HTTP acceptance until probe-back; no real host is used.
    private func startDiagnosticReachabilityStream(_ chat: ChatSummary) {
        guard DiagnosticSubagentFixture.liveReachabilityFixture, diagnosticReachabilityTask == nil,
              let turn = projection.snapshots[chat.id]?.thread.turns?.last(where: \.isInProgress)?.id else { return }
        let stream = DiagnosticReachabilityEventStream(), run = generation
        diagnosticReachabilityStartSequence = projection.lastSequence
        startDiagnosticReplay(hostEpoch: projection.hostEpoch) { _ in stream }
        diagnosticReachabilityTask = Task { [weak self] in
            do {
                while !DiagnosticSubagentFixture.signalled("probe-fails") { try await Task.sleep(for: .milliseconds(100)) }
                guard let self, run == self.generation, !Task.isCancelled else { return }
                await self.check()
                while !DiagnosticSubagentFixture.signalled("stream-alive") { try await Task.sleep(for: .milliseconds(100)) }
                var first = true, text = ""
                repeat {
                    guard run == self.generation, !Task.isCancelled else { return }
                    let base = self.projection.lastSequence, item = "reachability-live-reply"
                    var bodies: [EventBody] = []
                    if first {
                        bodies.append(EventBody(type: "activity", category: "thread_item_upsert", state: "updated",
                            detail: #"{"turnId":"\#(turn)","itemId":"\#(item)","state":"started","lifecycleAuthority":2,"itemType":"agentMessage","item":{"id":"\#(item)","type":"agentMessage","status":"inProgress"}}"#))
                    }
                    bodies.append(EventBody(type: "activity", category: "projection_changed", state: "updated",
                        detail: #"{"resource":"assistant_messages","change":"update"}"#))
                    let delta = first ? "The stream is still arriving. " : "More live updates. "
                    text += delta
                    bodies.append(EventBody(type: "assistant_delta", text: delta))
                    let events = bodies.enumerated().map { offset, body in
                        ReplayEvent(eventId: "reachability-\(base + UInt64(offset) + 1)", hostEpoch: self.projection.hostEpoch,
                            sequence: base + UInt64(offset) + 1, occurredAt: "2000", conversationId: chat.id,
                            event: body, turnId: turn, itemId: item)
                    }
                    DiagnosticSubagentFixture.recordReachability(text: text, sequence: base + UInt64(bodies.count))
                    stream.yield(try JSONEncoder().encode(events))
                    first = false
                    try await Task.sleep(for: .seconds(1))
                    if DiagnosticSubagentFixture.signalled("probe-back"), !self.diagnosticProbeRecovered {
                        await self.check()
                        self.diagnosticProbeRecovered = self.macConnected == true
                    }
                } while !Task.isCancelled
            } catch { stream.close() }
        }
    }

    private var diagnosticHostDownStarted = false
    /// `-diagnostics-host-down`: a moment after the fixture chat is on screen (with
    /// `-diagnostics-host-down-early`, as it starts to open), Cupola stops on the synthetic
    /// computer and the app notices at its next check, as it would on its own.
    func diagnosticChatShown() {
        if !DiagnosticSubagentFixture.hostDownEarly { startDiagnosticHostDown() }
    }
    private func startDiagnosticHostDown() {
        guard DiagnosticSubagentFixture.hostDownFixture, !diagnosticHostDownStarted else { return }
        diagnosticHostDownStarted = true
        let run = generation
        let delay = DiagnosticSubagentFixture.hostDownEarly ? 0.0 : 2.5
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(delay))
            guard let self, run == self.generation, !Task.isCancelled else { return }
            DiagnosticSubagentFixture.stopHost()
            await self.check(renew: true)
        }
    }

    private var diagnosticStreamStarted = false
    /// `-diagnostics-stream-reply`: once the open fixture chat has loaded, a
    /// reply streams into its running turn as a batching host sends it (an
    /// item start, then a text change and its delta every second, then the
    /// completed text), through the same frame path as the event socket. The
    /// last part waits for `wonderctl signal stream`, so the scenario sees the
    /// reply still streaming however slow the simulator is. The fixture's HTTP
    /// history never contains this reply.
    private func startDiagnosticStream(_ chat: ChatSummary) {
        startDiagnosticReasoningStream(chat)
        startDiagnosticReachabilityStream(chat)
        if DiagnosticSubagentFixture.hostDownEarly { startDiagnosticHostDown() }
        guard DiagnosticSubagentFixture.streamReplyFixture, !diagnosticStreamStarted,
              let turn = projection.snapshots[chat.id]?.thread.turns?.last(where: \.isInProgress)?.id else { return }
        diagnosticStreamStarted = true
        let run = generation
        Task { [weak self] in
            let item = "live-stream-reply"
            let longMarkdown = ProcessInfo.processInfo.arguments.contains("-diagnostics-stream-long-markdown")
            let parts = longMarkdown ? DiagnosticSubagentFixture.longMarkdownStream
                : DiagnosticSubagentFixture.appStoreFixture
                ? ["## Fix intermittent sign-out after wake\n\n", "**Problem:** after the app wakes, ", "concurrent requests could each refresh ", "an expired token. The losing refresh ", "used a revoked token, and the server ", "signed the user out.\n\n", "**Fix:** `TokenRefresher` is an actor ", "that runs one refresh at a time; ", "other requests await its result.\n\n", "**Tests:** "]
                : ["one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"]
            var text = ""
            for index in 0...(parts.count + 1) {
                try? await Task.sleep(for: longMarkdown ? .milliseconds(300) : .seconds(1))
                while index == 1, DiagnosticSubagentFixture.sendHeldFixture,
                      !DiagnosticSubagentFixture.signalled("first-token") {
                    try? await Task.sleep(for: .milliseconds(100))
                    guard let self, !Task.isCancelled, run == self.generation else { return }
                }
                while index == parts.count, !DiagnosticSubagentFixture.signalled("stream") {
                    try? await Task.sleep(for: .milliseconds(100))
                    guard let self, run == self.generation else { return }
                }
                guard let self, run == self.generation else { return }
                let base = self.projection.lastSequence, epoch = self.projection.hostEpoch
                func event(_ offset: UInt64, _ body: EventBody) -> ReplayEvent {
                    ReplayEvent(eventId: "stream-\(base + offset)", hostEpoch: epoch, sequence: base + offset,
                                occurredAt: ISO8601DateFormatter().string(from: Date()), conversationId: chat.id, event: body, turnId: turn, itemId: item)
                }
                let frame: [ReplayEvent]
                if index == 0 {
                    let detail = #"{"turnId":"\#(turn)","itemId":"\#(item)","state":"started","lifecycleAuthority":2,"itemType":"agentMessage","item":{"id":"\#(item)","type":"agentMessage","status":"inProgress"}}"#
                    frame = [event(1, EventBody(type: "activity", category: "thread_item_upsert", state: "updated", detail: detail))]
                } else if index <= parts.count {
                    let delta = DiagnosticSubagentFixture.appStoreFixture || longMarkdown ? parts[index - 1] : "Streaming reply, part \(parts[index - 1]). "
                    text += delta
                    frame = [event(1, EventBody(type: "activity", category: "projection_changed", state: "updated", detail: #"{"resource":"assistant_messages","change":"update"}"#)),
                             event(2, EventBody(type: "assistant_delta", text: delta))]
                        + (DiagnosticSubagentFixture.largeHistoryFixture ? [ReplayEvent(eventId: "stream-tool-\(base + 3)", hostEpoch: epoch, sequence: base + 3,
                            occurredAt: ISO8601DateFormatter().string(from: Date()), conversationId: chat.id,
                            event: EventBody(type: "activity", category: "thread_item_upsert", state: "updated",
                                detail: #"{"turnId":"\#(turn)","itemId":"live-tool-\#(index)","state":"completed","lifecycleAuthority":2,"itemType":"commandExecution","item":{"id":"live-tool-\#(index)","type":"commandExecution","status":"completed","command":"/bin/zsh -lc 'swift test --filter Part\#(index)'","exitCode":0}}"#),
                            turnId: turn, itemId: "live-tool-\(index)")] : [])
                } else {
                    // A host ends every item of the turn; the send-running fixture's earlier reply is still
                    // "streaming" and would keep the header on "Writing reply" after the last part.
                    func finished(_ id: String, _ text: String) -> EventBody {
                        EventBody(type: "activity", category: "thread_item_upsert", state: "updated",
                                  detail: #"{"turnId":"\#(turn)","itemId":"\#(id)","state":"completed","lifecycleAuthority":2,"itemType":"agentMessage","item":{"id":"\#(id)","type":"agentMessage","status":"completed","text":"\#(text)"}}"#)
                    }
                    let full = text.trimmingCharacters(in: .whitespaces)
                    frame = [event(1, EventBody(type: "assistant_completed", text: full)),
                             event(2, finished(item, full)),
                             event(3, finished("send-earlier-reply", "Drafting the release notes."))]
                }
                do { try self.applyFrame(frame, batched: true, run: run) } catch { return }
            }
        }
    }
    #endif

    #if WONDER_DIAGNOSTICS
    private var diagnosticBannerStarted = false
    /// `-diagnostics-in-app-banner`: while the fixture Project thread is open, the
    /// "Weekend plans" chat finishes a reply (2 s), the open chat itself finishes
    /// one (3 s, which must not raise a banner) and then "Release checklist"
    /// asks for permission (4.5 s). All arrive as live frames, as the event socket delivers them.
    private func startDiagnosticBanner(_ chat: ChatSummary) {
        guard DiagnosticSubagentFixture.inAppBannerFixture, chat.id == DiagnosticSubagentFixture.parentID, !diagnosticBannerStarted else { return }
        diagnosticBannerStarted = true
        let run = generation, other = DiagnosticSubagentFixture.bannerChatID
        // `-diagnostics-banner-wait`: the live-feed voice scenario starts voice first,
        // then `signal banner-start` runs the events and `signal voice-live-question`
        // makes the open chat ask a question.
        let held = ProcessInfo.processInfo.arguments.contains("-diagnostics-banner-wait")
        Task { [weak self] in
            while held && !DiagnosticSubagentFixture.signalled("banner-start") {
                do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
            }
            // A reply's events name its item, as the host's do: an open chat renders
            // a completed reply only by its turn and item.
            @MainActor func frame(_ bodies: [EventBody], on model: ConnectionModel, in conversation: String) -> [ReplayEvent] {
                let base = model.projection.lastSequence, now = ISO8601DateFormatter().string(from: Date())
                return bodies.enumerated().map { index, body in
                    ReplayEvent(eventId: "banner-\(base + UInt64(index) + 1)", hostEpoch: model.projection.hostEpoch, sequence: base + UInt64(index) + 1,
                                occurredAt: now, conversationId: conversation, event: body, turnId: DiagnosticSubagentFixture.bannerTurnID,
                                itemId: DiagnosticSubagentFixture.bannerReplyItemID, messageId: "fixture-banner-message")
                }
            }
            try? await Task.sleep(for: .seconds(2))
            guard let self, run == self.generation else { return }
            try? self.applyFrame(frame([EventBody(type: "assistant_completed", text: DiagnosticSubagentFixture.bannerReply),
                                        EventBody(type: "message_state", state: "completed")], on: self, in: other), batched: true, run: run)
            await self.loadAttention()
            try? await Task.sleep(for: .seconds(1))
            guard run == self.generation else { return }
            DiagnosticSubagentFixture.markBannerOpenChatReply()
            try? self.applyFrame(frame([EventBody(type: "assistant_completed", text: DiagnosticSubagentFixture.bannerOpenChatReply),
                                        EventBody(type: "message_state", state: "completed")], on: self, in: DiagnosticSubagentFixture.parentID), batched: true, run: run)
            try? await Task.sleep(for: .seconds(1.5))
            guard run == self.generation else { return }
            DiagnosticSubagentFixture.markBannerApproval()
            try? self.applyFrame(frame([EventBody(type: "approval_opened")], on: self, in: DiagnosticSubagentFixture.bannerApprovalChatID), batched: true, run: run)
            await self.loadAttention()
            guard held else { return }
            while !DiagnosticSubagentFixture.signalled("voice-live-question") {
                do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
            }
            guard run == self.generation else { return }
            try? self.applyFrame(frame([EventBody(type: "approval_opened")], on: self, in: DiagnosticSubagentFixture.parentID), batched: true, run: run)
            await self.loadAttention()
        }
    }

    private var diagnosticIdleTaskStarted = false
    private func startDiagnosticIdleTaskCompletion(_ chat: ChatSummary) {
        guard DiagnosticSubagentFixture.taskFinishesIdleFixture, !diagnosticIdleTaskStarted else { return }
        diagnosticIdleTaskStarted = true
        let run = generation
        Task { [weak self] in
            while !DiagnosticSubagentFixture.idleTaskFinished {
                do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
                guard let self, run == self.generation else { return }
            }
            guard let self, run == self.generation, !Task.isCancelled else { return }
            let category = ProcessInfo.processInfo.arguments.contains("-diagnostics-task-finish-status") ? "native_status" : "native_history"
            let event = ReplayEvent(eventId: "idle-task-completed", hostEpoch: self.projection.hostEpoch,
                sequence: self.projection.lastSequence + 1, occurredAt: ISO8601DateFormatter().string(from: Date()),
                conversationId: chat.id, event: EventBody(type: "activity", category: category, state: "changed"),
                turnId: nil, itemId: nil)
            try? self.applyFrame([event], batched: true, run: run)
        }
    }
    private var diagnosticMacWakeStarted = false
    private var diagnosticMacArchiveStarted = false
    /// `-diagnostics-mac-archive`: on `wonderctl signal mac-archive` a thread
    /// is archived on the computer, and the host's `native_status` event
    /// arrives through the frame path, well before the 30 s refresh.
    private func startDiagnosticMacArchive() {
        guard DiagnosticSubagentFixture.macArchiveFixture, !diagnosticMacArchiveStarted else { return }
        diagnosticMacArchiveStarted = true
        Task { [weak self] in
            while !DiagnosticSubagentFixture.signalled("mac-archive") { try? await Task.sleep(for: .milliseconds(100)) }
            guard let self else { return }
            DiagnosticSubagentFixture.markMacArchived()
            let status = ReplayEvent(eventId: "mac-archive", hostEpoch: self.projection.hostEpoch, sequence: self.projection.lastSequence + 1,
                                     occurredAt: ISO8601DateFormatter().string(from: Date()), conversationId: nil,
                                     event: EventBody(type: "activity", category: "native_status", state: "changed"), turnId: nil, itemId: nil)
            try? self.applyFrame([status], batched: true, run: self.generation)
        }
    }
    private var diagnosticMacWaitingStarted = false
    /// `-diagnostics-mac-waiting`: on `wonderctl signal mac-waiting` the
    /// fixture's Claude thread waits on the Mac, and the host's `native_status`
    /// event arrives through the frame path, well before the 30 s refresh.
    private func startDiagnosticMacWaiting() {
        guard DiagnosticSubagentFixture.macWaitingFixture, !diagnosticMacWaitingStarted else { return }
        diagnosticMacWaitingStarted = true
        Task { [weak self] in
            while !DiagnosticSubagentFixture.signalled("mac-waiting") { try? await Task.sleep(for: .milliseconds(100)) }
            guard let self else { return }
            DiagnosticSubagentFixture.markMacWaiting()
            let status = ReplayEvent(eventId: "mac-waiting", hostEpoch: self.projection.hostEpoch, sequence: self.projection.lastSequence + 1,
                                     occurredAt: ISO8601DateFormatter().string(from: Date()), conversationId: "mac-waiting-chat",
                                     event: EventBody(type: "activity", category: "native_status", state: "changed"), turnId: nil, itemId: nil)
            try? self.applyFrame([status], batched: true, run: self.generation)
            guard DiagnosticSubagentFixture.macPermissionFixture else { return }
            let run = self.generation
            var previous: Int?
            while run == self.generation && !Task.isCancelled {
                let count = DiagnosticSubagentFixture.signalled("mac-permission-closed") ? 0
                    : DiagnosticSubagentFixture.signalled("mac-dialog-one") ? 1
                    : DiagnosticSubagentFixture.signalled("mac-dialog-three") ? 3 : 2
                if count != previous {
                    let event = ReplayEvent(eventId: UUID().uuidString, hostEpoch: self.projection.hostEpoch,
                        sequence: self.projection.lastSequence + 1, occurredAt: ISO8601DateFormatter().string(from: Date()),
                        conversationId: nil, event: EventBody(type: "activity", category: "mac_dialogs", state: "changed",
                            detail: "{\"count\":\(count)}"), turnId: nil, itemId: nil)
                    try? self.applyFrame([event], batched: true, run: run)
                    previous = count
                }
                if count == 0 { break }
                try? await Task.sleep(for: .milliseconds(100))
            }
            guard run == self.generation, !Task.isCancelled else { return }
            let closed = ReplayEvent(eventId: "mac-permission-closed", hostEpoch: self.projection.hostEpoch, sequence: self.projection.lastSequence + 1,
                                     occurredAt: ISO8601DateFormatter().string(from: Date()), conversationId: "mac-waiting-chat",
                                     event: EventBody(type: "activity", category: "native_status", state: "changed"), turnId: nil, itemId: nil)
            try? self.applyFrame([closed], batched: true, run: self.generation)
        }
    }
    /// `-diagnostics-mac-wake`: 1.5 s after the fixture chat opens and reloads,
    /// another app on the Mac writes a reply (the fixture's history now has it)
    /// and the host's `native_history` wake-up arrives through the frame path.
    /// The chat's activity check runs every 5 s otherwise.
    private func startDiagnosticMacWake(_ chat: ChatSummary) {
        guard DiagnosticSubagentFixture.macWakeFixture, !diagnosticMacWakeStarted else { return }
        diagnosticMacWakeStarted = true
        let run = generation
        Task { [weak self] in
            // Once the chat is on screen and its opening history check (and any reload) is done.
            while let self, run == self.generation,
                  self.visibleChat?.id != chat.id || !self.diagnosticOpenHistoryChecked.contains(chat.id) || self.nativeHistoryRefreshing.contains(chat.id) {
                try? await Task.sleep(for: .milliseconds(100))
            }
            try? await Task.sleep(for: .seconds(1.5))
            guard let self, run == self.generation else { return }
            DiagnosticSubagentFixture.markMacWrote()
            let wake = ReplayEvent(eventId: "mac-wake", hostEpoch: self.projection.hostEpoch, sequence: self.projection.lastSequence + 1,
                                   occurredAt: ISO8601DateFormatter().string(from: Date()), conversationId: chat.id,
                                   event: EventBody(type: "activity", category: "native_history", state: "changed"), turnId: nil, itemId: nil)
            try? self.applyFrame([wake], batched: true, run: run)
        }
    }
    #endif

    // MARK: Working watchdog
    //
    // "Working" has sources a missed event can leave stuck: a history whose
    // newest turn stayed in progress (a dropped turn end, a chat nobody is
    // looking at whose events only marked it stale) and a Chats row read before
    // the thread ended. While any shows work, this one loop re-reads what has
    // been quiet for an interval and the rows that still show work, so every
    // surface converges on the Mac's answer within about two intervals.
    static let workWatchdogInterval: Duration = .seconds(10)
    /// Reads per tick: the watchdog is a low-priority backstop, not a refresh storm.
    static let workWatchdogReadsPerTick = 3
    private var workWatchdog: Task<Void, Never>?
    private var workWatchdogRun: UUID?
    /// When each working chat's history last heard from the Mac (an event or a read).
    private var workTouched: [String: ContinuousClock.Instant] = [:]
    private var hasWorkToWatch: Bool {
        if !workingSnapshots.isEmpty { return true }
        let rows = projects.workingRows
        return !rows.conversations.isEmpty || rows.unattached
    }
    func armWorkWatchdog() {
        guard workWatchdog == nil, foreground, !previewMode, connection != nil, !accessEnded, hasWorkToWatch else { return }
        let run = generation
        workWatchdogRun = run
        workWatchdog = Task { @MainActor [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.workWatchdogInterval)
                guard let self, !Task.isCancelled, run == self.generation else { break }
                guard self.reconcileWork(run: run) else { break }
            }
            if let self, self.workWatchdogRun == run { self.workWatchdog = nil; self.workWatchdogRun = nil }
        }
    }
    private func stopWorkWatchdog() {
        workWatchdog?.cancel(); workWatchdog = nil; workWatchdogRun = nil; workTouched = [:]
    }
    /// One tick. False when nothing shows work any more, which ends the loop.
    private func reconcileWork(run: UUID) -> Bool {
        let rows = projects.workingRows
        guard !workingSnapshots.isEmpty || !rows.conversations.isEmpty || rows.unattached else { return false }
        workTouched = workTouched.filter { workingSnapshots.contains($0.key) }
        for id in rows.conversations { scheduleListRefresh(id, run: run) }
        if rows.unattached { scheduleListRefresh(nil, run: run) }
        let now = ContinuousClock.now
        let quiet = workingSnapshots.filter { id in
            workTouched[id].map { now - $0 >= Self.workWatchdogInterval } ?? true
        }
        // Oldest silence first, so a busy tick still reaches every chat in turn.
        let never = ContinuousClock.Instant.now - .seconds(3600)
        let due = quiet.sorted { (workTouched[$0] ?? never) < (workTouched[$1] ?? never) }
            .prefix(Self.workWatchdogReadsPerTick)
        for id in due {
            workTouched[id] = now
            Task { @MainActor [weak self] in
                guard let self, run == self.generation, !Task.isCancelled, self.workingSnapshots.contains(id) else { return }
                await self.refreshConversation(self.attentionChat(id))
            }
        }
        return true
    }

    /// The status table's prompt sidebar reconciliation budget.
    static let sidebarStatusDelay: Duration = .milliseconds(300)

    /// Once per recovery, never while the stream is healthy: reread the chat on screen
    /// and the list, whatever the replay delivered.
    private func catchUpOpenChat(run: UUID) {
        guard let chat = visibleChat else { return }
        scheduleListRefresh(chat.id, run: run, urgent: true)
        rereadAfterRecovery(chat, run: run)
    }

    /// An import already in flight may have read before the connection returned, so
    /// remember the catch-up and let the import's cleanup run it instead of dropping it.
    private func rereadAfterRecovery(_ chat: ChatSummary, run: UUID) {
        if nativeHistoryRefreshing.contains(chat.id) { pendingCatchUp.insert(chat.id); return }
        pendingCatchUp.remove(chat.id)
        refreshCoordinator.request(.conversation(chat.id), cadence: .seconds(2), urgent: true) { [weak self] in
            guard let self, run == self.generation, !Task.isCancelled, self.visibleChat?.id == chat.id else { return }
            if self.nativeHistoryRefreshing.contains(chat.id) { self.pendingCatchUp.insert(chat.id); return }
            await self.refreshConversation(chat)
        }
    }

    private func scheduleConversationRefresh(_ chat: ChatSummary, run: UUID, urgent: Bool = false) {
        // Imports emit historyRefresh invalidations deliberately. Their owner
        // always does a canonical GET after import; no parallel GET of old data.
        guard !nativeHistoryRefreshing.contains(chat.id) else { return }
        refreshCoordinator.request(.conversation(chat.id), cadence: .seconds(2), urgent: urgent) { [weak self] in
            guard let self, run == self.generation, !Task.isCancelled,
                  self.visibleChat?.id == chat.id, !self.nativeHistoryRefreshing.contains(chat.id),
                  self.projection.dirty.contains(chat.id) || self.projection.snapshots[chat.id] == nil else { return }
            await self.refreshConversation(chat)
        }
    }

    /// A screen's wakes and fallback checks use the connection's import lane.
    /// Terminal activity may bypass progress spacing; every import keeps L8's GET.
    func requestNativeHistoryRefresh(_ chat: ChatSummary, urgent: Bool = false, activityChanged: Bool = false) {
        let run = generation
        if activityChanged {
            nativeActivityRefreshes.insert(chat.id)
            scheduleSubagentRefresh(chat.id, run: run, urgent: urgent)
            scheduleListRefresh(chat.id, run: run, urgent: urgent)
        }
        refreshCoordinator.request(.nativeHistory(chat.id), cadence: .milliseconds(2500), urgent: urgent) { [weak self] in
            guard let self, run == self.generation, !Task.isCancelled,
                  self.visibleChat?.id == chat.id, self.shouldPollNativeActivity(chat.id) else { return }
            await self.reloadNativeHistory(chat)
            if run == self.generation, self.nativeActivityRefreshes.remove(chat.id) != nil, self.visibleChat?.id == chat.id {
                await self.loadPullRequests(chat)
            }
        }
    }

    private func scheduleSubagentRefresh(_ conversation: String, run: UUID, urgent: Bool = false) {
        guard let parent = [selectedChat, visibleChat].compactMap({ $0 }).first(where: {
            $0.id == conversation && isProject($0)
        }) else { return }
        refreshCoordinator.request(.subagents(conversation), cadence: .seconds(2), urgent: urgent) { [weak self] in
            guard let self, run == self.generation, !Task.isCancelled,
                  self.selectedChat?.id == conversation || self.visibleChat?.id == conversation else { return }
            await self.loadProjectSubagents(parent)
        }
    }

    private func scheduleListRefresh(_ conversation: String?, run: UUID, urgent: Bool = false) {
        if let conversation { eventListConversations.insert(conversation) }
        else { eventListDiscovery = true }
        refreshCoordinator.request(.list, cadence: .seconds(2), urgent: urgent) { [weak self] in
            guard let self, run == self.generation, !Task.isCancelled else { return }
            let affected = self.eventListConversations, discover = self.eventListDiscovery
            self.eventListConversations.removeAll(); self.eventListDiscovery = false
            do {
                try await self.refreshCoordinator.read(.list) { [self] in
                    // Clear before reading: a wake during this read keeps one trailing pass.
                    var next = projection; next.listDirty = false
                    try commit(next, publishing: .list)
                    await projects.refreshEventRows(affected, discover: discover)
                    guard run == generation, !Task.isCancelled else { return }
                    if projects.failure != nil {
                        next = projection; next.listDirty = true
                        try commit(next, publishing: .list)
                    }
                }
            } catch {
                self.readFailed(error, run: run)
            }
        }
    }

    private func scheduleEventRefresh(_ event: ReplayEvent, run: UUID) {
        let body = event.event
        struct Change: Decodable { let resource: String? }
        let resource = body.detail?.data(using: .utf8).flatMap { try? JSONDecoder().decode(Change.self, from: $0).resource }
        let attention = body.type == "approval_opened" || body.type == "approval_resolved"
            || resource == "approvals" || resource == "async_questions"
        let terminal = body.type == "assistant_completed" || body.type == "terminal_error"
            || ["completed", "failed", "interrupted", "cancelled"].contains(body.state ?? "")
        let status = body.type == "activity" && body.category == "native_status"
        let conversationChanged = resource == "conversations" || body.category == "conversation"
        let unknown = event.conversationId?.isEmpty != false
        let prompt = attention || terminal || status || conversationChanged
        // Applied text has no list effect, while all invalidations, native
        // notices and snapshot-covered events still reconcile other caches.
        if projection.listDirty || prompt || body.category == "native_history" {
            scheduleListRefresh(event.conversationId, run: run, urgent: prompt)
        }
        if attention || terminal || unknown {
            refreshCoordinator.request(.attention, cadence: .milliseconds(300), urgent: attention) { [weak self] in
                guard let self, run == self.generation, !Task.isCancelled else { return }
                await self.loadAttention()
            }
        }
        if resource == "async_questions", let id = event.conversationId, !id.isEmpty {
            refreshCoordinator.request(.questions(id), cadence: .milliseconds(300), urgent: true) { [weak self] in
                guard let self, run == self.generation, !Task.isCancelled else { return }
                _ = await self.loadAsyncQuestions(self.attentionChat(id))
            }
        }
        let affected = [selectedChat, visibleChat].compactMap { $0 }.filter {
            unknown || $0.id == event.conversationId
        }
        for chat in affected {
            if chat.id == visibleChat?.id, projection.dirty.contains(chat.id) {
                scheduleConversationRefresh(chat, run: run, urgent: prompt)
            }
            if isProject(chat), status || conversationChanged || unknown {
                refreshCoordinator.request(.detail(chat.id), cadence: .seconds(2), urgent: prompt) { [weak self] in
                    guard let self, run == self.generation, !Task.isCancelled,
                          self.selectedChat?.id == chat.id || self.visibleChat?.id == chat.id else { return }
                    await self.projects.refreshDetail(chat.id)
                }
            }
            if isProject(chat), prompt || unknown || body.category == "native_history" {
                scheduleSubagentRefresh(chat.id, run: run, urgent: prompt)
            }
            if isProject(chat), terminal || unknown || resource == "messages" || body.category == "conversation" {
                refreshCoordinator.request(.queue(chat.id), cadence: .seconds(2), urgent: terminal) { [weak self] in
                    guard let self, run == self.generation, !Task.isCancelled,
                          self.visibleChat?.id == chat.id || self.selectedChat?.id == chat.id else { return }
                    try? await self.loadQueue(chat)
                }
            }
        }
    }

    /// `token` is the session the failed request carried, when known.
    private func readFailed(_ failure: Error, run: UUID, origin: String? = nil, token: String? = nil) {
        let kind = failureKind(failure)
        guard run == generation, !Task.isCancelled, kind != .cancelled else { return }
        if let token, token != connection?.credential.sessionToken { return }
        if let origin, origin != connection?.origin, failure is URLError || failure is PairingFailure || kind == .route { return }
        if SigningIdentityFailure.requiresPairing(failure) {
            try? requireIdentityRepair()
            stopForIdentityRecovery()
            return
        }
        if case PairingFailure.wrongHost = failure {
            refusedStreamSession = token ?? connection?.credential.sessionToken
            setIfChanged(\.macConnected, false)
            noteLoss(.keyChanged)
            return
        }
        if case PairingFailure.response(401) = failure {
            refusedStreamSession = token ?? connection?.credential.sessionToken
            recoverAccess(run: run, token: token)
            return
        }
        setIfChanged(\.cachedConversationIds, projection.knownConversationIDs)
        // An ended session, a resync or a refusal leaves the working route alone.
        if kind == .route, !confirmedCurrentStream {
            setIfChanged(\.macConnected, false)
            if foreground {
                let failedOrigin = origin ?? connection?.origin
                Task { [weak self] in _ = await self?.recheckRoutes(excluding: failedOrigin) }
            }
        }
        if (failure as NSError).domain == NSCocoaErrorDomain { storageFailure = .chatsNotSaved }
    }

    /// A request the computer refused for its session. Usually the session
    /// just ran out (the computer ends each after an hour), so the shared
    /// check renews it with the device key and confirms this device is still
    /// listed; only a refused renewal or a missing device ends access. Then
    /// reads run again; a pending send repeats with its own request ID. One
    /// recovery runs at a time and never re-enters `readFailed`'s recovery.
    private func recoverAccess(run: UUID, token: String?) {
        guard let saved = connection, !accessEnded, !isDemo, !previewMode else { return }
        let token = token ?? saved.credential.sessionToken
        // Sent with a session already replaced: the next request carries the new one.
        guard token == saved.credential.sessionToken, !recentlyRecovered(token) else { return }
        Task { [weak self] in
            guard let self, await self.recoveredConnection(after: token, run: run) != nil,
                  run == self.generation, !Task.isCancelled else { return }
            await self.loadChats(force: true)
        }
    }

    /// Recovers from a request the computer refused for `token`'s session,
    /// joining a recovery already running. Returns the connection on the
    /// renewed session to repeat the request with, or nil when access was not
    /// restored (the check then shows unreachable or access ended).
    private func recoveredConnection(after token: String, run: UUID) async -> SavedConnection? {
        guard run == generation, !accessEnded, !isDemo, !previewMode, let saved = connection, !saved.requiresPairing else { return nil }
        // Another caller renewed it already.
        if saved.credential.sessionToken != token { return saved }
        refusedStreamSession = token
        guard !recentlyRecovered(token) else { return nil }
        let task: Task<Void, Never>
        if let accessRecovery { task = accessRecovery.task } else {
            let id = UUID()
            task = Task { [weak self] in
                guard let self else { return }
                await self.check(renew: true)
                // It joined a check that did not renew: renew now.
                if run == self.generation, !Task.isCancelled, self.macConnected == true, !self.accessEnded,
                   self.connection?.credential.sessionToken == token {
                    await self.check(renew: true)
                }
                if let renewed = self.connection?.credential.sessionToken, renewed != token { self.recoveredSession = (renewed, .now) }
                if self.accessRecovery?.id == id { self.accessRecovery = nil }
            }
            accessRecovery = (id, task)
        }
        await task.value
        guard run == generation, !accessEnded, macConnected == true, let current = connection,
              current.credential.sessionToken != token,
              current.credential.hostInstallationId == saved.credential.hostInstallationId,
              current.credential.deviceId == saved.credential.deviceId else { return nil }
        return current
    }

    /// A read that is safe to repeat. When the computer refuses it for its
    /// session (usually the hour ran out), the session is renewed and the
    /// read runs once more on the renewed one. Returns the reply and the
    /// connection that read it. Throws `CancellationError` when access could
    /// not be restored: the connection check already shows why.
    private func readRenewingSession<T: Decodable & Sendable>(_ path: String, _ saved: SavedConnection, run: UUID) async throws -> (T, SavedConnection) {
        do {
            let value: T = try await api.request(path, origin: saved.origin, credential: saved.credential)
            return (value, saved)
        } catch PairingFailure.response(401) where !isDemo && !previewMode {
            guard let renewed = await recoveredConnection(after: saved.credential.sessionToken, run: run) else { throw CancellationError() }
            let value: T = try await api.request(path, origin: renewed.origin, credential: renewed.credential)
            return (value, renewed)
        }
    }

    /// Whether the session `saved` carries should be renewed now: it ends
    /// within the margin, and a renewal did not already return this end.
    private func renewalDue(_ saved: SavedConnection) -> Bool {
        guard let expires = saved.credential.expiresAtMs, expires != renewalCappedAtMs else { return false }
        return SessionRenewal.due(expiresAtMs: expires, nowMs: SessionRenewal.nowMs)
    }

    /// While the live connection runs (the app in front, or voice talking),
    /// renews the session before the computer ends it. A renewal that could
    /// not reach the computer tries again after `retry`.
    private func scheduleRenewal(after retry: Duration = .zero) {
        renewalTimer?.cancel(); renewalTimer = nil
        guard foreground, !isDemo, !previewMode, !accessEnded, let saved = connection, !saved.requiresPairing,
              saved.credential.expiresAtMs != renewalCappedAtMs,
              let due = SessionRenewal.delay(expiresAtMs: saved.credential.expiresAtMs, nowMs: SessionRenewal.nowMs) else { return }
        let run = generation
        let delay = max(due, retry)
        renewalTimer = Task { [weak self] in
            do { try await Task.sleep(for: delay) } catch { return }
            guard let self, run == self.generation, self.foreground, !self.accessEnded else { return }
            self.renewalTimer = nil
            // Due now: the check renews before it reads.
            await self.check()
            // A replaced session already scheduled its own renewal.
            guard !Task.isCancelled, run == self.generation, self.renewalTimer == nil else { return }
            self.scheduleRenewal(after: .seconds(30))
        }
    }

    /// Renews the session `saved` carries with the device key. Callers needing
    /// the same session renewed join one renewal. Returns the connection on the
    /// renewed session, or nil when the connection moved on meanwhile.
    private func renewSession(_ saved: SavedConnection, run: UUID) async throws -> SavedConnection? {
        if let latest = connection, latest.credential.hostInstallationId == saved.credential.hostInstallationId,
           latest.credential.deviceId == saved.credential.deviceId,
           latest.credential.sessionToken != saved.credential.sessionToken { return latest }
        let token = saved.credential.sessionToken
        if let renewal, renewal.token == token { return try await renewal.task.value }
        let task = Task { [weak self] () throws -> SavedConnection? in
            guard let self else { return nil }
            return try await self.performRenewal(saved, run: run)
        }
        renewal = (token, task)
        defer { if renewal?.token == token { renewal = nil } }
        return try await task.value
    }

    private func performRenewal(_ saved: SavedConnection, run: UUID) async throws -> SavedConnection? {
        #if WONDER_DIAGNOSTICS
        let renewalAPI = diagnosticRouteAPI ?? api
        #else
        let renewalAPI = api
        #endif
        /// The device key signs a fresh challenge over the signed-in connection.
        func renewOverRequests() async throws -> Credential? {
            struct Refresh: Encodable { let deviceId: String }
            let challenge: Challenge
            do {
                challenge = try await renewalAPI.request("/api/v1/pairing/session/refresh-challenge", origin: saved.origin, body: JSONEncoder().encode(Refresh(deviceId: saved.credential.deviceId)))
            } catch { throw EnrollmentPoll.renewalFailure(error) }
            guard run == generation else { return nil }
            try challenge.validate(origin: saved.origin, hostID: saved.credential.hostInstallationId, deviceID: saved.credential.deviceId)
            struct Signed: Encodable { let challengeId: String; let signature: String }
            let signature = try await signingIdentity.sign(challenge.transcript)
            guard run == generation else { return nil }
            return try await renewalAPI.request("/api/v1/pairing/session", origin: saved.origin, body: JSONEncoder().encode(Signed(challengeId: challenge.challengeId, signature: signature)))
        }
        let renewedCredential: Credential?
        if renewalAPI.isRelay(saved.origin), let pin = saved.hostKeyPin {
            // An ended session can't open the relay's channel, and the relay
            // serves nothing else: the device key signs in again inside it.
            do {
                renewedCredential = try await renewalAPI.renewSealed(origin: saved.origin, credential: saved.credential, pin: pin,
                                                                     sign: signingIdentity.sealedSign)
            } catch SealedRPCFailure.renewalUnsupported {
                // An older computer renews only on a connection its current
                // session opens; once that session ended, home or Tailscale must.
                do { renewedCredential = try await renewOverRequests() }
                catch PairingFailure.response(401) { throw RelayUnsupported.signIn }
            }
        } else {
            renewedCredential = try await renewOverRequests()
        }
        guard run == generation, let credential = renewedCredential else { return nil }
        guard credential.deviceId == saved.credential.deviceId, credential.hostInstallationId == saved.credential.hostInstallationId else { throw PairingFailure.wrongHost }
        guard let latest = connection,
              latest.credential.hostInstallationId == saved.credential.hostInstallationId,
              latest.credential.deviceId == saved.credential.deviceId else { return nil }
        let updated = SavedConnection(origin: latest.origin, credential: credential, hostName: latest.hostName,
            storageDeviceId: latest.storageDeviceId).preservingStorage(from: latest)
        let rotated = latest.credential.sessionToken != credential.sessionToken
        // A time-limited pairing ends at a fixed time: a renewal near it
        // returns that same end, and renewing again would repeat at once.
        renewalCappedAtMs = SessionRenewal.due(expiresAtMs: credential.expiresAtMs, nowMs: SessionRenewal.nowMs) ? credential.expiresAtMs : nil
        try saveConnection(updated); connection = updated
        if api.isRelay(updated.origin), let pin = updated.hostKeyPin {
            // On the relay, the next calls ride a connection signed in with
            // the new session. The live stream on the old one is a handoff:
            // the replay loop reopens it once this check ends.
            if socket is SealedEventStream { socket = nil }
            _ = try await api.sealedConnection(origin: updated.origin, credential: updated.credential, pin: pin,
                                                      sign: signingIdentity.sealedSign)
            guard run == generation else { return nil }
        } else if rotated, foreground, socket != nil {
            // The computer closes a live stream when the session it opened
            // with ends. Open and confirm one on the renewed session, then
            // retire the old one; its closing is a handoff, not a lost
            // connection. If the new one fails, the old one keeps working and
            // the replay loop reopens on the renewed session when it ends.
            try? await moveEventSocket(to: updated, run: run)
            guard run == generation else { return nil }
        }
        return connection
    }

    static func escape(_ value: String) -> String {
        value.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? ""
    }
    /// `verifyRoute`: confirm the route (and a pinned computer's key) even
    /// while the app still counts the computer as connected, as on a return
    /// from the background, without renewing a current session.
    func check(renew: Bool = false, verifyRoute: Bool = false, userInitiated: Bool = false) async {
        guard !previewMode, !retiredAfterPairing, let saved = connection else { return }
        // Asked for explicitly (Check connection, Try again, the app coming
        // to the front): do not wait out an earlier miss.
        if renew || userInitiated { reconnectNow() }
        if saved.requiresPairing {
            if store == nil { await prepare(saved) }
            stopForIdentityRecovery()
            return
        }
        if let connectionCheck {
            await connectionCheck.value
            return
        }
        guard !busy else { return }
        checkingConnection = true
        defer { checkingConnection = false }
        // Preparing the cache changes the read generation. Do it before a
        // renewal captures that generation, including on a cold launch.
        await prepare(saved)
        guard !Task.isCancelled, !busy, !retiredAfterPairing,
              preparation?.isCancelled != true,
              connection?.credential.hostInstallationId == saved.credential.hostInstallationId,
              connection?.credential.deviceId == saved.credential.deviceId else { return }
        // Loading saved chats suspends; join a check another caller started meanwhile.
        if let connectionCheck {
            await connectionCheck.value
            return
        }
        if userInitiated { busy = true }
        defer { if userInitiated { busy = false } }
        // Renewal follows the session's expiry, not whether the computer was
        // reachable: coming back from a brief overlay keeps a current session.
        let task = Task { await performConnectionCheck(renew: renew || accessEnded, verifyRoute: verifyRoute) }
        connectionCheck = task
        await withTaskCancellationHandler { await task.value } onCancel: { task.cancel() }
        connectionCheck = nil
    }

    private func performConnectionCheck(renew: Bool, verifyRoute: Bool = false, retryRoute: Bool = true) async {
        guard var saved = connection else { return }
        let run = generation
        let attempt = homeNetworkTrust.mark()
        startRouteMonitoring()
        do {
            if retryRoute, !isDemo, macConnected != true || renew || verifyRoute {
                guard await recheckRoutes() else { throw routeKeyChanged ? PairingFailure.wrongHost : URLError(.cannotConnectToHost) }
                guard run == generation, !Task.isCancelled, let selected = connection else { return }
                saved = selected
            }
            // Renew when asked, or when the session ends within the margin
            // (a cold launch or a resume after the hour ran out).
            var renewed = false
            if (renew || renewalDue(saved)) && !isDemo {
                guard try await renewSession(saved, run: run) != nil, run == generation else { return }
                renewed = true
            }
            struct Device: Decodable, Sendable { let id: String; let revokedAt: String? }
            var current = connection ?? saved
            let devices: [Device]
            do {
                devices = try await api.request("/api/v1/devices", origin: current.origin, credential: current.credential)
            } catch PairingFailure.response(401) where !renewed && !isDemo {
                // The session ended before it was renewed. Renew it once; only
                // a refused renewal or a refused renewed session ends access.
                guard isCurrentConnection(current, run: run) else { return }
                refusedStreamSession = current.credential.sessionToken
                guard let next = try await renewSession(current, run: run), run == generation else { return }
                current = next
                devices = try await api.request("/api/v1/devices", origin: current.origin, credential: current.credential)
            }
            guard isCurrentConnection(current, run: run) else { return }
            // The computer no longer lists this device: it was removed there.
            guard devices.contains(where: { $0.id == current.credential.deviceId && $0.revokedAt == nil }) else { throw PairingFailure.response(401) }
            guard connection?.credential.hostInstallationId == current.credential.hostInstallationId else { return }
            // Only an authenticated device check can retire a session refusal.
            refusedStreamSession = nil
            // Clear access first so reconnecting can schedule its pending writes.
            setIfChanged(\.accessEnded, false)
            // Every check lands here; unchanged answers must not republish.
            setIfChanged(\.macConnected, true)
            setIfChanged(\.status, connectedStatus); setIfChanged(\.error, nil); noteLoss(nil)
            struct Host: Decodable, Sendable {
                let hostInstallationId: String
                let hostName: String?
                let platform: String?
                let routes: [ConnectionRoute]?
            }
            if let host: Host = try? await api.request("/api/v1/host/status", origin: current.origin, credential: current.credential),
               run == generation, host.hostInstallationId == current.credential.hostInstallationId {
                if let routes = host.routes, current.hostKeyPin != nil, let latest = connection,
                   latest.credential.hostInstallationId == current.credential.hostInstallationId {
                    let accepted = routes.filter { (try? PairingLink.origin($0.origin)) == $0.origin }
                    let learned = try ConnectionRouteLearning.merge(accepted, into: latest, from: current.origin)
                    // Unchanged routes (every check) cost no Keychain write or republish.
                    if try JSONEncoder().encode(learned) != JSONEncoder().encode(latest) {
                        try saveConnection(learned); connection = learned
                    }
                }
                if hostPlatform != host.platform { hostPlatform = host.platform }
                if let name = host.hostName, connection?.credential.hostInstallationId == host.hostInstallationId {
                    try applyHostName(name)
                }
            }
        } catch PairingFailure.response(401) {
            // The computer refused renewal by the device key, or no longer
            // lists this device: its access really ended.
            guard run == generation, !Task.isCancelled else { return }
            refusedStreamSession = connection?.credential.sessionToken
            setIfChanged(\.error, nil); accessEnded = true; noteLoss(.accessEnded)
            if let host = connection?.credential.hostInstallationId { InAppBannerCenter.shared.forget(host: host) }
            stopReading(); setIfChanged(\.cachedConversationIds, projection.knownConversationIDs)
        } catch {
            guard run == generation, !Task.isCancelled, !(error is CancellationError) else { return }
            if SigningIdentityFailure.requiresPairing(error) {
                do { try requireIdentityRepair() }
                catch { self.error = error.localizedDescription }
                stopForIdentityRecovery()
                return
            }
            if error is URLError, saved.hostKeyPin != nil, homeNetworkTrust.provedAnotherKey(saved.origin, since: attempt) {
                // A key mismatch is security evidence even if an older socket lives.
                refusedStreamSession = connection?.credential.sessionToken
                setIfChanged(\.macConnected, false)
                noteLoss(.keyChanged)
                setIfChanged(\.error, nil)
                return
            }
            // A network miss on an auxiliary request cannot overrule a current
            // authenticated stream. Identity/key failures always keep their gate.
            if failureKind(error) == .route, confirmedCurrentStream { return }
            if case PairingFailure.wrongHost = error {
                refusedStreamSession = connection?.credential.sessionToken
            }
            setIfChanged(\.macConnected, false)
            // An unreachable computer says nothing about its key any more.
            if connectionLoss == .keyChanged { connectionLoss = nil }
            if let network = error as? URLError {
                // After a full route check that found no way in, this check is
                // answered by that miss rather than racing the same routes again.
                if retryRoute, failureKind(error) == .route, await recheckRoutes(excluding: saved.origin), connection?.origin != saved.origin {
                    guard run == generation, !Task.isCancelled else { return }
                    await performConnectionCheck(renew: renew, retryRoute: false)
                    return
                }
                switch network.code {
                case .notConnectedToInternet:
                    setIfChanged(\.status, "This device is offline. Connect to the internet and try again.")
                case .cannotFindHost, .dnsLookupFailed, .cannotConnectToHost, .timedOut:
                    setIfChanged(\.status, UnreachableAdvice.status(routes: saved.routes))
                    // Cupola may be stopped while the computer is on: its supervisor can say so.
                    probeRescue(saved)
                default:
                    setIfChanged(\.status, "The connection was interrupted. Try again.")
                }
            } else if case PairingFailure.wrongHost = error {
                // Never a reachability problem: retrying the same key fails the same way.
                noteLoss(.keyChanged)
                setIfChanged(\.error, nil)
                return
            } else if let failure = error as? PairingFailure {
                setIfChanged(\.status, failure.localizedDescription)
            } else if let relay = error as? RelayUnsupported {
                setIfChanged(\.status, relay.localizedDescription)
            } else {
                setIfChanged(\.status, "Could not connect to your computer.")
            }
            setIfChanged(\.error, error is SigningIdentityFailure ? error.localizedDescription : nil)
        }
    }
    /// Adds a thread's working folder, a linked worktree its Project leaves
    /// out, to that Project so Files and Terminal can open it. The primary
    /// folder and the other folders stay as they are.
    func addWorkingFolderToProject(_ conversationID: String) async throws {
        if previewMode {
            #if WONDER_DIAGNOSTICS
            try await Task.sleep(for: .milliseconds(300))
            previewWorktreeAdded = true
            #endif
            return
        }
        let detail = try await projects.loadDetail(conversationID)
        // Folders changed on another device since the last catalog read: read
        // them again once and add to the current set rather than overwrite it.
        for attempt in 0..<2 {
            if attempt > 0 || projects.project(detail.projectId) == nil { await projects.refresh() }
            try Task.checkCancellation()
            guard let project = projects.project(detail.projectId) else { throw PairingFailure.response(404) }
            var folders = project.folders.map(\.path)
            guard !folders.contains(detail.workingFolder) else { return }
            guard folders.count < 16 else {
                throw PairingFailure.hostMessage(422, "\(project.name) already has 16 folders. Remove one on your computer, then add this worktree.")
            }
            folders.append(detail.workingFolder)
            do {
                _ = try await projects.update(project.id, fields: ["folders": folders,
                    "primaryIndex": project.folders.firstIndex(where: \.isPrimary) ?? 0, "rootsRevision": project.rootsRevision])
                return
            } catch PairingFailure.response(409) where attempt == 0 {}
        }
    }

    /// Why adding a thread's folder to its Project failed, for Files and Terminal.
    static func addFolderFailure(_ error: Error) -> String {
        switch error {
        case PairingFailure.hostMessage(_, let message): message
        case PairingFailure.response(409): "The Project’s folders changed on another device. Try again."
        case PairingFailure.response(422): "Your computer couldn’t add this folder to the Project. Check it on your computer."
        case PairingFailure.response(404): "This Project was removed on your computer."
        default: "Cupola couldn’t reach your computer. Check the connection and try again."
        }
    }

    /// Asks the computer to disconnect this device before it is forgotten
    /// here. Best effort: an unreachable or refusing computer still lists the
    /// device until it is removed there with `cupola devices`.
    ///
    /// Tailscale and relay addresses use system TLS, which says nothing about
    /// Cupola's key, so the session goes only to a route that has just proved
    /// the paired key (`/host/proof`, over pinned TLS at home). A key change,
    /// an unpinned pairing or a failed proof forgets locally and sends nothing.
    @discardableResult func revokeThisDevice(_ saved: SavedConnection, timeout: Duration = .seconds(4)) async -> Bool {
        guard !isDemo, !previewMode, !saved.requiresPairing, !accessEnded,
              connectionLoss != .keyChanged, !routeKeyChanged, saved.hostKeyPin != nil,
              let route = saved.routes.first(where: { $0.origin == saved.origin }) else { return false }
        let api = api, signer = signingIdentity, trust = homeNetworkTrust
        let device = saved.credential.deviceId
        let path = "/api/v1/devices/\(device)/revoke"
        let attempt = Task { () -> Bool in
            try await ConnectionRouteProber.check(route, saved: saved, api: api, keyMismatches: trust, sign: signer.sealedSign)
            try Task.checkCancellation()
            let nonce = UUID().uuidString, issued = UInt64(Date().timeIntervalSince1970 * 1000)
            let transcript = try SignedAction.transcript(action: "device.revoke", path: path, values: [], nonce: nonce,
                                                         connection: saved, issuedAtMs: issued)
            let body = try SignedAction.body(nonce: nonce, issuedAtMs: issued, signature: try await signer.sign(transcript))
            struct Revoked: Decodable, Sendable {}
            let _: Revoked = try await api.request("/api/v1/devices/\(Self.escape(device))/revoke", origin: saved.origin,
                                                   body: body, credential: saved.credential)
            return true
        }
        let deadline = Task { try? await Task.sleep(for: timeout); attempt.cancel() }
        defer { deadline.cancel() }
        return (try? await attempt.value) ?? false
    }

    private func noteLoss(_ loss: ConnectionLoss?) {
        if connectionLoss != loss { connectionLoss = loss }
        if let loss { setIfChanged(\.status, loss.message(computer: macName)) }
    }

    private func saveConnection(_ saved: SavedConnection) throws {
        if let persistConnection { try persistConnection(saved) }
        else { try identity.save(JSONEncoder().encode(saved), account: "connection") }
    }

    func forget() async {
        guard !busy else { error = "Wait for the connection check to finish, then try again."; return }
        busy = true
        defer { busy = false }
        if let saved = connection { await revokeThisDevice(saved) }
        do {
            dictation.forget()
            endVoice()
            stopReading()
            preparation?.cancel()
            // Removal runs on the writer queue, after any write already started,
            // so a pending cache write cannot recreate the forgotten chats.
            // Queued reading positions are dropped first for the same reason.
            await intents?.discard(); intents = nil; savedPositions = [:]; unsavedPositions = [:]
            if let writer { try await writer.remove() }
            else if let target = store ?? connection.map(Self.readStore(for:)) { try await ProjectionWriter(store: target).remove() }
            if let saved = connection { ManagementDraftStore(host: saved.credential.hostInstallationId).removeAll() }
            projects.forgetCache()
            if let saved = connection { NewChatDraftStore.remove(host: saved.credential.hostInstallationId) }
            if let persistConnection { try persistConnection(nil) }
            else { try identity.forgetConnection() }
            partition = nil; store = nil; writer = nil; projection = ProjectionState(); publish(.everything)
            reloadOrphanedDrafts(); orphanedDraftError = nil
            forgetTypedDrafts()
            cancelOutboxDrain()
            stoppingProjectSubagents = [:]; projectSubagentStopErrors = [:]
            projectSubagentLoadTokens = [:]
            projectSubagentAvailability = [:]
            projectSubagentLookup = [:]
            projectSubagentFreshIDs = [:]
            projectSubagentNextCurrentCursor = [:]
            projectSubagentNextArchivedCursor = [:]
            projectSubagentInitialCurrentCursor = [:]
            projectSubagentInitialArchivedCursor = [:]
            projectSubagentSeenCurrentCursors = [:]
            projectSubagentSeenArchivedCursors = [:]
            projectSubagentExpandedParents = []
            projectSubagentPagingLimited = []
            projectSubagentRefreshPending = []
            projectSubagentPageRevision = [:]
            loadingOlderProjectSubagents = []
            projectSubagents = [:]; projectSubagentErrors = [:]; composers = [:]; composerErrors = [:]; sending = []; preparingSends = []; finishingDictation = []; intentLoadFailures = []; attention = []; asyncQuestions = [:]; retryableAsyncReplies = []; savedAsyncReplies = [:]; attentionErrors = [:]; resolving = []; savedDecisions = [:]; files = [:]; queues = [:]; queuedSends = [:]; awaitingAcceptance = [:]; awaitingSince = [:]; uploadTasks.values.forEach { $0.cancel() }; uploadTasks = [:]; uploading = []; uploadProgress = [:]; uploadFailures = [:]; stopping = []; controlErrors = [:]
            selectedChat = nil; macConnected = nil; hasConnectedThisLaunch = false; connection = nil; error = nil; accessEnded = false; connectionLoss = nil
            endRescue()
            status = "Connect to your computer to get started."
        }
        catch { self.error = error.localizedDescription }
    }
}

/// A conversation's grouped rows and the per-row lookups its view needs,
/// derived once from one projection revision.
/// One chat's typed text, owned by `ConnectionModel.composers` and observed
/// only by that chat's message editor.
@MainActor final class ComposerDraftText: ObservableObject {
    @Published fileprivate(set) var text: String
    fileprivate init(_ text: String) { self.text = text }
}

/// What views other than the message editor read from typed text: Send and
/// Stop, the prompt suggestion, Implement plan and the length warning. A
/// keystroke that changes none of these does not notify the model.
struct ComposerDraftFacts: Equatable {
    let isEmpty: Bool
    let isBlank: Bool
    let isTooLong: Bool
    init(_ text: String) {
        isEmpty = text.isEmpty
        // Stops at the first visible character; trimming would copy the draft.
        isBlank = !text.unicodeScalars.contains { !CharacterSet.whitespacesAndNewlines.contains($0) }
        isTooLong = text.utf8.count > 65536
    }
}

struct ConversationTimeline {
    struct Key: Equatable {
        let chatID: String
        let title: String
        let rows: UInt64
        let queued: Set<String>
        let pending: [String]
        let activeTurns: Set<String>
        let activeTurn: String?
        let focusedRowID: String?
    }
    /// The model's cache key this value was prepared for; nil when uncached.
    var key: Key?
    let chatID: String
    let rows: [ReadRow]
    let entries: [ChatFeedEntry]
    let minimap: ConversationMinimapProjection
    /// Turns that failed or were stopped, by turn ID: one row each.
    let outcomes: [String: TurnOutcome]
    /// Row and turn positions for placing settled questions.
    let feedIndex: ChatFeedIndex
    let latestActivityEntryIDs: Set<String>
    let latestActiveActivityEntryID: String?
    let disclosureEntries: [ActivityDisclosurePolicy.Entry]
    let retainedTurnIDs: Set<String>
    let attachmentIDs: [String]
    let turns: [String: ReadTurn]
    /// Each finished reply that edited files, keyed by the turn's last entry:
    /// the "Changed N files" row after it.
    let replyChanges: [String: ResponseEditedFiles]
    /// The latest finished reply that edited files, for Code changes' Last reply.
    let latestReplyEdits: ResponseEditedFiles?
    /// That reply is the conversation's last finished one: the Git pill's dot.
    let lastReplyChanged: Bool
    let promptSuggestion: PromptSuggestion?
    /// Provider names by reply row ID, set only on a thread whose replies came
    /// from more than one provider: the first reply of each provider's run.
    /// Replies without a recorded provider get none and are not guessed.
    let providerNotes: [String: String]

    /// The reply's edits for its files list, while the timeline still has them.
    func replyChanges(turnID: String) -> ResponseEditedFiles? {
        replyChanges.values.first { $0.turnID == turnID }
    }

    init(chatID: String, rows: [ReadRow], activeTurnIDs: Set<String>, activeTurnID: String?, focusedRowID: String?, turns: [ReadTurn]?) {
        var byID: [String: ReadTurn] = [:]
        for turn in turns ?? [] where byID[turn.id] == nil { byID[turn.id] = turn }
        self.chatID = chatID
        self.turns = byID
        // A finished turn shows only its reply; its earlier texts join its work.
        let rows = ReadRow.settlingFinishedTurns(rows, turns: byID, activeTurnIDs: activeTurnIDs)
        self.rows = rows
        entries = ChatFeedEntry.grouping(rows, activeTurnIDs: activeTurnIDs, focusedRowID: focusedRowID)
        minimap = ConversationMinimapProjection(entries: entries)
        outcomes = TurnOutcome.prepare(rows: rows, turns: byID, activeTurnIDs: activeTurnIDs)
        feedIndex = ChatFeedIndex(entries: entries)
        latestActivityEntryIDs = ChatFeedEntry.latestActivityEntryIDs(entries)
        latestActiveActivityEntryID = ChatFeedEntry.latestActivityEntryID(entries, turnID: activeTurnID)
        promptSuggestion = PromptSuggestion.latest(in: turns ?? [])
        replyChanges = ResponseEditedFiles.footers(entries: entries, activeTurnIDs: activeTurnIDs, turns: byID)
        (latestReplyEdits, lastReplyChanged) = ResponseEditedFiles.latest(footers: replyChanges, entries: entries, activeTurnIDs: activeTurnIDs)
        disclosureEntries = entries.compactMap { entry in
            guard entry.isActivity, let turnID = entry.rows.first?.turnId else { return nil }
            return ActivityDisclosurePolicy.Entry(conversationID: chatID, turnID: turnID, entryID: entry.id,
                lifecycle: ActivityDisclosurePolicy.lifecycle(for: byID[turnID]),
                autoOpenWhileActive: activeTurnIDs.contains(turnID))
        }
        retainedTurnIDs = turns.map { Set($0.map(\.id)) } ?? Set(disclosureEntries.map { $0.key.turnID })
        attachmentIDs = rows.flatMap(\.attachmentIds)
        providerNotes = Self.providerNotes(rows)
    }

    /// T3 Code records the provider in each reply's item ID (`t3Provider`).
    static func providerNotes(_ rows: [ReadRow]) -> [String: String] {
        let replies = rows.compactMap { row -> (id: String, provider: String)? in
            guard !row.isUser, row.item?.type == "agentMessage",
                  let provider = row.item?.payload?["t3Provider"]?.string, !provider.isEmpty else { return nil }
            return (row.id, provider)
        }
        guard Set(replies.map(\.provider)).count > 1 else { return [:] }
        var notes: [String: String] = [:]
        var previous: String?
        for reply in replies where reply.provider != previous {
            notes[reply.id] = ReadRow.providerName(reply.provider)
            previous = reply.provider
        }
        return notes
    }
}

#if WONDER_DIAGNOSTICS
/// A tiny generated black MP4 lets the real Files sheet exercise AVFoundation's
/// authenticated range path without a paired Mac or a model request.
private final class DiagnosticWorkspaceMediaProtocol: URLProtocol, @unchecked Sendable {
    static let video = Data(base64Encoded: "AAAAIGZ0eXBpc29tAAACAGlzb21pc28yYXZjMW1wNDEAAAMxbW9vdgAAAGxtdmhkAAAAAAAAAAAAAAAAAAAD6AAAB9AAAQAAAQAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgAAAlx0cmFrAAAAXHRraGQAAAADAAAAAAAAAAAAAAABAAAAAAAAB9AAAAAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAEAAAABAAAAAAAAkZWR0cwAAABxlbHN0AAAAAAAAAAEAAAfQAAAAAAABAAAAAAHUbWRpYQAAACBtZGhkAAAAAAAAAAAAAAAAAABAAAAAgABVxAAAAAAALWhkbHIAAAAAAAAAAHZpZGUAAAAAAAAAAAAAAABWaWRlb0hhbmRsZXIAAAABf21pbmYAAAAUdm1oZAAAAAEAAAAAAAAAAAAAACRkaW5mAAAAHGRyZWYAAAAAAAAAAQAAAAx1cmwgAAAAAQAAAT9zdGJsAAAAv3N0c2QAAAAAAAAAAQAAAK9hdmMxAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAAAAEAAQABIAAAASAAAAAAAAAABFExhdmM2My4xLjEwMSBsaWJ4MjY0AAAAAAAAAAAAAAAAGP//AAAANWF2Y0MBZAAK/+EAGGdkAAqs2UQmwEQAAAMABAAAAwAIPEiWWAEABmjr48siwP34+AAAAAAQcGFzcAAAAAEAAAABAAAAFGJ0cnQAAAAAAAALmAAAAAAAAAAYc3R0cwAAAAAAAAABAAAAAgAAQAAAAAAUc3RzcwAAAAAAAAABAAAAAQAAABxzdHNjAAAAAAAAAAEAAAABAAAAAgAAAAEAAAAcc3RzegAAAAAAAAAAAAAAAgAAAtcAAAAPAAAAFHN0Y28AAAAAAAAAAQAAA2EAAABhdWR0YQAAAFltZXRhAAAAAAAAACFoZGxyAAAAAAAAAABtZGlyYXBwbAAAAAAAAAAAAAAAACxpbHN0AAAAJKl0b28AAAAcZGF0YQAAAAEAAAAATGF2ZjYzLjEuMTAxAAAACGZyZWUAAALubWRhdAAAAq0GBf//qdxF6b3m2Ui3lizYINkj7u94MjY0IC0gY29yZSAxNjUgcjMyMjIgYjM1NjA1YSAtIEguMjY0L01QRUctNCBBVkMgY29kZWMgLSBDb3B5bGVmdCAyMDAzLTIwMjUgLSBodHRwOi8vd3d3LnZpZGVvbGFuLm9yZy94MjY0Lmh0bWwgLSBvcHRpb25zOiBjYWJhYz0xIHJlZj0zIGRlYmxvY2s9MTowOjAgYW5hbHlzZT0weDM6MHgxMTMgbWU9aGV4IHN1Ym1lPTcgcHN5PTEgcHN5X3JkPTEuMDA6MC4wMCBtaXhlZF9yZWY9MSBtZV9yYW5nZT0xNiBjaHJvbWFfbWU9MSB0cmVsbGlzPTEgOHg4ZGN0PTEgY3FtPTAgZGVhZHpvbmU9MjEsMTEgZmFzdF9wc2tpcD0xIGNocm9tYV9xcF9vZmZzZXQ9LTIgdGhyZWFkcz0yIGxvb2thaGVhZF90aHJlYWRzPTEgc2xpY2VkX3RocmVhZHM9MCBucj0wIGRlY2ltYXRlPTEgaW50ZXJsYWNlZD0wIGJsdXJheV9jb21wYXQ9MCBjb25zdHJhaW5lZF9pbnRyYT0wIGJmcmFtZXM9MyBiX3B5cmFtaWQ9MiBiX2FkYXB0PTEgYl9iaWFzPTAgZGlyZWN0PTEgd2VpZ2h0Yj0xIG9wZW5fZ29wPTAgd2VpZ2h0cD0yIGtleWludD0yNTAga2V5aW50X21pbj0xIHNjZW5lY3V0PTQwIGludHJhX3JlZnJlc2g9MCByY19sb29rYWhlYWQ9NDAgcmM9Y3JmIG1idHJlZT0xIGNyZj0yMy4wIHFjb21wPTAuNjAgcXBtaW49MCBxcG1heD02OSBxcHN0ZXA9NCBpcF9yYXRpbz0xLjQwIGFxPTE6MS4wMACAAAAAImWIhAAW//730z/MsuyaJLXjqPeinIvz0z8pUaJ8gb17Rs0AAAALQZohbEFf/talm0A=") ?? Data()
    private static let revision = String(repeating: "a", count: 64)

    override class func canInit(with request: URLRequest) -> Bool {
        request.url?.host == "workspace-media.invalid"
    }

    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        guard let url = request.url else { return }
        if ProcessInfo.processInfo.arguments.contains("-workspace-media-offline-preview") {
            reply(url: url, status: 503, bytes: Data(), headers: [:])
            return
        }
        guard request.value(forHTTPHeaderField: "Origin") == "https://workspace-media.invalid",
              request.value(forHTTPHeaderField: "Cookie")?.hasPrefix("__Host-wonder_session=") == true,
              let range = request.value(forHTTPHeaderField: "Range"), range.hasPrefix("bytes=") else {
            reply(url: url, status: 403, bytes: Data(), headers: [:])
            return
        }
        let parts = range.dropFirst("bytes=".count).split(separator: "-", omittingEmptySubsequences: false)
        guard parts.count == 2, let start = Int(parts[0]), let requestedEnd = Int(parts[1]),
              start >= 0, requestedEnd >= start, start < Self.video.count else {
            reply(url: url, status: 416, bytes: Data(), headers: [:])
            return
        }
        if let revision = request.value(forHTTPHeaderField: "X-Wonder-Revision"),
           revision != Self.revision {
            reply(url: url, status: 409, bytes: Data(), headers: [:])
            return
        }
        if request.value(forHTTPHeaderField: "X-Wonder-Revision") != nil,
           ProcessInfo.processInfo.arguments.contains("-workspace-media-stale-preview") {
            reply(url: url, status: 409, bytes: Data(), headers: [:])
            return
        }
        let end = min(requestedEnd, Self.video.count - 1)
        let bytes = Self.video.subdata(in: start..<(end + 1))
        reply(url: url, status: 206, bytes: bytes, headers: [
            "Content-Type": "video/mp4",
            "Content-Length": String(bytes.count),
            "Content-Range": "bytes \(start)-\(end)/\(Self.video.count)",
            "X-Wonder-Revision": Self.revision
        ])
    }

    override func stopLoading() {}

    private func reply(url: URL, status: Int, bytes: Data, headers: [String: String]) {
        guard let response = HTTPURLResponse(url: url, statusCode: status,
                                             httpVersion: "HTTP/1.1", headerFields: headers) else { return }
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        if !bytes.isEmpty { client?.urlProtocol(self, didLoad: bytes) }
        client?.urlProtocolDidFinishLoading(self)
    }
}
#endif

extension ConnectionModel {
    /// A terminal socket proves this phone as the events stream does: a fresh
    /// challenge from the Mac, signed by the device key.
    func signedTerminalChallenge() async throws -> (saved: SavedConnection, challengeID: String, signature: String) {
        guard let saved = connection, !accessEnded else { throw PairingFailure.response(401) }
        let challenge: Challenge = try await api.request("/api/v1/events/challenge", origin: saved.origin, credential: saved.credential)
        try challenge.validate(origin: saved.origin, hostID: saved.credential.hostInstallationId, deviceID: saved.credential.deviceId)
        let signature = try await signingIdentity.sign(challenge.transcript)
        guard connection?.credential.hostInstallationId == saved.credential.hostInstallationId, !accessEnded else { throw CancellationError() }
        return (saved, challenge.challengeId, signature)
    }

    /// How a terminal reaches the Mac: explicit Diagnostics terminal fixtures
    /// also work in the real shell, so continuity checks never start a live shell.
    func terminalTransport() -> any TerminalTransport {
        #if WONDER_DIAGNOSTICS
        if previewMode || DiagnosticTerminalTransport.enabled { return DiagnosticTerminalTransport.shared }
        #endif
        return LiveTerminalTransport(model: self)
    }
}

/// The app on the Mac that has a Claude conversation open, as the Mac names it.
struct MacHolder: Decodable, Equatable, Sendable {
    /// "Claude", "T3 Code", "Claude Code in Terminal".
    let app: String
    /// "desktop", "app" or "terminal".
    let kind: String
    let replying: Bool
    let background: Bool
    /// Background agents, commands or monitors still running there, when known.
    let backgroundTasks: Int?
    /// A message sent normally waits for it.
    let waits: Bool
    /// The Mac can stop it for Send now.
    let canTakeOver: Bool
    /// A thread open in T3 Code: "sends" (messages go into it there, without
    /// waiting or stopping it), "cantSend" (the waiting message has what T3
    /// Code's send can't carry, `t3CodeLimit`, so it waits), "connect"
    /// (connecting T3 Code in Settings would do that) or "notFound" (connected,
    /// but this thread wasn't found there). Absent for other apps and older Macs.
    var t3Code: String? = nil
    /// With "cantSend": "attachments", "skills" or "model".
    var t3CodeLimit: String? = nil
    /// With "connect" or "notFound", why (`T3CodeNote`): "signedOut",
    /// "expired", "renewing", "revoked", "unreadable", "finding", "unmatched",
    /// "unreachable" or "refused". Absent from older Macs.
    var t3CodeReason: String? = nil
    /// What Send now stops there, for its confirmation.
    var stops: String? {
        switch backgroundTasks ?? 0 {
        case 0: background ? "Background work running there will stop too." : nil
        case 1: "1 background task running there will stop too."
        case let count: "\(count) background tasks running there will stop too."
        }
    }
}

/// Why a sent message shows as still sending: what the agent has not done yet.
enum SendAcceptance: Sendable, Equatable {
    /// A Send now waits for the agent to take it into the running reply.
    case steering
    /// The Mac stops Claude's reply there before this message starts.
    case stoppingMacReply
    /// Send now: the Mac stops the app that holds the conversation, then sends.
    case takingOver
    var label: String {
        switch self {
        case .steering: "Sending…"
        case .stoppingMacReply: "Stopping the computer’s reply…"
        case .takingOver: "Taking over from your computer…"
        }
    }
}

#endif // os(iOS)

/// New chat's Start in T3 Code (`T3CodeLaunch`): the Mac's T3 Code
/// connection and the launch itself.
extension ConnectionModel {
    /// nil on a Mac without T3 Code support or when it can't be read: the
    /// choice stays hidden.
    func t3CodeStatus(refresh: Bool = false, catalog: Bool = false) async -> T3CodeStatus? {
        guard connection != nil, !accessEnded, !previewMode else { return nil }
        var query: [String] = []
        if refresh { query.append("refresh=true") }
        if catalog { query.append("catalog=true") }
        let suffix = query.isEmpty ? "" : "?" + query.joined(separator: "&")
        return try? await manage("/api/v1/t3-code" + suffix, decodingStatuses: [503])
    }

    /// Sent once; T3 Code has no retry key, so a failure is never repeated here.
    func launchInT3Code(projectID: String, request: T3CodeLaunchRequest) async throws -> T3CodeLaunched {
        try await manage("/api/v1/projects/\(Self.escape(projectID))/t3-code/threads", method: "POST", body: request.body())
    }
}


#if WONDER_DIAGNOSTICS
extension ConnectionModel {
    /// Counts only, so a hop line shows when the newest reply (an item after the user's message) reached the screen's data.
    fileprivate func latencyShape(_ id: String) -> String {
        let items = (projection.snapshots[id]?.thread.turns ?? []).flatMap(\.items)
        return "items=\(items.count) last=\(items.last?.type ?? "-")"
    }
}
/// Hop marks for the open and send paths (PERF-LATENCY): one unified-log line per hop,
/// subsystem com.swaymun.wonder, category "latency". Content-free (hop names and a chat prefix).
/// `scripts/perf-latency-hops` reads them back and prints the gaps.
enum LatencyHops {
    private static let logger = Logger(subsystem: "com.swaymun.wonder", category: "latency")
    @MainActor static var sendArmed: Set<String> = []
    static func mark(_ hop: String, _ chat: String = "", _ detail: String = "") {
        let uptime = ProcessInfo.processInfo.systemUptime * 1000
        logger.notice("hop=\(hop, privacy: .public) up=\(uptime, format: .fixed(precision: 1), privacy: .public) chat=\(String(chat.prefix(8)), privacy: .public) \(detail, privacy: .public)")
    }
}
#endif
