import Foundation
import CryptoKit
import Network
import Security

// The paired computer's route and trust policy: home addresses with their
// pinned key, Tailscale, then the relay's sealed connection. The app and the
// share extension both use it; neither copies it.

/// Notes, across a route race's concurrent probes, that one proved another key.
public final class RouteKeyMismatch: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    public init() {}
    public func note() { lock.withLock { value = true } }
    public var seen: Bool { lock.withLock { value } }
}

/// It has no UIKit dependency, so the owning tests exercise the same code
/// without starting a simulator. A probe must authenticate the host before
/// succeeding.
public struct ConnectionRouteSelector<C: Clock>: Sendable where C.Duration == Duration {
    public struct Selection: Sendable {
        public let route: ConnectionRoute
        public let elapsed: Duration
    }
    public let clock: C
    public let probe: @Sendable (ConnectionRoute) async throws -> Void

    public init(clock: C, probe: @escaping @Sendable (ConnectionRoute) async throws -> Void) {
        self.clock = clock; self.probe = probe
    }

    /// On a local network a home address that answers within its deadline
    /// wins over a faster Tailscale answer, so the phone stays on the direct
    /// path at home. On another network the wait ends with the home deadline,
    /// 1.5 s. The relay is tried only once every other route has failed.
    public func select(_ routes: [ConnectionRoute], localNetworkAvailable: Bool, excluding failedOrigin: String? = nil) async throws -> Selection? {
        let routes = routes.filter { $0.origin != failedOrigin }
        let immediate = routes.filter { $0.kind != .relay && ($0.kind != .home || localNetworkAvailable) }
        let selected = await race(immediate)
        try Task.checkCancellation()
        if let selected { return selected }
        // T3 retries remembered home addresses last only if nothing else works.
        if !localNetworkAvailable {
            let fallback = await race(routes.filter { $0.kind == .home })
            try Task.checkCancellation()
            if let fallback { return fallback }
        }
        // The relay is for when the home network and Tailscale both fail.
        let relay = await race(routes.filter { $0.kind == .relay })
        try Task.checkCancellation()
        return relay
    }

    /// A healthy later route stays in use while earlier routes are checked.
    /// T3 moves back after the first successful check, without racing against
    /// the already connected route or requiring a second success. Arriving on
    /// a local network, home wins over a faster Tailscale answer here too.
    public func switchBack(_ routes: [ConnectionRoute], currentOrigin: String, localNetworkAvailable: Bool) async throws -> Selection? {
        // The relay is never a route to move back to, wherever it is listed.
        let earlier = routes.prefix { $0.origin != currentOrigin }
            .filter { $0.kind != .relay && (localNetworkAvailable || $0.kind != .home) }
        let selected = await race(earlier)
        try Task.checkCancellation()
        return selected
    }

    /// The first home address to answer, else the first other route to
    /// answer, held until every home check has answered or missed its deadline.
    private func race(_ routes: [ConnectionRoute]) async -> Selection? {
        await withTaskGroup(of: (ConnectionRoute.Kind, Selection?).self) { group in
            for route in routes {
                group.addTask {
                    let start = clock.now
                    do {
                        try await withThrowingTaskGroup(of: Void.self) { checks in
                            checks.addTask { try await probe(route) }
                            checks.addTask {
                                try await clock.sleep(for: route.kind == .home ? .milliseconds(1500) : .seconds(6))
                                throw URLError(.timedOut)
                            }
                            defer { checks.cancelAll() }
                            try await checks.next()
                        }
                        try Task.checkCancellation()
                        return (route.kind, Selection(route: route, elapsed: start.duration(to: clock.now)))
                    } catch { return (route.kind, nil) }
                }
            }
            defer { group.cancelAll() }
            var homeChecks = routes.filter { $0.kind == .home }.count
            var answered: Selection?
            while case let (kind, result)? = await group.next() {
                if kind == .home { homeChecks -= 1 }
                if let result, kind == .home || homeChecks <= 0 { return result }
                answered = answered ?? result
                if homeChecks <= 0, let answered { return answered }
            }
            return answered
        }
    }
}

public extension SigningIdentity {
    /// Signs the sealed connection's challenge with this device's key.
    nonisolated var sealedSign: SealedRPC.Sign { { [self] transcript in try await self.sign(transcript) } }
}

/// Home addresses whose TLS certificate carried another key. Evidence counts
/// only for the attempt that saw it: take a `mark()` before connecting and ask
/// about certificates seen since, so a past mismatch never explains a later
/// timeout.
public protocol TLSKeyMismatches: Sendable {
    func mark() -> UInt64
    func provedAnotherKey(_ origin: String, since mark: UInt64) -> Bool
}

public enum ConnectionRouteProber {
    /// `keyMismatches` reports a home address whose TLS certificate carried
    /// another key: URLSession fails that connection before `/host/proof`, but
    /// it is still a key change, not an unreachable computer.
    public static func check(_ route: ConnectionRoute, saved: SavedConnection, api: PairingAPI,
                             keyMismatches: (any TLSKeyMismatches)? = nil, sign: SealedRPC.Sign? = nil) async throws {
        guard try PairingLink.origin(route.origin) == route.origin else { throw PairingFailure.invalidLink }
        // The relay serves only the sealed channel: its handshake proves the
        // pinned key, and the session goes inside it.
        if route.kind == .relay || api.isRelay(route.origin) {
            guard let pin = saved.hostKeyPin, let sign else { throw PairingFailure.wrongHost }
            do { _ = try await api.sealedConnection(origin: route.origin, credential: saved.credential, pin: pin, sign: sign) }
            catch SealedChannelFailure.hostProof { throw PairingFailure.wrongHost }
            // The computer proved its key and refused only the session, which
            // the connection check then renews inside the channel.
            catch PairingFailure.response(401) {}
            return
        }
        // Never send a session to a learned/recycled home address without its pin.
        guard saved.hostKeyPin != nil || (!route.learned && route.kind != .home) else { throw PairingFailure.wrongHost }
        struct Ready: Decodable, Sendable {}
        let since = keyMismatches?.mark() ?? 0
        do { let _: Ready = try await api.request("/readyz", origin: route.origin) }
        catch let error as URLError {
            if saved.hostKeyPin != nil, keyMismatches?.provedAnotherKey(route.origin, since: since) == true { throw PairingFailure.wrongHost }
            throw error
        }
        try Task.checkCancellation()
        if let pin = saved.hostKeyPin {
            let nonce = UUID().uuidString.replacingOccurrences(of: "-", with: "")
            let proof: HostProof = try await api.request("/api/v1/host/proof?nonce=\(nonce)", origin: route.origin)
            try proof.validate(pin: pin, origin: route.origin, nonce: nonce)
        }
    }
}

/// What to check when no saved way in reached the computer. Same Wi-Fi is the
/// usual setup; Tailscale reaches it from anywhere once added.
public enum UnreachableAdvice {
    public static func status(routes: [ConnectionRoute]) -> String {
        let home = routes.contains { $0.kind == .home }
        let tailscale = routes.contains { route in
            route.kind == .tailscale && (URL(string: route.origin)?.host?.hasSuffix(".ts.net") ?? false)
        }
        switch (home, tailscale) {
        case (true, false):
            return "Could not reach your computer. Make sure it’s on, Cupola is running and this device is on the same Wi-Fi, then try again."
        case (true, true):
            return "Could not reach your computer. Make sure it’s on and Cupola is running, and that this device is on the same Wi-Fi or connected to Tailscale."
        case (false, true):
            return "Could not reach your computer. Make sure it’s on, Cupola is running and Tailscale is connected on this device, then try again."
        case (false, false):
            return "Could not reach your computer. Make sure it’s on and Cupola is running, then try again."
        }
    }
}

/// Trust overrides apply only to the exact private address and port learned
/// from a pinned host. Everything else keeps the system's normal trust policy.
public final class HomeNetworkTrust: NSObject, URLSessionDelegate, TLSKeyMismatches, @unchecked Sendable {
    public enum Decision: Equatable, Sendable { case system, accept, reject }
    private let lock = NSLock()
    private var pins: [String: String] = [:]
    /// Pinned addresses whose last certificate carried another key: the pin
    /// it failed and when. A pin change or an accepted certificate clears it.
    private var mismatches: [String: (pin: String, seen: UInt64)] = [:]
    private var seen: UInt64 = 0

    public override init() { super.init() }

    public static func isHomeOrigin(_ origin: String) -> Bool {
        guard (try? PairingLink.origin(origin)) == origin,
              let host = URLComponents(string: origin)?.host else { return false }
        let address = host.trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        if let v4 = IPv4Address(address) {
            let b = Array(v4.rawValue)
            return b[0] == 10 || (b[0] == 172 && (16...31).contains(b[1])) || (b[0] == 192 && b[1] == 168)
        }
        if let v6 = IPv6Address(address) {
            let b = Array(v6.rawValue)
            return b[0] & 0xfe == 0xfc && Array(b.prefix(6)) != [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0]
        }
        return false
    }

    /// TLS challenges report port 443 without it, so pins and mismatches use the
    /// origin without a default port whatever form the saved address kept.
    public static func pinKey(_ origin: String) -> String {
        origin.hasSuffix(":443") ? String(origin.dropLast(4)) : origin
    }

    public func update(_ saved: SavedConnection?) { update(saved.map { [$0] } ?? []) }

    /// Shared background clients may serve several paired computers. An address
    /// claimed by different keys is ambiguous and stays untrusted.
    public func update(_ connections: [SavedConnection]) {
        var entries: [String: String] = [:], conflicts: Set<String> = []
        for saved in connections where !saved.requiresPairing {
            guard let pin = saved.hostKeyPin else { continue }
            for route in saved.routes where route.kind == .home && Self.isHomeOrigin(route.origin) {
                // The supervisor's recovery address (one port above) serves the same key.
                for origin in [route.origin] + (RescueRoute.origin(forHome: route).map { [$0] } ?? []) {
                    let key = Self.pinKey(origin)
                    if let previous = entries[key], previous != pin { conflicts.insert(key) }
                    entries[key] = pin
                }
            }
        }
        for origin in conflicts { entries.removeValue(forKey: origin) }
        lock.withLock { pins = entries }
    }

    /// Temporary trust from the QR code, before any pairing secret is sent.
    public func preparePairing(origin: String, pin: String?) throws {
        let entries: [String: String]
        if Self.isHomeOrigin(origin) {
            guard let pin else { throw PairingFailure.wrongHost }
            entries = [Self.pinKey(origin): pin]
        } else { entries = [:] }
        lock.withLock { pins = entries }
    }

    /// Security exports an uncompressed P-256 point. CryptoKit reconstructs its
    /// DER SubjectPublicKeyInfo, the same bytes hashed by the host and P2.
    public static func digest(_ certificate: SecCertificate) -> String? {
        guard let key = SecCertificateCopyKey(certificate),
              let attributes = SecKeyCopyAttributes(key) as? [String: Any],
              attributes[kSecAttrKeyType as String] as? String == kSecAttrKeyTypeECSECPrimeRandom as String,
              attributes[kSecAttrKeySizeInBits as String] as? Int == 256,
              let raw = SecKeyCopyExternalRepresentation(key, nil) as Data?,
              let publicKey = try? P256.Signing.PublicKey(x963Representation: raw) else { return nil }
        return Data(SHA256.hash(data: publicKey.derRepresentation)).base64URL
    }

    public func decision(host: String, port: Int, certificate: SecCertificate?) -> Decision {
        let address = host.contains(":") && !host.hasPrefix("[") ? "[" + host + "]" : host
        let origin = "https://" + address + (port == 443 ? "" : ":\(port)")
        let expected = lock.withLock { pins[origin] }
        guard let expected else { return Self.isHomeOrigin(origin) ? .reject : .system }
        guard let certificate else { return .reject }
        guard Self.digest(certificate) == expected else {
            lock.withLock { seen &+= 1; mismatches[origin] = (expected, seen) }
            return .reject
        }
        lock.withLock { mismatches[origin] = nil }
        return .accept
    }

    public func mark() -> UInt64 { lock.withLock { seen } }

    /// The paired home address answered with a certificate for another key
    /// after `mark`, during the attempt asking.
    public func provedAnotherKey(_ origin: String, since mark: UInt64) -> Bool {
        let key = Self.pinKey(origin)
        return lock.withLock {
            mismatches[key].map { $0.pin == pins[key] && $0.seen > mark } ?? false
        }
    }

    public func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                           completionHandler: @escaping @Sendable (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust else {
            completionHandler(.performDefaultHandling, nil); return
        }
        let trust = challenge.protectionSpace.serverTrust
        let leaf = trust.flatMap { (SecTrustCopyCertificateChain($0) as? [SecCertificate])?.first }
        switch decision(host: challenge.protectionSpace.host, port: challenge.protectionSpace.port, certificate: leaf) {
        case .system: completionHandler(.performDefaultHandling, nil)
        case .reject: completionHandler(.cancelAuthenticationChallenge, nil)
        case .accept:
            guard let trust else { completionHandler(.cancelAuthenticationChallenge, nil); return }
            completionHandler(.useCredential, URLCredential(trust: trust))
        }
    }
}
