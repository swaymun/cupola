import AuthenticationServices
import CryptoKit
import Foundation
import WonderPairing

/// A signed-in session with Wonder's notification service. Tokens are stored
/// in this device's Keychain; the refresh token only works with this device's key.
struct AccountSession: Codable, Equatable, Sendable {
    let sessionId: String
    var accessToken: String
    var accessExpiry: Date
    var refreshToken: String
    var refreshExpiry: Date
    var signedInAt: Date
}

enum AccountFailure: LocalizedError, Equatable {
    case signedOut, storage, unavailable, rejected, otherService
    var errorDescription: String? {
        switch self {
        case .signedOut: "Sign in to get notifications."
        case .storage: "Unlock this device and try again."
        case .unavailable: "Couldn't reach Cupola's notification service. Check your connection and try again."
        case .rejected: "Apple didn't confirm the sign-in. Try again."
        case .otherService: "This computer uses a different notification service from your Cupola account."
        }
    }
}

/// Cupola accounts are for notifications, Live Activities and turning on Reach
/// from anywhere (Cupola's relay). Pairing on Wi-Fi or Tailscale and chats work
/// without one; provider subscriptions and voice keys are separate. Signing out
/// or deleting the account turns notifications off on this iPhone for every Mac.
@MainActor final class WonderAccount: ObservableObject {
    static let shared = WonderAccount()
    /// The official notification service; a source build may name its own with
    /// the `WonderNotificationService` Info.plist key.
    static let officialService = "https://wonder-push.saimun-shahee.workers.dev"
    static let consent = "Notifications use Cupola’s service on Cloudflare to reach this device. Previews are end-to-end encrypted between this device and your computer."

    /// `path, JSON body, bearer token` → response body, or `PairingFailure.response(status)`.
    typealias Post = @Sendable (String, Data, String?) async throws -> Data

    @Published private(set) var session: AccountSession?
    @Published private(set) var working = false
    @Published var error: String?
    var isSignedIn: Bool { session != nil }
    let service: String
    var privacyURL: URL { URL(string: service + "/privacy")! }

    private let read: (String) throws -> Data?
    private let write: (Data, String) throws -> Void
    private let remove: (String) throws -> Void
    private let post: Post
    /// Called when the account ends on this iPhone (sign-out, deletion, or the
    /// service no longer knowing the session), before local state is cleared.
    var ended: () -> Void = { PushNotifications.shared.accountEnded() }
    fileprivate var nonce: String?
    private var refreshing: Task<AccountSession, Error>?
    nonisolated fileprivate static let sessionAccount = "account-session-v1", keyAccount = "account-device-key-v1"

    init(service: String = Bundle.main.object(forInfoDictionaryKey: "WonderNotificationService") as? String ?? WonderAccount.officialService,
         read: @escaping (String) throws -> Data? = { try PhoneIdentity().read($0) },
         write: @escaping (Data, String) throws -> Void = { try PhoneIdentity().save($0, account: $1) },
         remove: @escaping (String) throws -> Void = { try PhoneIdentity().delete($0) },
         post: Post? = nil) {
        var origin = service.hasSuffix("/") ? String(service.dropLast()) : service
        var read = read, write = write, remove = remove, transport = post
        #if WONDER_DIAGNOSTICS
        if transport == nil, let fixture = Self.diagnosticFixture {
            (read, write, remove, transport) = (fixture.read, fixture.write, fixture.remove, fixture.post)
            origin = fixture.service ?? origin
        }
        #endif
        self.service = origin
        self.read = read; self.write = write; self.remove = remove; self.post = transport ?? Self.network(origin)
        session = try? self.read(Self.sessionAccount).flatMap { try JSONDecoder().decode(AccountSession.self, from: $0) }
        if let session, session.refreshExpiry <= Date() { self.session = nil; try? self.remove(Self.sessionAccount) }
    }

    // MARK: Sign in with Apple

    /// Configures Apple's request: no name or email, and a fresh nonce that the
    /// service checks against the identity token.
    func prepare(_ request: ASAuthorizationAppleIDRequest) {
        request.requestedScopes = []
        let value = Self.randomToken()
        nonce = value
        request.nonce = SHA256.hash(data: Data(value.utf8)).map { String(format: "%02x", $0) }.joined()
    }

    func complete(_ result: Result<ASAuthorization, Error>) async {
        switch result {
        case .failure(let failure):
            // Closing Apple's sheet is not an error worth showing.
            if (failure as? ASAuthorizationError)?.code != .canceled { error = AccountFailure.rejected.localizedDescription }
        case .success(let authorization):
            guard let credential = authorization.credential as? ASAuthorizationAppleIDCredential,
                  let token = credential.identityToken.flatMap({ String(data: $0, encoding: .utf8) }) else {
                error = AccountFailure.rejected.localizedDescription
                return
            }
            await signIn(identityToken: token)
        }
    }

    func signIn(identityToken: String) async {
        guard let nonce, !working else { return }
        self.nonce = nil
        working = true; error = nil
        defer { working = false }
        do {
            let key = try deviceKey()
            let signature = try key.signature(for: Data("signin|apple|\(nonce)".utf8)).rawRepresentation.base64URL
            let body = try JSONSerialization.data(withJSONObject: ["identityToken": identityToken, "nonce": nonce,
                                                                  "deviceKey": key.publicKey.x963Representation.base64URL, "signature": signature])
            try store(try await call("/v1/auth/apple", body: body))
        } catch let failure as AccountFailure { error = failure.localizedDescription }
        catch PairingFailure.response(401), PairingFailure.response(403) { error = AccountFailure.rejected.localizedDescription }
        catch { self.error = AccountFailure.unavailable.localizedDescription }
    }

    // MARK: Tokens

    /// A current access token, refreshed with this device's key when it is
    /// about to expire. Throws `AccountFailure.signedOut` without a session.
    func accessToken() async throws -> String {
        guard let session else { throw AccountFailure.signedOut }
        if session.accessExpiry.timeIntervalSinceNow > 60 { return session.accessToken }
        if let refreshing { return try await refreshing.value.accessToken }
        let task = Task { try await refresh(session) }
        refreshing = task
        defer { refreshing = nil }
        return try await task.value.accessToken
    }

    private func refresh(_ current: AccountSession) async throws -> AccountSession {
        let timestamp = Int(Date().timeIntervalSince1970)
        let signature = try deviceKey().signature(for: Data("refresh|\(current.sessionId)|\(timestamp)".utf8)).rawRepresentation.base64URL
        let body = try JSONSerialization.data(withJSONObject: ["sessionId": current.sessionId, "refreshToken": current.refreshToken,
                                                              "timestamp": timestamp, "signature": signature])
        do {
            let renewed = try await call("/v1/auth/refresh", body: body)
            guard session?.sessionId == current.sessionId else { throw AccountFailure.signedOut }
            return try store(renewed, signedInAt: current.signedInAt)
        } catch PairingFailure.response(401) {
            // The service no longer knows this session (deleted elsewhere, expired).
            if session?.sessionId == current.sessionId { end() }
            throw AccountFailure.signedOut
        }
    }

    // MARK: Sign out and delete

    /// Turns notifications off for every Mac, then ends the session. It ends on
    /// this iPhone even offline; the service drops it when it next sees it, or
    /// when the refresh token expires.
    func signOut() async {
        guard session != nil, !working else { return }
        working = true; error = nil
        defer { working = false }
        let token = try? await accessToken()
        end()
        if let token { _ = try? await post("/v1/auth/signout", Data("{}".utf8), token) }
    }

    /// Deletes the account and everything the service holds for it. Stays
    /// signed in, with an error, if the service could not confirm it.
    @discardableResult func deleteAccount() async -> Bool {
        guard session != nil, !working else { return false }
        working = true; error = nil
        defer { working = false }
        do {
            let token = try await accessToken()
            _ = try await post("/v1/account/delete", Data("{}".utf8), token)
            end()
            return true
        } catch AccountFailure.signedOut {
            return true
        } catch {
            self.error = "Your account wasn't deleted. Check your connection and try again."
            return false
        }
    }

    /// A one-time grant to turn on Reach from anywhere for the computer with
    /// this host key (the raw point, base64url). It expires in ten minutes.
    func relayGrant(hostKey: String) async throws -> (grant: String, label: String) {
        struct Grant: Decodable { let grant, label: String }
        let body = try JSONSerialization.data(withJSONObject: ["hostKey": hostKey])
        let reply = try JSONDecoder().decode(Grant.self, from: try await post("/v1/relay/grants", body, try await accessToken()))
        return (reply.grant, reply.label)
    }

    private func end() {
        ended()
        session = nil; refreshing?.cancel(); refreshing = nil
        try? remove(Self.sessionAccount)
    }

    // MARK: Storage

    private struct Tokens: Decodable { let sessionId, accessToken, refreshToken: String; let accessExpiresIn, refreshExpiresIn: Double }
    private func call(_ path: String, body: Data) async throws -> Tokens {
        try JSONDecoder().decode(Tokens.self, from: try await post(path, body, nil))
    }
    @discardableResult private func store(_ tokens: Tokens, signedInAt: Date = Date()) throws -> AccountSession {
        let now = Date()
        let value = AccountSession(sessionId: tokens.sessionId, accessToken: tokens.accessToken, accessExpiry: now.addingTimeInterval(tokens.accessExpiresIn),
                                   refreshToken: tokens.refreshToken, refreshExpiry: now.addingTimeInterval(tokens.refreshExpiresIn), signedInAt: signedInAt)
        do { try write(JSONEncoder().encode(value), Self.sessionAccount) } catch { throw AccountFailure.storage }
        session = value
        return value
    }
    /// One key per installation, kept only in this device's Keychain. Sessions
    /// are bound to it, so a copied refresh token is useless elsewhere.
    private func deviceKey() throws -> P256.Signing.PrivateKey {
        do {
            if let data = try read(Self.keyAccount) { return try P256.Signing.PrivateKey(rawRepresentation: data) }
            let key = P256.Signing.PrivateKey()
            try write(key.rawRepresentation, Self.keyAccount)
            return key
        } catch { throw AccountFailure.storage }
    }
    private static let urlSession = URLSession(configuration: .ephemeral)
    private static func network(_ origin: String) -> Post {
        { path, body, bearer in
            guard let url = URL(string: origin + path) else { throw AccountFailure.unavailable }
            var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 15)
            request.httpMethod = "POST"; request.httpBody = body
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            if let bearer { request.setValue("Bearer \(bearer)", forHTTPHeaderField: "Authorization") }
            let (data, response) = try await urlSession.data(for: request)
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            guard (200..<300).contains(status) else { throw PairingFailure.response(status) }
            return data
        }
    }
    private static func randomToken() -> String {
        var bytes = [UInt8](repeating: 0, count: 32)
        _ = SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes)
        return Data(bytes).base64URL
    }
}

#if WONDER_DIAGNOSTICS
extension WonderAccount {
    struct Fixture {
        let read: (String) throws -> Data?
        let write: (Data, String) throws -> Void
        let remove: (String) throws -> Void
        let post: Post
        var service: String?

        /// In-memory storage and a local stand-in for the service: nothing
        /// reaches Apple, Cloudflare or this device's Keychain.
        static func local(signedIn: Bool, service: String? = nil) -> Fixture {
            final class Box: @unchecked Sendable { var values: [String: Data] = [:] }
            let box = Box()
            if signedIn {
                let now = Date()
                box.values[WonderAccount.sessionAccount] = try? JSONEncoder().encode(AccountSession(
                    sessionId: "00000000-0000-4000-8000-00000000a11c", accessToken: "fixture-access", accessExpiry: now.addingTimeInterval(3600),
                    refreshToken: "fixture-refresh", refreshExpiry: now.addingTimeInterval(90 * 86400), signedInAt: now))
            }
            return Fixture(read: { box.values[$0] }, write: { box.values[$1] = $0 }, remove: { box.values[$0] = nil }, post: { path, _, _ in
                switch path {
                case "/v1/relay/grants":
                    return Data(#"{"grant":"fixture-grant","label":"aaaaaaaaaaaaaaaaaaaaaaaaaa","expiresIn":600}"#.utf8)
                case "/v1/auth/apple", "/v1/auth/refresh":
                    return Data(#"{"sessionId":"00000000-0000-4000-8000-00000000a11c","accessToken":"fixture-access","refreshToken":"fixture-refresh","accessExpiresIn":3600,"refreshExpiresIn":7776000}"#.utf8)
                default:
                    return Data("{}".utf8)
                }
            }, service: service)
        }
    }
    /// `-diagnostics-account-fixture` starts signed out (add
    /// `-diagnostics-account-signed-in` to start signed in) against the local
    /// stand-in. The unit-test host is always signed in to that stand-in, so
    /// push tests exercise setup rather than the sign-in gate.
    static var diagnosticFixture: Fixture? {
        let arguments = ProcessInfo.processInfo.arguments
        if arguments.contains("-diagnostics-account-fixture") { return .local(signedIn: arguments.contains("-diagnostics-account-signed-in")) }
        // Matches the push service the existing push tests register with.
        if ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil { return .local(signedIn: true, service: "https://push.example.test") }
        return nil
    }
    /// Scenarios cannot drive Apple's sheet; the fixture signs in directly.
    static var usesFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-account-fixture") }
    func signInWithFixture() async {
        nonce = "fixture"
        await signIn(identityToken: "fixture")
    }
}
#endif
