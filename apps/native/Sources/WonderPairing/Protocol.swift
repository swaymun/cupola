import Foundation
import CryptoKit

public extension Data {
    var base64URL: String { base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "") }
}

public enum PairingFailure: LocalizedError {
    case invalidLink, wrongHost, expired, response(Int), annotationRejected(Int, String), skillRejected(Int, String), hostMessage(Int, String), missingIdentity
    /// The thread works in a linked worktree its Project leaves out. Adding
    /// the folder to the Project fixes it; trying again does not.
    case worktreeNotInProject(String)
    /// A pairing code typed with a home-network address. Only the QR code and
    /// link carry the key that proves the computer there, so a code can't.
    case homeNetworkCode
    public var errorDescription: String? {
        switch self {
        case .invalidLink: "Use the complete address or pairing QR code shown on your computer."
        case .wrongHost: "This connection does not match your computer. Create a new pairing code on the intended computer."
        case .expired: "This pairing request expired. Run cupola pair on your computer for a new code."
        case .response(let code): code == 409 ? "Confirm this device on your computer." : code == 410 ? "This request expired or was rejected. Run cupola pair on your computer for a new code." : code == 401 || code == 404 ? "Access is no longer available. Check this device with cupola devices on your computer." : "Cupola could not connect. Check your computer and try again."
        case .annotationRejected(_, let detail), .skillRejected(_, let detail), .hostMessage(_, let detail), .worktreeNotInProject(let detail): detail
        case .missingIdentity: "This device’s saved identity is unavailable. Pair again from your computer."
        case .homeNetworkCode: "On the same Wi-Fi, scan the QR code or paste the pairing link; a code works with a Tailscale address."
        }
    }
}

/// Something the relay can't carry yet (sockets and streamed downloads have
/// no sealed form). It opens on the home network or over Tailscale; changing
/// route is the fix, so callers stop retrying and say so.
public enum RelayUnsupported: LocalizedError, Equatable, Sendable {
    case terminal, workspacePreview
    /// A computer that predates signing in again through the relay.
    case signIn
    public var errorDescription: String? {
        switch self {
        case .terminal: "Terminal can’t open through Cupola’s relay yet. It opens when you’re on the same network as your computer or connected with Tailscale."
        case .workspacePreview: "This preview can’t open through Cupola’s relay yet. It opens when you’re on the same network as your computer or connected with Tailscale."
        case .signIn: "Connect on the same network as your computer or with Tailscale to sign this device in again. The version of Cupola on your computer can’t do that through the relay."
        }
    }
}

public struct PairingLink: Sendable {
    public let origin: String
    public let offerID: String
    public let secret: String
    public let hostID: String
    /// Canonical base64url SHA-256 of the paired host’s DER SubjectPublicKeyInfo.
    public let hostKey: String?
    public static func origin(_ input: String) throws -> String {
        guard let url = URLComponents(string: input.trimmingCharacters(in: .whitespacesAndNewlines)),
              url.scheme == "https", let host = url.host, !host.isEmpty,
              url.user == nil, url.password == nil, url.query == nil,
              url.fragment == nil, url.path.isEmpty || url.path == "/" else { throw PairingFailure.invalidLink }
        return "https://" + host + (url.port.map { ":\($0)" } ?? "")
    }
    /// A typed computer address. A bare name or IP ("studio.tail1234.ts.net",
    /// "100.64.1.2:3778") gets https://; anything with another scheme, a
    /// sign-in, a path, a query or a fragment is still refused. The scheme
    /// alone proves nothing about the computer: trust stays with the route.
    public static func address(_ input: String) throws -> String {
        let trimmed = input.trimmingCharacters(in: .whitespacesAndNewlines)
        return try origin(trimmed.contains("://") ? trimmed : "https://" + trimmed)
    }
    public init(_ input: String) throws {
        guard var url = URLComponents(string: input.trimmingCharacters(in: .whitespacesAndNewlines)),
              url.path == "/pair", url.query == nil, let fragment = url.fragment,
              let fields = URLComponents(string: "?" + fragment)?.queryItems,
              Set(fields.map(\.name)).count == fields.count,
              Set(fields.map(\.name)) == ["secret", "offerId", "hostInstallationId"] ||
                Set(fields.map(\.name)) == ["secret", "offerId", "hostInstallationId", "hostKey"],
              let secret = fields.first(where: { $0.name == "secret" })?.value, !secret.isEmpty,
              let offer = fields.first(where: { $0.name == "offerId" })?.value, UUID(uuidString: offer) != nil,
              let host = fields.first(where: { $0.name == "hostInstallationId" })?.value, !host.isEmpty else { throw PairingFailure.invalidLink }
        if let field = fields.first(where: { $0.name == "hostKey" }) {
            guard let value = field.value, value.utf8.count == 43,
                  value.utf8.allSatisfy({ (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) || $0 == 45 || $0 == 95 }),
                  let decoded = Data(base64Encoded: value.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/") + "="),
                  decoded.count == 32, decoded.base64URL == value else { throw PairingFailure.invalidLink }
            hostKey = value
        } else { hostKey = nil }
        url.path = ""; url.fragment = nil
        origin = try Self.origin(url.string ?? "")
        offerID = offer; self.secret = secret; hostID = host
    }
}

public struct PublicKey: Codable, Sendable {
    public let kty: String
    public let crv: String
    public let x: String
    public let y: String
    public init(_ key: P256.Signing.PublicKey) {
        let bytes = key.x963Representation
        kty = "EC"; crv = "P-256"; x = bytes[1..<33].base64URL; y = bytes[33..<65].base64URL
    }
}

public struct Challenge: Codable, Sendable {
    public let challengeId: String
    public let deviceId: String
    public let nonce: String
    public let origin: String
    public let hostInstallationId: String
    public let offerId: String
    public let issuedAtMs: UInt64
    public let expiresAtMs: UInt64
    public var transcript: Data {
        Data(["wonder-session-v1", deviceId, challengeId, nonce, origin, hostInstallationId, String(issuedAtMs), String(expiresAtMs)].joined(separator: "\n").utf8)
    }
    public var verificationCode: String { String(SHA256.hash(data: transcript).map { String(format: "%02x", $0) }.joined().prefix(12)).uppercased() }
    public func validate(origin: String, hostID: String?, deviceID: String?, now: UInt64 = UInt64(Date().timeIntervalSince1970 * 1000)) throws {
        guard self.origin == origin, hostID == nil || hostInstallationId == hostID, deviceID == nil || deviceId == deviceID,
              !hostInstallationId.isEmpty, !deviceId.isEmpty, issuedAtMs < expiresAtMs else { throw PairingFailure.wrongHost }
        guard now < expiresAtMs, issuedAtMs <= now + 30_000 else { throw PairingFailure.expired }
    }
}
public struct Claim: Codable, Sendable { public let deviceId: String; public let challenge: Challenge }
public struct Credential: Codable, Sendable {
    public let sessionToken: String
    public let deviceId: String
    public let csrfToken: String
    public let hostInstallationId: String
    public let expiresAtMs: UInt64?
}
/// Connection details and route selection use this metadata; normal UI shows the host.
public struct ConnectionRoute: Codable, Equatable, Sendable {
    public enum Kind: String, Codable, Sendable { case home, tailscale, relay }
    public let kind: Kind
    public let origin: String
    public var lastOK: Date?
    public var lastRTTms: Int?
    public fileprivate(set) var learnedFromOrigins: Set<String>
    public var learned: Bool { !learnedFromOrigins.isEmpty }

    /// A route added by pairing or by the person is explicit, never learned.
    public init(kind: Kind, origin: String, lastOK: Date? = nil, lastRTTms: Int? = nil) {
        self.kind = kind; self.origin = origin
        self.lastOK = lastOK; self.lastRTTms = lastRTTms
        learnedFromOrigins = []
    }

    private enum CodingKeys: String, CodingKey { case kind, origin, learned, learnedFromOrigins, lastOK, lastRTTms }
    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        kind = try values.decode(Kind.self, forKey: .kind)
        origin = try values.decode(String.self, forKey: .origin)
        lastOK = try values.decodeIfPresent(Date.self, forKey: .lastOK)
        lastRTTms = try values.decodeIfPresent(Int.self, forKey: .lastRTTms)
        learnedFromOrigins = try values.decodeIfPresent(Set<String>.self, forKey: .learnedFromOrigins) ?? []
        if let learned = try values.decodeIfPresent(Bool.self, forKey: .learned), learned != self.learned {
            throw DecodingError.dataCorruptedError(forKey: .learnedFromOrigins, in: values, debugDescription: "A learned route must retain its source addresses.")
        }
    }
    public func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        try values.encode(kind, forKey: .kind)
        try values.encode(origin, forKey: .origin)
        try values.encode(learned, forKey: .learned)
        try values.encode(learnedFromOrigins, forKey: .learnedFromOrigins)
        try values.encodeIfPresent(lastOK, forKey: .lastOK)
        try values.encodeIfPresent(lastRTTms, forKey: .lastRTTms)
    }
}

public struct SavedConnection: Codable, Sendable {
    /// The selected route, retained for existing request and signature callers.
    public private(set) var origin: String
    public private(set) var routes: [ConnectionRoute]
    public let hostKeyPin: String?
    public private(set) var credential: Credential
    public let hostName: String?
    public var requiresPairing: Bool
    public let storageDeviceId: String
    // Origin-only callers replace credentials/names, not the saved route snapshot.
    private var hasRouteSnapshot: Bool
    public init(origin: String, credential: Credential, hostName: String? = nil, requiresPairing: Bool = false, storageDeviceId: String? = nil, hostKeyPin: String? = nil) {
        self.origin = origin; self.credential = credential; self.hostName = hostName
        self.requiresPairing = requiresPairing
        self.storageDeviceId = storageDeviceId ?? credential.deviceId
        routes = [ConnectionRoute(kind: .tailscale, origin: origin)]
        self.hostKeyPin = hostKeyPin
        hasRouteSnapshot = false
    }
    /// A complete snapshot, in caller-supplied order. Selection never reorders it.
    public init(routes: [ConnectionRoute], origin: String? = nil, credential: Credential, hostName: String? = nil, requiresPairing: Bool = false, storageDeviceId: String? = nil, hostKeyPin: String? = nil) throws {
        guard let selected = origin ?? routes.first?.origin,
              routes.contains(where: { $0.origin == selected }),
              Set(routes.map(\.origin)).count == routes.count else { throw PairingFailure.invalidLink }
        self.init(origin: selected, credential: credential, hostName: hostName, requiresPairing: requiresPairing,
                  storageDeviceId: storageDeviceId, hostKeyPin: hostKeyPin)
        self.routes = routes
        hasRouteSnapshot = true
    }
    private enum CodingKeys: String, CodingKey { case origin, routes, hostKeyPin, credential, hostName, requiresPairing, storageDeviceId }
    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        let storedOrigin = try values.decodeIfPresent(String.self, forKey: .origin)
        if let storedRoutes = try values.decodeIfPresent([ConnectionRoute].self, forKey: .routes) {
            guard let selected = storedOrigin ?? storedRoutes.first?.origin,
                  storedRoutes.contains(where: { $0.origin == selected }),
                  Set(storedRoutes.map(\.origin)).count == storedRoutes.count else {
                throw DecodingError.dataCorruptedError(forKey: .routes, in: values, debugDescription: "Saved routes must be unique and contain the selected address.")
            }
            origin = selected; routes = storedRoutes; hasRouteSnapshot = true
        } else {
            origin = try values.decode(String.self, forKey: .origin)
            routes = [ConnectionRoute(kind: .tailscale, origin: origin)]
            hasRouteSnapshot = false
        }
        hostKeyPin = try values.decodeIfPresent(String.self, forKey: .hostKeyPin)
        credential = try values.decode(Credential.self, forKey: .credential)
        hostName = try values.decodeIfPresent(String.self, forKey: .hostName)
        requiresPairing = try values.decodeIfPresent(Bool.self, forKey: .requiresPairing) ?? false
        storageDeviceId = try values.decodeIfPresent(String.self, forKey: .storageDeviceId) ?? credential.deviceId
    }
    public func preservingStorage(from previous: SavedConnection?) -> SavedConnection {
        guard let previous, previous.credential.hostInstallationId == credential.hostInstallationId else { return self }
        var result = SavedConnection(origin: origin, credential: credential, hostName: hostName ?? previous.hostName,
                                     requiresPairing: requiresPairing, storageDeviceId: previous.storageDeviceId,
                                     hostKeyPin: hostKeyPin ?? previous.hostKeyPin)
        result.routes = hasRouteSnapshot ? routes : previous.routes
        if !result.routes.contains(where: { $0.origin == origin }) { result.routes.append(contentsOf: routes) }
        // A re-pair on another Wi-Fi must not forget the home address: the same computer
        // is usually back on it later. The key pin proves whoever answers there.
        for home in previous.routes where home.kind == .home && !home.learned
            && !result.routes.contains(where: { $0.origin == home.origin }) {
            result.routes.append(home)
        }
        result.hasRouteSnapshot = true
        return result
    }

    /// Adds an explicit route (or promotes a learned one), preserving its place and measurements.
    public mutating func addRoute(_ route: ConnectionRoute) {
        var explicit = route
        explicit.learnedFromOrigins = []
        if let index = routes.firstIndex(where: { $0.origin == route.origin }) {
            explicit.lastOK = route.lastOK ?? routes[index].lastOK
            explicit.lastRTTms = route.lastRTTms ?? routes[index].lastRTTms
            routes[index] = explicit
        } else { routes.append(explicit) }
        hasRouteSnapshot = true
    }

    /// Replaces the addresses advertised through one known, successfully connected route.
    /// Call only after verifying the response belongs to this paired host.
    public mutating func mergeLearnedRoutes(_ advertised: [ConnectionRoute], from sourceOrigin: String) {
        guard routes.contains(where: { $0.origin == sourceOrigin }) else { return }
        let reported = Set(advertised.map(\.origin))
        let previouslyLearned = Set(routes.filter(\.learned).map(\.origin))
        let explicitOrigins = Set(routes.filter { !$0.learned }.map(\.origin))
        for index in routes.indices where routes[index].learned && !reported.contains(routes[index].origin) {
            routes[index].learnedFromOrigins.remove(sourceOrigin)
        }
        for route in advertised where route.origin != sourceOrigin {
            if let index = routes.firstIndex(where: { $0.origin == route.origin }) {
                // An explicit pairing route can never become learned.
                if previouslyLearned.contains(route.origin) { routes[index].learnedFromOrigins.insert(sourceOrigin) }
            } else {
                var learned = ConnectionRoute(kind: route.kind, origin: route.origin)
                learned.learnedFromOrigins = [sourceOrigin]
                routes.append(learned)
            }
        }
        pruneLearnedRoutes(explicitOrigins: explicitOrigins)
        hasRouteSnapshot = true
    }

    /// Learned routes follow their source. Removing the last explicit route forgets the host.
    public func removingRoute(origin removedOrigin: String) -> SavedConnection? {
        guard let removed = routes.first(where: { $0.origin == removedOrigin }), !removed.learned else { return self }
        var result = self
        let explicitOrigins = Set(routes.filter { !$0.learned && $0.origin != removedOrigin }.map(\.origin))
        result.routes.removeAll { $0.origin == removedOrigin }
        result.pruneLearnedRoutes(explicitOrigins: explicitOrigins)
        guard !result.routes.isEmpty else { return nil }
        result.hasRouteSnapshot = true
        return result
    }

    /// The same connection on a renewed session (routes and pin unchanged).
    public func renewed(credential renewed: Credential) -> SavedConnection {
        var result = self; result.credential = renewed
        return result
    }

    public func usingRoute(origin selectedOrigin: String) throws -> SavedConnection {
        guard routes.contains(where: { $0.origin == selectedOrigin }) else { throw PairingFailure.invalidLink }
        var result = self; result.origin = selectedOrigin
        result.hasRouteSnapshot = true
        return result
    }

    private mutating func pruneLearnedRoutes(explicitOrigins: Set<String>) {
        // Reachability from explicit routes also eliminates orphaned learned cycles.
        var reachable = explicitOrigins
        while true {
            let next = reachable.union(routes.filter { !$0.learnedFromOrigins.isDisjoint(with: reachable) }.map(\.origin))
            if next == reachable { break }
            reachable = next
        }
        routes.removeAll { !reachable.contains($0.origin) }
        for index in routes.indices { routes[index].learnedFromOrigins.formIntersection(reachable) }
        if !routes.contains(where: { $0.origin == origin }), let first = routes.first { origin = first.origin }
    }
}

// Pairing secrets and credentials must never follow a redirect to another host.
final class NoRedirect: NSObject, URLSessionTaskDelegate, Sendable {
    private let serverTrustDelegate: (any URLSessionDelegate)?
    init(serverTrustDelegate: (any URLSessionDelegate)? = nil) { self.serverTrustDelegate = serverTrustDelegate }
    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping @Sendable (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
              let handler = serverTrustDelegate?.urlSession(_:didReceive:completionHandler:) else {
            completionHandler(.performDefaultHandling, nil); return
        }
        handler(session, challenge, completionHandler)
    }
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping @Sendable (URLRequest?) -> Void) { completionHandler(nil) }
}
public struct WorkspaceMediaChunk: Sendable {
    public let bytes: Data
    public let start: UInt64
    public let end: UInt64
    public let total: UInt64
    public let mimeType: String
    /// A file-change token from the opened Mac descriptor, not a content hash.
    public let revision: String
}
/// Relay addresses and their open sealed connections. A relay address serves
/// only the sealed channel, so nothing is sent to one in plaintext.
///
/// Each address is one `Route` whose transitions all happen here, under one lock.
/// What a late result may change is decided by identity: a connection removes only
/// itself (`remove(_:for:)`), and an opening installs or signs out only while it is
/// still the route's current opening.
final class SealedRoutes: @unchecked Sendable {
    /// The connection serving an address. `open` and `signedOut` exclude each other.
    enum Live {
        case none
        case open(SealedRPC)
        /// The computer refused this device's session; until a connection signs in
        /// again requests answer 401, as at home.
        case signedOut
    }
    struct Opening {
        let id: UUID
        let token: String
        let task: Task<SealedRPC, Error>
        var waiters: Int
    }
    struct Route {
        var live: Live = .none
        var opening: Opening?
    }
    enum Install {
        /// The connection now serves the address; `replaced` is what it took off.
        case installed(replaced: SealedRPC?)
        /// A newer opening or renewal took the address first; the caller closes its connection.
        case superseded
    }

    private let lock = NSLock()
    private var relayOrigins: Set<String> = []
    private var routes: [String: Route] = [:]

    /// `r-<26 base32>.<zone>`, as the relay service names every address.
    static func looksLikeRelay(_ origin: String) -> Bool {
        guard let host = URL(string: origin)?.host(), host.hasPrefix("r-") else { return false }
        let label = host.dropFirst(2).prefix { $0 != "." }
        return label.count == 26 && label.allSatisfy { ("a"..."z").contains($0) || ("2"..."7").contains($0) }
            && host.count > label.count + 3
    }
    func mark(_ origins: Set<String>) { lock.withLock { relayOrigins = origins } }
    func isRelay(_ origin: String) -> Bool { lock.withLock { relayOrigins.contains(origin) } || Self.looksLikeRelay(origin) }
    func client(_ origin: String) -> SealedRPC? {
        lock.withLock { if case .open(let client) = routes[origin]?.live { client } else { nil } }
    }
    /// Keeps a route entry only while it holds something.
    private func settle(_ origin: String) {
        if let route = routes[origin], case .none = route.live, route.opening == nil { routes[origin] = nil }
    }
    /// Makes `client` the connection for `origin`. An opening passes its `id` and installs
    /// only while it is the route's current opening; a renewal passes none, takes the
    /// address and supersedes any opening in flight.
    func install(_ client: SealedRPC, for origin: String, opening id: UUID? = nil) -> Install {
        lock.withLock {
            var route = routes[origin] ?? Route()
            if let id, route.opening?.id != id { return .superseded }
            route.opening = nil
            var replaced: SealedRPC?
            if case .open(let old) = route.live, old !== client { replaced = old }
            route.live = .open(client)
            routes[origin] = route
            return .installed(replaced: replaced)
        }
    }
    /// Drops `client` from `origin` if it is still the connection there: a late failure
    /// of a replaced connection must not unregister the one that replaced it.
    @discardableResult
    func remove(_ client: SealedRPC, for origin: String) -> Bool {
        lock.withLock {
            guard case .open(let current) = routes[origin]?.live, current === client else { return false }
            routes[origin]?.live = .none
            settle(origin)
            return true
        }
    }
    /// Clears `origin` (the connection and any refusal), returning the connection to close.
    func detach(_ origin: String) -> SealedRPC? {
        lock.withLock {
            guard routes[origin] != nil else { return nil }
            var current: SealedRPC?
            if case .open(let client) = routes[origin]?.live { current = client }
            routes[origin]?.live = .none
            settle(origin)
            return current
        }
    }
    /// The connection being opened for `origin` with this session, so callers that ask
    /// while it opens join it instead of each opening one (the later install would close
    /// the earlier caller's connection under it). Each joiner is a waiter: the opening
    /// ends only when every waiter has given up (`abandonConnect`). An opening with
    /// another session supersedes the one in flight.
    func joinConnect(origin: String, token: String, start: (UUID) -> Task<SealedRPC, Error>) -> (id: UUID, task: Task<SealedRPC, Error>) {
        lock.withLock {
            var route = routes[origin] ?? Route()
            if var pending = route.opening, pending.token == token, !pending.task.isCancelled {
                pending.waiters += 1
                route.opening = pending
                routes[origin] = route
                return (pending.id, pending.task)
            }
            let id = UUID()
            let task = start(id)
            route.opening = Opening(id: id, token: token, task: task, waiters: 1)
            routes[origin] = route
            return (id, task)
        }
    }
    /// A waiter on the opening `id` was cancelled; the last one to leave cancels the
    /// opening, which closes its socket (a relay that never welcomes would otherwise
    /// hold the route check or sign-out past its deadline).
    func abandonConnect(origin: String, id: UUID) {
        let cancelled: Task<SealedRPC, Error>? = lock.withLock {
            guard var pending = routes[origin]?.opening, pending.id == id else { return nil }
            pending.waiters -= 1
            if pending.waiters > 0 { routes[origin]?.opening = pending; return nil }
            routes[origin]?.opening = nil
            settle(origin)
            return pending.task
        }
        cancelled?.cancel()
    }
    func finishConnect(origin: String, id: UUID) {
        lock.withLock {
            guard routes[origin]?.opening?.id == id else { return }
            routes[origin]?.opening = nil
            settle(origin)
        }
    }
    /// Notes a refused session from opening `id`, taking off the connection it replaces.
    /// `nil` when the opening is no longer the current one: its refusal says nothing
    /// about a newer session. Otherwise the connection taken off, if any, to close.
    func signOut(_ origin: String, opening id: UUID) -> (applied: Bool, replaced: SealedRPC?) {
        lock.withLock {
            guard var route = routes[origin], route.opening?.id == id else { return (false, nil) }
            var replaced: SealedRPC?
            if case .open(let old) = route.live { replaced = old }
            route.live = .signedOut
            routes[origin] = route
            return (true, replaced)
        }
    }
    func isSignedOut(_ origin: String) -> Bool {
        lock.withLock { if case .signedOut = routes[origin]?.live { true } else { false } }
    }
    /// Takes every open connection off, and forgets refusals; openings in flight stay.
    func removeAll() -> [SealedRPC] {
        lock.withLock {
            var taken: [SealedRPC] = []
            for origin in Array(routes.keys) {
                if case .open(let client) = routes[origin]?.live { taken.append(client) }
                routes[origin]?.live = .none
                settle(origin)
            }
            return taken
        }
    }
}

/// Hands one waiting caller either the shared opening's result or its own
/// cancellation, whichever comes first, so a cancelled caller never waits on the
/// opening's other callers.
private final class CallerGate<Value: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Value, Error>?
    private var outcome: Result<Value, Error>?
    private var resumed = false

    func install(_ continuation: CheckedContinuation<Value, Error>) {
        let ready: Result<Value, Error>? = lock.withLock {
            if let outcome { resumed = true; return outcome }
            self.continuation = continuation
            return nil
        }
        if let ready { continuation.resume(with: ready) }
    }
    func finish(_ result: Result<Value, Error>) { settle(result) }
    func cancel() { settle(.failure(CancellationError())) }

    private func settle(_ result: Result<Value, Error>) {
        let waiting: CheckedContinuation<Value, Error>? = lock.withLock {
            guard !resumed, outcome == nil else { return nil }
            guard let waiting = continuation else { outcome = result; return nil }
            continuation = nil
            resumed = true
            return waiting
        }
        waiting?.resume(with: result)
    }
}

public final class PairingAPI: Sendable {
    private let session: URLSession
    let sealed = SealedRoutes()
    private let timing: (@Sendable (Double, Double, Int, Bool) -> Void)?
    /// Opens the socket under a sealed connection; a URLSession WebSocket unless a stand-in computer is given.
    private let sealedSocket: (@Sendable (URLRequest) -> any SealedSocket)?
    public convenience init(serverTrustDelegate: (any URLSessionDelegate)? = nil, timing: (@Sendable (Double, Double, Int, Bool) -> Void)? = nil) { self.init(configuration: .ephemeral, serverTrustDelegate: serverTrustDelegate, timing: timing) }
    public init(configuration: URLSessionConfiguration, serverTrustDelegate: (any URLSessionDelegate)? = nil, timing: (@Sendable (Double, Double, Int, Bool) -> Void)? = nil,
                sealedSocket: (@Sendable (URLRequest) -> any SealedSocket)? = nil) {
        self.timing = timing
        self.sealedSocket = sealedSocket
        session = URLSession(configuration: configuration, delegate: NoRedirect(serverTrustDelegate: serverTrustDelegate), delegateQueue: nil)
    }
    deinit {
        session.invalidateAndCancel()
        for client in sealed.removeAll() { Task { await client.close() } }
    }

    // MARK: Relay

    /// The relay addresses of the saved computer. Requests to them go only
    /// through a sealed connection.
    public func markRelay(_ origins: Set<String>) { sealed.mark(origins) }
    public func isRelay(_ origin: String) -> Bool { sealed.isRelay(origin) }

    /// The open sealed connection to `origin` signed in with this session, or a
    /// new one signed in as this device (a renewed session replaces the old
    /// connection). Throws `SealedChannelFailure.hostProof` when the computer's
    /// key is not `pin`, and `PairingFailure.response(401)` when it proved its
    /// key but refused the session (usually ended): `renewSealed` signs in again.
    public func sealedConnection(origin: String, credential: Credential, pin: String,
                                 sign: @escaping SealedRPC.Sign) async throws -> SealedRPC {
        if let open = sealed.client(origin), await open.isOpen, await open.sessionToken == credential.sessionToken { return open }
        let (id, task) = sealed.joinConnect(origin: origin, token: credential.sessionToken) { id in
            Task {
                defer { self.sealed.finishConnect(origin: origin, id: id) }
                return try await self.openSealedConnection(origin: origin, opening: id, credential: credential, pin: pin, sign: sign)
            }
        }
        // The opening is unstructured so callers can share it. A cancelled caller
        // leaves at once (`task.value` would hold it until the shared opening ends,
        // which a relay that never welcomes never does); the last one to leave
        // cancels the opening itself.
        let gate = CallerGate<SealedRPC>()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<SealedRPC, Error>) in
                gate.install(continuation)
                Task { gate.finish(await task.result) }
            }
        } onCancel: {
            gate.cancel()
            sealed.abandonConnect(origin: origin, id: id)
        }
    }

    private func openSealedConnection(origin: String, opening: UUID, credential: Credential, pin: String,
                                      sign: @escaping SealedRPC.Sign) async throws -> SealedRPC {
        let client = try sealedClient(origin)
        do { try await client.connect(pin: pin, credential: credential, sign: sign) }
        catch SealedRPCFailure.signedOut {
            let (_, replaced) = sealed.signOut(origin, opening: opening)
            if let replaced { await replaced.close() }
            throw PairingFailure.response(401)
        }
        switch sealed.install(client, for: origin, opening: opening) {
        case .installed(let replaced):
            if let replaced { await replaced.close() }
            return client
        case .superseded:
            await client.close()
            throw URLError(.networkConnectionLost)
        }
    }

    /// Signs this device in again through the relay with its device key (see
    /// `SealedRPC.renew`) and keeps the new connection for `origin`. Returns
    /// the new session for the caller to save. Throws
    /// `PairingFailure.response(401)` when the computer refuses the device, and
    /// `SealedRPCFailure.renewalUnsupported` when it predates this renewal.
    public func renewSealed(origin: String, credential: Credential, pin: String,
                            sign: @escaping SealedRPC.Sign) async throws -> Credential {
        let client = try sealedClient(origin)
        let renewed: Credential
        do { renewed = try await client.renew(pin: pin, credential: credential, sign: sign) }
        catch SealedRPCFailure.signedOut { throw PairingFailure.response(401) }
        catch SealedRPCFailure.refused { throw URLError(.badServerResponse) }
        catch SealedRPCFailure.closed { throw URLError(.networkConnectionLost) }
        if case .installed(let replaced) = sealed.install(client, for: origin), let replaced { await replaced.close() }
        return renewed
    }

    /// A sealed connection to `origin`'s `/api/v1/rpc/sealed`, not yet opened.
    private func sealedClient(_ origin: String) throws -> SealedRPC {
        guard var components = URLComponents(string: try PairingLink.origin(origin)) else { throw PairingFailure.invalidLink }
        components.scheme = "wss"
        components.path = "/api/v1/rpc/sealed"
        guard let url = components.url else { throw PairingFailure.invalidLink }
        var request = URLRequest(url: url, timeoutInterval: 30)
        // The channel carries the session; the upgrade carries no credential.
        request.httpShouldHandleCookies = false
        request.setValue(origin, forHTTPHeaderField: "Origin")
        let socket: any SealedSocket
        if let sealedSocket { socket = sealedSocket(request) } else { socket = WebSocketSealedSocket.open(request, in: session) }
        return SealedRPC(socket: socket, origin: origin)
    }

    /// Ends the sealed connection to `origin` (or every one).
    public func closeSealed(origin: String? = nil) async {
        let closing = origin.map { sealed.detach($0).map { [$0] } ?? [] } ?? detachSealed()
        for client in closing { await client.close() }
    }

    /// Takes the sealed connections open now off this client, for the caller
    /// to close later. A connection opened afterwards is not among them.
    public func detachSealed() -> [SealedRPC] { sealed.removeAll() }

    /// Runs a request through the relay's sealed connection when `origin` is a
    /// relay address; `nil` for an ordinary address.
    private func sealedData(_ request: URLRequest, origin: String, deadline: TimeInterval? = nil) async throws -> (Data, URLResponse)? {
        guard sealed.isRelay(origin) else { return nil }
        guard let client = sealed.client(origin) else {
            throw sealed.isSignedOut(origin) ? PairingFailure.response(401) : URLError(.networkConnectionLost)
        }
        do { return try await client.http(request, deadline: deadline) }
        catch SealedRPCFailure.closed {
            sealed.remove(client, for: origin)
            throw URLError(.networkConnectionLost)
        } catch SealedRPCFailure.refused { throw URLError(.badServerResponse) }
    }

    /// Sockets and streamed downloads have no sealed form yet. A feature the
    /// relay can't carry says so (`refusal`), so callers explain it rather
    /// than retrying as if the network dropped.
    private func refusePlaintextRelay(_ origin: String, _ refusal: Error = URLError(.networkConnectionLost)) throws {
        if sealed.isRelay(origin) { throw refusal }
    }
    public func eventSocket(connection: SavedConnection) throws -> URLSessionWebSocketTask {
        let origin = try PairingLink.origin(connection.origin)
        try refusePlaintextRelay(origin)
        guard var components = URLComponents(string: origin) else { throw PairingFailure.invalidLink }
        components.scheme = "wss"
        components.path = "/api/v1/events"
        guard let url = components.url else { throw PairingFailure.invalidLink }
        var request = URLRequest(url: url, timeoutInterval: 30)
        request.httpShouldHandleCookies = false
        request.setValue(origin, forHTTPHeaderField: "Origin")
        request.setValue("__Host-wonder_session=\(connection.credential.sessionToken)", forHTTPHeaderField: "Cookie")
        request.setValue(Self.compactView, forHTTPHeaderField: Self.compactViewHeader)
        return session.webSocketTask(with: request)
    }
    /// The socket for one owner terminal; the first frame is a `TerminalAttach`.
    public func terminalSocket(connection: SavedConnection, terminalID: String) throws -> URLSessionWebSocketTask {
        guard terminalID.hasPrefix("term_"), terminalID.count <= 64,
              terminalID.dropFirst(5).allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber) }) else { throw PairingFailure.invalidLink }
        let origin = try PairingLink.origin(connection.origin)
        try refusePlaintextRelay(origin, RelayUnsupported.terminal)
        guard var components = URLComponents(string: origin) else { throw PairingFailure.invalidLink }
        components.scheme = "wss"
        components.path = "/api/v1/terminals/\(terminalID)/socket"
        guard let url = components.url else { throw PairingFailure.invalidLink }
        var request = URLRequest(url: url, timeoutInterval: 30)
        request.httpShouldHandleCookies = false
        request.setValue(origin, forHTTPHeaderField: "Origin")
        request.setValue("__Host-wonder_session=\(connection.credential.sessionToken)", forHTTPHeaderField: "Cookie")
        return session.webSocketTask(with: request)
    }
    public func download(_ path: String, connection: SavedConnection, file: ConversationFile) async throws -> Data {
        let (data, http) = try await fetchFile(path, connection: connection, file: file)
        try file.verify(data, mime: http.mimeType)
        return data
    }
    /// The header naming a phone-sized preview's own SHA-256.
    public static let previewSha256Header = "x-wonder-preview-sha256"
    /// What the Mac says when a picture it kept by reference is gone or changed.
    public static let mediaSourceGone = "media-source-gone"
    /// A picture as the phone shows it in a conversation: a JPEG at most
    /// `maxPixels` on its long edge that the Mac made from the verified
    /// original, checked against the hash it names; or the original itself
    /// (already small, or a Mac without previews), checked as `download` does.
    public func downloadPreview(_ path: String, connection: SavedConnection, file: ConversationFile, maxPixels: Int) async throws -> Data {
        let (data, http) = try await fetchFile(path + "?maxPixels=\(maxPixels)", connection: connection, file: file)
        if let expected = http.value(forHTTPHeaderField: Self.previewSha256Header) {
            try ConversationFile.verifyPreview(data, mime: http.mimeType, sha256: expected, of: file)
        } else {
            try file.verify(data, mime: http.mimeType)
        }
        return data
    }
    private func fetchFile(_ path: String, connection: SavedConnection, file: ConversationFile) async throws -> (Data, HTTPURLResponse) {
        guard file.byteSize.map({ $0 >= 0 && $0 <= 8 * 1024 * 1024 }) == true else { throw FileFailure.tooLarge }
        let origin = try PairingLink.origin(connection.origin)
        guard let url = URL(string: origin + path) else { throw PairingFailure.invalidLink }
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 30)
        request.httpShouldHandleCookies = false
        request.setValue(origin, forHTTPHeaderField: "Origin")
        let data: Data, response: URLResponse
        func accept(_ response: URLResponse) throws {
            let http = response as? HTTPURLResponse
            if http?.statusCode == 410, http?.value(forHTTPHeaderField: Self.errorCodeHeader) == Self.mediaSourceGone { throw FileFailure.gone }
            guard http?.statusCode == 200 else { throw PairingFailure.response(http?.statusCode ?? 0) }
        }
        if let relayed = try await sealedData(request, origin: origin, deadline: Self.relayFileDeadline(byteSize: file.byteSize)) {
            (data, response) = relayed
            try accept(response)
            guard data.count <= 8 * 1024 * 1024 else { throw FileFailure.tooLarge }
        } else {
            request.setValue("__Host-wonder_session=\(connection.credential.sessionToken)", forHTTPHeaderField: "Cookie")
            let (bytes, streamed) = try await session.bytes(for: request)
            response = streamed
            try accept(response)
            data = try await Self.collect(bytes, limit: 8 * 1024 * 1024, expected: file.byteSize, exceeded: FileFailure.tooLarge)
        }
        guard let http = response as? HTTPURLResponse else { throw PairingFailure.response(0) }
        return (data, http)
    }
    /// On the relay a file arrives as one sealed frame with no progress until
    /// the last byte, so its deadline covers the transfer: 30 s plus a second
    /// per 128 KB (about 1 Mbit/s), about 94 s for an 8 MB original.
    static func relayFileDeadline(byteSize: Int?) -> TimeInterval {
        30 + Double(max(0, byteSize ?? 0)) / Double(128 * 1024)
    }
    /// Workspace previews stream from the paired Mac; large PDFs and images
    /// take time over Wi-Fi, so callers can show progress. An open preview
    /// can hold the shown and a newer revision while PDFKit, WebKit or image
    /// decoding add their own working memory, so the cap follows installed
    /// RAM: about 1/32 of it, 128 MB on a 4 GB iPhone and 256 MB from 8 GB.
    public static let workspacePreviewLimit = previewLimit(physicalMemory: ProcessInfo.processInfo.physicalMemory)
    static func previewLimit(physicalMemory: UInt64) -> Int {
        let megabyte: UInt64 = 1024 * 1024
        // Round down to whole megabytes; reported RAM is slightly under nominal.
        let scaled = (physicalMemory / 32 / megabyte) * megabyte
        return Int(min(256 * megabyte, max(64 * megabyte, scaled)))
    }
    /// A whole verified workspace file, as a preview or comment needs it.
    public func downloadWorkspaceBytes(_ path: String, connection: SavedConnection, byteSize: Int?, sha256: String?, mimeType: String?,
                                       progress: (@Sendable (_ received: Int, _ expected: Int?) -> Void)? = nil) async throws -> Data {
        guard case .read(let read) = try await readWorkspaceFile(path, connection: connection, byteSize: byteSize, sha256: sha256,
                                                                  mimeType: mimeType, progress: progress),
              !read.isPartial else { throw URLError(.badServerResponse) }
        return read.data
    }
    /// The most a preview asks for when it only shows a file's start.
    public static let workspacePrefixLimit = 1024 * 1024
    /// Reads a workspace file for preview.
    ///
    /// `ifNoneMatch` sends back the `validator` of an earlier read; the Mac
    /// answers `.unchanged` without a body while that revision is current.
    /// The validator is the Mac's change token for the file, not a hash of
    /// its bytes, so every body that does arrive is still checked here.
    /// `prefix` asks for only the file's first bytes. A longer file then
    /// arrives as a partial read (`isPartial`) whose bytes are not the file:
    /// `sha256` can't check them, and their hash must never stand in for the
    /// file's (comments and exports hash a whole read). A Mac without prefix
    /// reads sends the whole file, which is checked as an ordinary read.
    public func readWorkspaceFile(_ path: String, connection: SavedConnection, byteSize: Int?, sha256: String?, mimeType: String?,
                                  prefix: Int? = nil, ifNoneMatch: String? = nil,
                                  progress: (@Sendable (_ received: Int, _ expected: Int?) -> Void)? = nil) async throws -> WorkspaceFileReadResult {
        let limit = Self.workspacePreviewLimit
        if let prefix {
            guard prefix > 0, prefix <= Self.workspacePrefixLimit, sha256 == nil else { throw FileFailure.integrity }
        } else {
            guard byteSize.map({ $0 >= 0 && $0 <= limit }) != false else { throw FileFailure.tooLarge }
        }
        let origin = try PairingLink.origin(connection.origin)
        try refusePlaintextRelay(origin, RelayUnsupported.workspacePreview)
        let target = prefix.map { path + (path.contains("?") ? "&" : "?") + "prefix=\($0)" } ?? path
        guard let url = URL(string: origin + target) else { throw PairingFailure.invalidLink }
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 30)
        request.httpShouldHandleCookies = false
        request.setValue(origin, forHTTPHeaderField: "Origin")
        request.setValue("__Host-wonder_session=\(connection.credential.sessionToken)", forHTTPHeaderField: "Cookie")
        if let ifNoneMatch { request.setValue(ifNoneMatch, forHTTPHeaderField: "If-None-Match") }
        // Download to a file and map it: a large PDF or image never needs a
        // second full copy in memory while it arrives.
        let expected = prefix.map { prefix in byteSize.map { min($0, prefix) } ?? prefix } ?? byteSize
        let watcher = WorkspaceDownloadProgress(limit: limit, byteSize: expected, progress: progress)
        progress?(0, expected)
        let session = session
        let (file, response): (URL?, URLResponse) = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                let task = session.downloadTask(with: request) { location, response, error in
                    watcher.finish()
                    if let error { continuation.resume(throwing: watcher.exceeded ? FileFailure.tooLarge : error); return }
                    guard let response else { continuation.resume(throwing: URLError(.badServerResponse)); return }
                    // An unchanged file has no body to keep.
                    if (response as? HTTPURLResponse)?.statusCode == 304 { continuation.resume(returning: (URL?.none, response)); return }
                    guard let location else { continuation.resume(throwing: URLError(.badServerResponse)); return }
                    // The system removes its file when this handler returns.
                    let kept = FileManager.default.temporaryDirectory
                        .appendingPathComponent("wonder-preview-" + UUID().uuidString)
                    do {
                        try FileManager.default.moveItem(at: location, to: kept)
                        continuation.resume(returning: (Optional(kept), response))
                    } catch { continuation.resume(throwing: error) }
                }
                watcher.start(task)
            }
        } onCancel: { watcher.cancel() }
        defer { if let file { try? FileManager.default.removeItem(at: file) } }
        try Task.checkCancellation()
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        if status == 304, ifNoneMatch != nil { return .unchanged }
        guard let http = response as? HTTPURLResponse, let downloaded = file,
              status == 200 || (status == 206 && prefix != nil) else {
            throw status == 413 ? FileFailure.tooLarge : PairingFailure.response(status)
        }
        let size = (try? downloaded.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
        guard size <= limit else { throw FileFailure.tooLarge }
        // A mapped file stays readable after its directory entry is removed.
        let data = size == 0 ? Data() : try Data(contentsOf: downloaded, options: .alwaysMapped)
        progress?(data.count, data.count)
        let validator = http.value(forHTTPHeaderField: "ETag").flatMap { $0.isEmpty ? nil : $0 }
        if status == 206 {
            // Only the start was sent: `bytes 0-(n-1)/total`, n of a longer file.
            guard let prefix, let total = Self.prefixTotal(http.value(forHTTPHeaderField: "Content-Range"), received: data.count),
                  data.count <= prefix, byteSize == nil || byteSize == total else { throw FileFailure.integrity }
            if let mimeType { try Self.validatePrefix(data, mime: mimeType) }
            return .read(WorkspaceFileRead(data: data, totalBytes: total, validator: validator))
        }
        guard byteSize == nil || byteSize == data.count,
              sha256 == nil || sha256 == ConversationFile.digest(data) else { throw FileFailure.integrity }
        if let mimeType { try ConversationFile.validateContent(data, mime: mimeType) }
        return .read(WorkspaceFileRead(data: data, totalBytes: data.count, validator: validator))
    }
    /// The file length a prefix read's `Content-Range` names, when it covers
    /// exactly the bytes received from the start of a longer file.
    static func prefixTotal(_ contentRange: String?, received: Int) -> Int? {
        guard received > 0, let value = contentRange?.trimmingCharacters(in: .whitespaces),
              value.hasPrefix("bytes 0-") else { return nil }
        let parts = value.dropFirst("bytes 0-".count).split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2, let last = Int(parts[0]), let total = Int(parts[1]),
              last == received - 1, total > received else { return nil }
        return total
    }
    /// A prefix may end part-way through a character, so a text prefix is
    /// checked without its last (at most three) bytes.
    static func validatePrefix(_ data: Data, mime: String) throws {
        guard mime.hasPrefix("text/") else { try ConversationFile.validateContent(data, mime: mime); return }
        guard (0...min(3, data.count)).contains(where: { String(data: data.dropLast($0), encoding: .utf8) != nil }) else {
            throw FileFailure.integrity
        }
    }
    public func downloadWorkspaceMediaRange(_ path: String, connection: SavedConnection,
                                            start: UInt64, end: UInt64, revision: String?) async throws -> WorkspaceMediaChunk {
        guard end >= start, end - start < 1024 * 1024,
              (start == 0 && end == 0) || revision != nil else { throw FileFailure.integrity }
        let origin = try PairingLink.origin(connection.origin)
        try refusePlaintextRelay(origin, RelayUnsupported.workspacePreview)
        guard let url = URL(string: origin + path) else { throw PairingFailure.invalidLink }
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 30)
        request.httpShouldHandleCookies = false
        request.setValue(origin, forHTTPHeaderField: "Origin")
        request.setValue("__Host-wonder_session=\(connection.credential.sessionToken)", forHTTPHeaderField: "Cookie")
        request.setValue("bytes=\(start)-\(end)", forHTTPHeaderField: "Range")
        if let revision { request.setValue(revision, forHTTPHeaderField: "X-Wonder-Revision") }
        let (bytes, response) = try await session.bytes(for: request)
        if (response as? HTTPURLResponse)?.statusCode == 409 { throw FileFailure.stale }
        guard let http = response as? HTTPURLResponse, http.statusCode == 206 else {
            throw PairingFailure.response((response as? HTTPURLResponse)?.statusCode ?? 0)
        }
        guard let contentRange = http.value(forHTTPHeaderField: "Content-Range"),
              let range = Self.parseMediaContentRange(contentRange), range.start == start,
              range.end <= end, range.end < range.total,
              let contentLength = http.value(forHTTPHeaderField: "Content-Length").flatMap(UInt64.init),
              contentLength == range.end - range.start + 1,
              let mime = http.mimeType?.lowercased(),
              ["video/mp4", "video/quicktime", "audio/mp4", "audio/mpeg", "audio/wav"].contains(mime),
              let responseRevision = http.value(forHTTPHeaderField: "X-Wonder-Revision"),
              responseRevision.count == 64, responseRevision.unicodeScalars.allSatisfy({ CharacterSet(charactersIn: "0123456789abcdef").contains($0) }) else { throw FileFailure.integrity }
        if let revision, revision != responseRevision { throw FileFailure.stale }
        let data = try await Self.collect(bytes, limit: Int(contentLength), expected: Int(contentLength), exceeded: FileFailure.integrity)
        guard UInt64(data.count) == contentLength else { throw FileFailure.integrity }
        return WorkspaceMediaChunk(bytes: data, start: range.start, end: range.end,
                                   total: range.total, mimeType: mime, revision: responseRevision)
    }

    /// Reads a response body in 64 KB chunks, failing with `exceeded` as soon as
    /// it passes `limit`. Appending to `Data` one byte at a time cost about six
    /// times as long for an 8 MB file.
    static func collect<Bytes: AsyncSequence>(_ bytes: Bytes, limit: Int, expected: Int?, exceeded: Error) async throws -> Data where Bytes.Element == UInt8 {
        let chunkSize = 64 * 1024
        var data = Data()
        if let expected, expected > 0 { data.reserveCapacity(min(expected, limit)) }
        var chunk = [UInt8]()
        chunk.reserveCapacity(chunkSize)
        func flush() throws {
            guard data.count + chunk.count <= limit else { throw exceeded }
            data.append(contentsOf: chunk)
            chunk.removeAll(keepingCapacity: true)
        }
        for try await byte in bytes {
            chunk.append(byte)
            if chunk.count == chunkSize { try flush() }
        }
        try flush()
        return data
    }

    private static func parseMediaContentRange(_ value: String) -> (start: UInt64, end: UInt64, total: UInt64)? {
        guard value.hasPrefix("bytes ") else { return nil }
        let range = value.dropFirst("bytes ".count)
        let parts = range.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2, let total = UInt64(parts[1]) else { return nil }
        let bounds = parts[0].split(separator: "-", omittingEmptySubsequences: false)
        guard bounds.count == 2, let start = UInt64(bounds[0]), let end = UInt64(bounds[1]), end >= start else { return nil }
        return (start, end, total)
    }
    /// Asks hosts to omit history data native clients never render (the raw
    /// event list and inline computer-use screenshots). Older hosts ignore it.
    static let compactViewHeader = "X-Wonder-History-View"
    static let compactView = "compact"

    /// Seconds a request may stay idle. An attachment upload sends up to about
    /// 11 MB of base64 and the Mac stores and hashes it before answering, so it
    /// gets longer than ordinary API calls. A project's thread list waits for
    /// the Mac's providers: Macs before Codex's thread index scanned every saved
    /// Codex thread (13-24 s measured), and the Mac bounds each provider call at
    /// 30 s before answering with what it has. Providers checks each install
    /// and sign-in for up to 20 s (both at once); Reconnect verifies and starts
    /// the runtime (up to about 60 s for Codex) and then checks it again.
    private func resource(_ path: String) -> String {
        path.split(separator: "?", maxSplits: 1).first.map(String.init) ?? path
    }

    /// The host names a refusal it expects a client to recover from here.
    public static let errorCodeHeader = "x-wonder-error"
    public static let worktreeNotInProject = "worktree-not-in-project"
    private static let filesOperations = ["roots", "directory", "file", "media", "git/status", "git/changes", "git/diff"]

    public static func timeoutInterval(for path: String, method: String = "GET") -> TimeInterval {
        let resource = path.split(separator: "?", maxSplits: 1).first.map(String.init) ?? path
        // The computer bounds command approval to two 60-second attempts.
        if method == "POST", resource == "/api/v1/t3-code/connect" { return 150 }
        if method == "GET", resource.hasPrefix("/api/v1/projects/"), resource.hasSuffix("/threads") { return 60 }
        if method == "GET", resource == "/api/v1/providers" { return 30 }
        if method == "POST", resource.hasPrefix("/api/v1/providers/"), resource.hasSuffix("/reconnect") { return 120 }
        if path.hasSuffix("/control/acquire") { return 135 }
        // The supervisor answers at once or not at all; a recovery request must not hold the notice up.
        if resource.hasPrefix("/api/v1/rescue/") { return 5 }
        // The computer waits up to two minutes for a started simulator to finish booting.
        if method == "POST", resource.hasPrefix("/api/v1/simulators/"), resource.hasSuffix("/boot") { return 130 }
        // The Mac checks the key or mints a voice pass with OpenAI (15 s bound).
        if resource.hasPrefix("/api/v1/voice/") { return 25 }
        if method == "POST", path.hasPrefix("/api/v1/conversations/"), path.hasSuffix("/files") { return 120 }
        // A push, pull or merge waits on the remote (the Mac stops at 90 s).
        if method == "POST", resource.hasPrefix("/api/v1/conversations/"), resource.hasSuffix("/repos/actions") { return 120 }
        if resource.hasPrefix("/api/v1/conversations/"), resource.hasSuffix("/repos/pull-request") { return 100 }
        return 15
    }

    private static func ownerStopBody(path: String, method: String, body: Data?) throws -> Data? {
        guard method == "POST", path.hasPrefix("/api/v1/conversations/") else { return body }
        let parts = (path.split(separator: "?", maxSplits: 1).first ?? "").split(separator: "/")
        guard parts.count == 7,
              parts[0] == "api", parts[1] == "v1", parts[2] == "conversations",
              parts[4] == "turns", parts[6] == "interrupt" else { return body }
        guard var object = (try? JSONSerialization.jsonObject(with: body ?? Data("{}".utf8))) as? [String: Any]
        else { return body }
        // One request invocation is one Stop tap. Build the identity before
        // either transport sees it so retransmission reuses the same bytes.
        // Explicitly replayed bodies retain their ID (even an invalid one).
        guard !object.keys.contains("ownerActionId") else { return body }
        object["ownerActionId"] = UUID().uuidString.lowercased()
        return try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    }

    public func request<T: Decodable & Sendable>(_ path: String, origin: String, body: Data? = nil,
                                                  credential: Credential? = nil, method: String? = nil,
                                                  decodingStatuses: Set<Int> = [], bearerToken: String? = nil) async throws -> T {
        guard let url = URL(string: try PairingLink.origin(origin) + path) else { throw PairingFailure.invalidLink }
        let httpMethod = method ?? (body == nil ? "GET" : "POST")
        var request = URLRequest(
            url: url,
            cachePolicy: .reloadIgnoringLocalCacheData,
            timeoutInterval: Self.timeoutInterval(for: path, method: httpMethod)
        )
        request.httpMethod = httpMethod
        request.httpBody = try Self.ownerStopBody(path: path, method: httpMethod, body: body)
        request.setValue(origin, forHTTPHeaderField: "Origin")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        if path.hasPrefix("/api/v1/conversations/") { request.setValue(Self.compactView, forHTTPHeaderField: Self.compactViewHeader) }
        request.httpShouldHandleCookies = false
        // A relay address gets no credential in the clear: the sealed
        // connection carries the session and CSRF token.
        let relayed = sealed.isRelay(origin)
        if let credential, !relayed {
            request.setValue("__Host-wonder_session=\(credential.sessionToken)", forHTTPHeaderField: "Cookie")
            request.setValue(credential.csrfToken, forHTTPHeaderField: "x-wonder-csrf")
        }
        if let bearerToken, credential == nil { request.setValue("Bearer \(bearerToken)", forHTTPHeaderField: "Authorization") }
        let start = timing == nil ? 0 : ProcessInfo.processInfo.systemUptime
        var received = start; var byteCount = 0; var succeeded = false
        defer { if let timing { timing((received-start)*1000, (ProcessInfo.processInfo.systemUptime-received)*1000, byteCount, succeeded) } }
        let (data, response): (Data, URLResponse)
        do {
            if relayed, let answer = try await sealedData(request, origin: origin) { (data, response) = answer }
            else { (data, response) = try await session.data(for: request) }
        }
        catch { received = ProcessInfo.processInfo.systemUptime; throw error }
        received = timing == nil ? 0 : ProcessInfo.processInfo.systemUptime; byteCount = data.count
        guard let http = response as? HTTPURLResponse,
              (200..<300).contains(http.statusCode) || decodingStatuses.contains(http.statusCode) else {
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            if path.hasPrefix("/api/v1/conversations/"), (path.hasSuffix("/messages") || path.hasSuffix("/steer")),
               [412, 422].contains(status), data.count <= 4096,
               let error = try? JSONDecoder().decode([String: String].self, from: data),
               error["error"] == "codex_skill_rejected", let detail = error["detail"], !detail.isEmpty {
                throw PairingFailure.skillRejected(status, detail)
            }
            if path.hasPrefix("/api/v1/conversations/"), path.hasSuffix("/messages"),
               [403, 409, 422].contains(status),
               let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               message.utf8.count <= 512,
               (message.hasPrefix("This annotation") || message.hasPrefix("Annotations require") ||
                message.hasPrefix("This file changed since you annotated") ||
                message.hasPrefix("Project folders changed. Reopen the annotation preview") ||
                message.hasPrefix("An annotated Project folder changed. Reopen the preview")) {
                throw PairingFailure.annotationRejected(status, message)
            }
            // A refused secret reply says why (expired, Keychain unavailable, value too large).
            // The host's words never contain the value.
            if path.hasPrefix("/api/v1/secret-requests/"), [400, 404, 409, 412].contains(status), data.count <= 4096 {
                if let reply = try? JSONDecoder().decode([String: String].self, from: data), reply["error"] == "already_resolved" {
                    throw PairingFailure.hostMessage(status, "This request was already answered or has expired.")
                }
                if let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
                   !message.isEmpty, message.utf8.count <= 512, !message.hasPrefix("{"), !message.hasPrefix("<") {
                    throw PairingFailure.hostMessage(status, message)
                }
            }
            // A refused approval answer says why (expired when the provider restarted, already in progress).
            if path.hasPrefix("/api/v1/approvals/"), path.hasSuffix("/resolve"), status == 409,
               let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !message.isEmpty, message.utf8.count <= 512, !message.hasPrefix("{"), !message.hasPrefix("<") {
                throw PairingFailure.hostMessage(status, message)
            }
            // The host words a refused Stop in plain language (running on the Mac, finished).
            if path.hasPrefix("/api/v1/project-conversations/"), path.hasSuffix("/stop"),
               [409, 422, 503].contains(status),
               let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !message.isEmpty, message.utf8.count <= 512 {
                throw PairingFailure.hostMessage(status, message)
            }
            // An agent task the host cannot read (T3 Code not answering) says why.
            if path.hasPrefix("/api/v1/project-conversations/"), resource(path).hasSuffix("/transcript"),
               resource(path).contains("/subagents/"), status == 503,
               let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !message.isEmpty, message.utf8.count <= 512, !message.hasPrefix("{"), !message.hasPrefix("<") {
                throw PairingFailure.hostMessage(status, message)
            }
            // Edit from here and file backups say why the Mac refused (still
            // working, open on the Mac, no backups for those replies), as do a
            // refused custom instruction or thread tools change (too long, empty).
            if (path.hasPrefix("/api/v1/conversations/") && resource(path).hasSuffix("/rewind"))
                || resource(path).hasPrefix("/api/v1/settings/file-restore")
                || resource(path).hasPrefix(CustomInstructionSettings.path)
                || resource(path).hasPrefix(ThreadToolSettings.path),
               [409, 422, 502, 503].contains(status),
               let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !message.isEmpty, message.utf8.count <= 512, !message.hasPrefix("{"), !message.hasPrefix("<") {
                throw PairingFailure.hostMessage(status, message)
            }
            // A refused provider runtime setting says why in plain words (no file at that
            // path, a name Cupola owns, a Keychain that is locked).
            if resource(path).hasPrefix(ProviderRuntimeSettings.path), [400, 404, 503].contains(status),
               let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !message.isEmpty, message.utf8.count <= 512, !message.hasPrefix("{"), !message.hasPrefix("<") {
                throw PairingFailure.hostMessage(status, message)
            }
            // A refused Reconnect or Update says why (an agent is working, Claude
            // didn't restart, the provider's installer isn't one Cupola runs).
            if path.hasPrefix("/api/v1/providers/"), path.hasSuffix("/reconnect") || path.hasSuffix("/update"),
               [409, 503].contains(status),
               let reply = try? JSONDecoder().decode([String: String].self, from: data),
               let message = reply["detail"], !message.isEmpty, message.utf8.count <= 512 {
                throw PairingFailure.hostMessage(status, message)
            }
            let refusal = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            let plainRefusal = !refusal.isEmpty && refusal.utf8.count <= 512 && !refusal.hasPrefix("{") && !refusal.hasPrefix("<")
            // Opening a chat must keep the host's specific refusal all the way
            // to the phone. The attach/detail routes use {"error": words};
            // history and older hosts may answer with plain words instead.
            // A 401 retains the shared session-renewal owner and its recovery.
            let openingChat = (httpMethod == "POST" && resource(path).hasPrefix("/api/v1/projects/") && resource(path).hasSuffix("/threads/attach"))
                || (httpMethod == "GET" && ((resource(path).split(separator: "/").count == 4
                    && (resource(path).hasPrefix("/api/v1/project-conversations/") || resource(path).hasPrefix("/api/v1/conversations/")))
                    || (resource(path).hasPrefix("/api/v1/conversations/") && resource(path).hasSuffix("/history") && resource(path).split(separator: "/").count == 5)))
            // Delete must keep the same bounded host words: unfinished work
            // or a search-cache cleanup failure has a specific recovery.
            // Authentication failures keep their existing shared handling.
            let deletingChat = httpMethod == "DELETE" && resource(path).hasPrefix("/api/v1/project-conversations/")
                && resource(path).split(separator: "/").count == 4
            if (openingChat || (deletingChat && [409, 422, 503].contains(status))),
               (400..<600).contains(status), status != 401, data.count <= 4096 {
                let reply = try? JSONDecoder().decode([String: String].self, from: data)
                let words = (reply?["error"] ?? reply?["detail"] ?? (plainRefusal ? refusal : ""))
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                if !words.isEmpty, words.utf8.count <= 512, !words.hasPrefix("{"), !words.hasPrefix("<") {
                    throw PairingFailure.hostMessage(status, words)
                }
            }
            // Files and Terminal offer to add a worktree the Project leaves out.
            let filesRequest = path.hasPrefix("/api/v1/conversations/")
                && Self.filesOperations.contains(where: { resource(path).hasSuffix("/workspace/" + $0) })
            if filesRequest || resource(path).hasSuffix("/terminals"), status == 403, plainRefusal,
               (response as? HTTPURLResponse)?.value(forHTTPHeaderField: Self.errorCodeHeader) == Self.worktreeNotInProject {
                throw PairingFailure.worktreeNotInProject(refusal)
            }
            // Files says why the Mac refused a folder or file in its own words.
            // A missing folder (404) keeps its own recovery in the browser.
            if filesRequest, [403, 409, 422].contains(status), plainRefusal {
                throw PairingFailure.hostMessage(status, refusal)
            }
            // Terminals say why one could not open (the limit, a protected folder).
            if resource(path).hasSuffix("/terminals") || resource(path).hasPrefix("/api/v1/terminals/"),
               [403, 404, 409, 429, 503].contains(status),
               let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !message.isEmpty, message.utf8.count <= 512, !message.hasPrefix("{"), !message.hasPrefix("<") {
                throw PairingFailure.hostMessage(status, message)
            }
            // The Git pill says why the Mac would not read a thread's folder (a
            // worktree left out of the Project, a removed Project).
            if path.hasPrefix("/api/v1/conversations/"), resource(path).hasSuffix("/workspace/git/summary"),
               [403, 404, 409].contains(status),
               let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !message.isEmpty, message.utf8.count <= 512, !message.hasPrefix("{"), !message.hasPrefix("<") {
                throw PairingFailure.hostMessage(status, message)
            }
            // New thread in T3 Code says what to do instead (connect T3 Code,
            // add the folder there, open it) or that it may have started.
            if resource(path).hasPrefix("/api/v1/projects/"), resource(path).hasSuffix("/t3-code/threads"),
               [404, 409, 422, 502, 503, 504].contains(status), plainRefusal {
                throw PairingFailure.hostMessage(status, refusal)
            }
            // Voice explains a missing or refused OpenAI key in the Mac's words.
            if path.hasPrefix("/api/v1/voice"), [409, 422, 502, 503].contains(status),
               let reply = try? JSONDecoder().decode([String: String].self, from: data),
               let message = reply["detail"], !message.isEmpty, message.utf8.count <= 512 {
                throw PairingFailure.hostMessage(status, message)
            }
            // Run now and Delete say why the Mac refused (a run in progress, the
            // agent offline, the thread unavailable).
            if resource(path).hasPrefix("/api/v1/automations/"),
               (httpMethod == "POST" && resource(path).hasSuffix("/run")) || httpMethod == "DELETE",
               [409, 503].contains(status),
               let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !message.isEmpty, message.utf8.count <= 512, !message.hasPrefix("{"), !message.hasPrefix("<") {
                throw PairingFailure.hostMessage(status, message)
            }
            // Project creation explains a refused folder or name in plain language.
            if path == "/api/v1/projects", httpMethod == "POST", [409, 422].contains(status),
               let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !message.isEmpty, message.utf8.count <= 512, !message.hasPrefix("Failed to") {
                throw PairingFailure.hostMessage(status, message)
            }
            // Opening the computer view says which device is viewing it, so
            // the phone can offer Take over. An older host's plain 409 stays as is.
            if resource(path) == "/api/v1/computer/sessions", httpMethod == "POST", status == 409,
               data.count <= 4096, let conflict = try? JSONDecoder().decode(ComputerViewerConflict.self, from: data) {
                throw conflict
            }
            throw PairingFailure.response(status)
        }
        let value: T
        do {
            let body = data.isEmpty ? Data("{}".utf8) : data
            // Conversation pages carry free-form tool payloads: read them in one pass.
            value = T.self is ThreadJSONDecodable.Type ? try ThreadJSON.decode(T.self, from: body) : try JSONDecoder().decode(T.self, from: body)
        } catch where !(200..<300).contains(http.statusCode) {
            throw PairingFailure.response(http.statusCode)
        }
        succeeded = true
        return value
    }

}


/// Reports a workspace download's progress and stops one that grows past the limit.
/// One read of a workspace file for preview.
public struct WorkspaceFileRead: Sendable, Equatable {
    /// The whole file, or only its first bytes when `isPartial`.
    public let data: Data
    /// The file's length on the Mac.
    public let totalBytes: Int
    /// The Mac's change token for this revision, sent back to ask whether
    /// the file changed. It is not a content hash; nil when the Mac can't
    /// vouch for the revision yet (a file written moments ago, an older Mac).
    public let validator: String?
    public var isPartial: Bool { data.count < totalBytes }
    public init(data: Data, totalBytes: Int, validator: String?) {
        self.data = data; self.totalBytes = totalBytes; self.validator = validator
    }
}

public enum WorkspaceFileReadResult: Sendable, Equatable {
    /// The revision named by `ifNoneMatch` is still current.
    case unchanged
    case read(WorkspaceFileRead)
}

final class WorkspaceDownloadProgress: @unchecked Sendable {
    private let limit: Int
    private let byteSize: Int?
    private let progress: (@Sendable (_ received: Int, _ expected: Int?) -> Void)?
    private let lock = NSLock()
    private var task: URLSessionDownloadTask?
    private var observation: NSKeyValueObservation?
    private var cancelled = false
    private var tooLarge = false
    private var lastReported = 0
    var exceeded: Bool { lock.withLock { tooLarge } }

    init(limit: Int, byteSize: Int?, progress: (@Sendable (_ received: Int, _ expected: Int?) -> Void)?) {
        self.limit = limit; self.byteSize = byteSize; self.progress = progress
    }
    func start(_ task: URLSessionDownloadTask) {
        let observation = task.observe(\.countOfBytesReceived, options: [.new]) { [weak self] task, _ in
            self?.received(Int(clamping: task.countOfBytesReceived), Int(clamping: task.countOfBytesExpectedToReceive), task)
        }
        let cancel = lock.withLock { () -> Bool in
            self.task = task; self.observation = observation
            return cancelled
        }
        if cancel { task.cancel() } else { task.resume() }
    }
    func cancel() {
        let task = lock.withLock { () -> URLSessionDownloadTask? in cancelled = true; return self.task }
        task?.cancel()
    }
    func finish() {
        let observation = lock.withLock { () -> NSKeyValueObservation? in
            defer { self.observation = nil }
            return self.observation
        }
        observation?.invalidate()
    }
    private func received(_ received: Int, _ reported: Int, _ task: URLSessionDownloadTask) {
        let expected = reported > 0 ? reported : byteSize
        if received > limit || (expected ?? 0) > limit {
            lock.withLock { tooLarge = true }
            task.cancel()
            return
        }
        // About every 256 KB; the UI coalesces further.
        let report = lock.withLock { () -> Bool in
            guard received - lastReported >= 256 * 1024 || received == expected else { return false }
            lastReported = received
            return true
        }
        if report { progress?(received, expected) }
    }
}
