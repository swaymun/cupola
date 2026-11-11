import Foundation
import CryptoKit

/// A synchronous publication fence closes the actor-to-main-actor handoff:
/// rotation invalidates enrollments before its durable marker callback runs.
private final class SigningIdentityRevision: @unchecked Sendable {
    private let lock = NSLock()
    private var revision = UUID()
    func current() -> UUID { lock.withLock { revision } }
    func advance() { lock.withLock { revision = UUID() } }
}

public enum SigningIdentityFailure: LocalizedError {
    case missing, invalidated, keychain(Int32), verificationFailed

    public var errorDescription: String? {
        switch self {
        case .missing, .invalidated:
            "This device’s connection needs to be set up again. Pair again from your computer."
        case .keychain:
            "Your saved connection could not be opened or saved. Unlock this device and try again."
        case .verificationFailed:
            "This device could not verify its connection key. Try again."
        }
    }

    /// Check only the documented permanent failure, including wrapped OS errors.
    /// The numeric value keeps deployment targets older than iOS 27 supported.
    public static func requiresPairing(_ error: Error) -> Bool {
        if let failure = error as? Self {
            switch failure { case .missing, .invalidated: return true; case .keychain, .verificationFailed: return false }
        }
        var current = error as NSError
        for _ in 0..<8 {
            if current.domain == "CryptoTokenKit", current.code == -10 { return true }
            guard let underlying = current.userInfo[NSUnderlyingErrorKey] as? NSError else { break }
            current = underlying
        }
        return false
    }
}

/// A single key retained from preflight through enrollment. Its representation
/// is an encrypted enclave blob on devices, never an exported private key.
public struct EnrollmentSigningIdentity: Sendable {
    public let publicKey: PublicKey
    public let representation: Data
    private let verificationKey: P256.Signing.PublicKey
    private let signData: @Sendable (Data) throws -> P256.Signing.ECDSASignature
    private var isCurrent: @Sendable () -> Bool = { true }

    public init(publicKey: P256.Signing.PublicKey, representation: Data,
                sign: @escaping @Sendable (Data) throws -> P256.Signing.ECDSASignature) {
        self.publicKey = PublicKey(publicKey)
        self.verificationKey = publicKey
        self.representation = representation
        self.signData = sign
    }

    public func signature(for data: Data) throws -> String {
        try checkCurrent()
        return try signData(data).rawRepresentation.base64URL
    }

    public func checkCurrent() throws {
        guard isCurrent() else { throw CancellationError() }
    }

    fileprivate func bound(to revision: SigningIdentityRevision) -> Self {
        var result = self
        let expected = revision.current()
        result.isCurrent = { revision.current() == expected }
        return result
    }

    fileprivate func preflight() throws {
        let challenge = Data(("wonder-identity-preflight-v1:" + UUID().uuidString).utf8)
        guard verificationKey.isValidSignature(try signData(challenge), for: challenge) else {
            throw SigningIdentityFailure.verificationFailed
        }
    }
}

/// The actor keeps enclave work off the main actor. Concurrent pairing screens
/// share one preparation; normal signing can never create or replace an identity.
public actor SigningIdentity {
    private let read: @Sendable () throws -> Data?
    private let save: @Sendable (Data) throws -> Void
    private let restore: @Sendable (Data) throws -> EnrollmentSigningIdentity
    private let create: @Sendable () throws -> EnrollmentSigningIdentity
    private var preparation: Task<EnrollmentSigningIdentity, Error>?
    private let revision = SigningIdentityRevision()

    public init(read: @escaping @Sendable () throws -> Data?,
                save: @escaping @Sendable (Data) throws -> Void,
                restore: @escaping @Sendable (Data) throws -> EnrollmentSigningIdentity,
                create: @escaping @Sendable () throws -> EnrollmentSigningIdentity) {
        self.read = read; self.save = save; self.restore = restore; self.create = create
    }

    public func sign(_ data: Data) async throws -> String {
        if let preparation { _ = try await preparation.value }
        guard let representation = try read() else { throw SigningIdentityFailure.missing }
        return try restore(representation).signature(for: data)
    }

    public func sign(_ data: Data, using identity: EnrollmentSigningIdentity) throws -> String {
        try identity.signature(for: data)
    }

    public func prepareForEnrollment(beforeReplacing: @escaping @Sendable () async throws -> Void) async throws -> EnrollmentSigningIdentity {
        try Task.checkCancellation()
        if let preparation { return try await preparation.value }
        let task = Task { try await self.prepare(beforeReplacing: beforeReplacing) }
        preparation = task
        defer { preparation = nil }
        return try await task.value
    }

    public func validateCurrent(_ identity: EnrollmentSigningIdentity) async throws {
        if let preparation { _ = try await preparation.value }
        try identity.checkCurrent()
        guard try read() == identity.representation else { throw CancellationError() }
    }

    private func prepare(beforeReplacing: @Sendable () async throws -> Void) async throws -> EnrollmentSigningIdentity {
        if let representation = try read() {
            do {
                let key = try restore(representation)
                try key.preflight()
                return key.bound(to: revision)
            } catch {
                guard SigningIdentityFailure.requiresPairing(error) else { throw error }
            }
        }
        let replacement = try create()
        try replacement.preflight()
        revision.advance()
        // Persist reconnect markers and stop old generations before changing the
        // single shared key. A failed callback or save leaves old data intact.
        try await beforeReplacing()
        try save(replacement.representation)
        return replacement.bound(to: revision)
    }
}
