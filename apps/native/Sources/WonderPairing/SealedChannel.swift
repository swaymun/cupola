import CryptoKit
import Foundation

/// A host's signature, with the key it pinned at pairing, over one origin and
/// nonce (`wonder-host-proof-v1`). `/api/v1/host/proof` and the sealed
/// handshake both answer with one.
public struct HostProof: Codable, Sendable {
    public let hostKey: String
    public let signature: String

    public init(hostKey: String, signature: String) { self.hostKey = hostKey; self.signature = signature }

    /// Throws `PairingFailure.wrongHost` unless the pinned key signed this origin and nonce.
    public func validate(pin: String, origin: String, nonce: String) throws {
        let spki = try Self.decode(hostKey)
        guard Data(SHA256.hash(data: spki)).base64URL == pin else { throw PairingFailure.wrongHost }
        let key = try P256.Signing.PublicKey(derRepresentation: spki)
        let signature = try P256.Signing.ECDSASignature(rawRepresentation: Self.decode(signature))
        let transcript = Data("wonder-host-proof-v1\n\(origin)\n\(nonce)".utf8)
        guard key.isValidSignature(signature, for: transcript) else { throw PairingFailure.wrongHost }
    }

    /// The host key as the relay service names it: the raw X9.63 point, base64url.
    public var rawHostKey: String? {
        guard let spki = try? Self.decode(hostKey),
              let key = try? P256.Signing.PublicKey(derRepresentation: spki) else { return nil }
        return key.x963Representation.base64URL
    }

    static func decode(_ value: String) throws -> Data {
        let padded = value.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        guard let data = Data(base64Encoded: padded + String(repeating: "=", count: (4 - padded.count % 4) % 4)) else {
            throw PairingFailure.wrongHost
        }
        return data
    }
}

public enum SealedChannelFailure: Error, Equatable, Sendable {
    /// Not a handshake this version reads.
    case handshake
    /// The host proof is not from the pinned key for this origin.
    case hostProof
    /// A frame failed to open: altered, out of order, replayed or cut short.
    case frame
    /// A direction ran out of frame numbers; open a new connection.
    case exhausted
}

/// The phone's half of `wonder-sealed-v1` (`crates/wonderd/src/rpc/sealed.rs`):
/// X25519 with a fresh key per connection, the host's pinned-key proof over
/// both keys and the dialed origin, then ChaCha20-Poly1305 per direction.
/// The relay edge sees the two handshake messages and opaque frames only.
public struct SealedInitiator: Sendable {
    public static let version = "wonder-sealed-v1"
    static let maxHandshakeBytes = 4 * 1024
    let privateKey: Curve25519.KeyAgreement.PrivateKey

    public init(privateKey: Curve25519.KeyAgreement.PrivateKey = .init()) { self.privateKey = privateKey }

    public var clientKey: Data { privateKey.publicKey.rawRepresentation }

    /// The first text message: `{"v":"wonder-sealed-v1","clientKey":…}`.
    public var hello: String {
        #"{"v":"\#(Self.version)","clientKey":"\#(clientKey.base64URL)"}"#
    }

    /// Checks the welcome's proof against `pin` and `origin`, then opens the channel.
    public func finish(welcome: String, pin: String, origin: String) throws -> SealedChannel {
        struct Welcome: Decodable { let serverKey: String; let proof: HostProof }
        guard welcome.utf8.count <= Self.maxHandshakeBytes,
              let parsed = try? JSONDecoder().decode(Welcome.self, from: Data(welcome.utf8)),
              let serverKey = try? HostProof.decode(parsed.serverKey), serverKey.count == 32,
              let peer = try? Curve25519.KeyAgreement.PublicKey(rawRepresentation: serverKey) else {
            throw SealedChannelFailure.handshake
        }
        let nonce = Self.handshakeNonce(clientKey: clientKey, serverKey: serverKey)
        do { try parsed.proof.validate(pin: pin, origin: origin, nonce: nonce) }
        catch { throw SealedChannelFailure.hostProof }
        guard let shared = try? privateKey.sharedSecretFromKeyAgreement(with: peer) else { throw SealedChannelFailure.handshake }
        let secret = shared.withUnsafeBytes { Data($0) }
        // An all-zero secret means a small-order key; the host refuses it too.
        guard secret.contains(where: { $0 != 0 }) else { throw SealedChannelFailure.handshake }
        let keys = Self.schedule(secret: secret, clientKey: clientKey, serverKey: serverKey)
        return SealedChannel(outgoing: keys.toHost, incoming: keys.toClient)
    }

    static func transcriptHash(clientKey: Data, serverKey: Data) -> Data {
        var hash = SHA256()
        hash.update(data: Data("\(version)\n".utf8))
        hash.update(data: clientKey)
        hash.update(data: serverKey)
        return Data(hash.finalize())
    }

    /// The nonce the host proof signs: base64url SHA-256 of the transcript.
    public static func handshakeNonce(clientKey: Data, serverKey: Data) -> String {
        transcriptHash(clientKey: clientKey, serverKey: serverKey).base64URL
    }

    /// `(client to host, host to client)` keys: HKDF-SHA256 over the X25519
    /// secret, salted with the transcript hash.
    static func schedule(secret: Data, clientKey: Data, serverKey: Data) -> (toHost: SymmetricKey, toClient: SymmetricKey) {
        let salt = transcriptHash(clientKey: clientKey, serverKey: serverKey)
        func key(_ info: String) -> SymmetricKey {
            HKDF<SHA256>.deriveKey(inputKeyMaterial: SymmetricKey(data: secret), salt: salt,
                                   info: Data(info.utf8), outputByteCount: 32)
        }
        return (key("\(version) client to host"), key("\(version) host to client"))
    }
}

/// The host's half of the handshake, as `rpc/sealed.rs` runs it: a stand-in
/// computer answers the phone's hello with a welcome proven by `hostKey`.
public enum SealedResponder {
    public static func answer(hello: String, hostKey: P256.Signing.PrivateKey, origin: String) throws -> (welcome: String, channel: SealedChannel) {
        struct Hello: Decodable { let v: String; let clientKey: String }
        guard let parsed = try? JSONDecoder().decode(Hello.self, from: Data(hello.utf8)), parsed.v == SealedInitiator.version,
              let clientKey = try? HostProof.decode(parsed.clientKey),
              let peer = try? Curve25519.KeyAgreement.PublicKey(rawRepresentation: clientKey) else { throw SealedChannelFailure.handshake }
        let server = Curve25519.KeyAgreement.PrivateKey()
        let serverKey = server.publicKey.rawRepresentation
        let nonce = SealedInitiator.handshakeNonce(clientKey: clientKey, serverKey: serverKey)
        let signature = try hostKey.signature(for: Data("wonder-host-proof-v1\n\(origin)\n\(nonce)".utf8)).rawRepresentation
        let secret = try server.sharedSecretFromKeyAgreement(with: peer).withUnsafeBytes { Data($0) }
        let keys = SealedInitiator.schedule(secret: secret, clientKey: clientKey, serverKey: serverKey)
        let welcome = try JSONSerialization.data(withJSONObject: ["serverKey": serverKey.base64URL,
            "proof": ["hostKey": hostKey.publicKey.derRepresentation.base64URL, "signature": signature.base64URL]])
        return (String(decoding: welcome, as: UTF8.self), SealedChannel(outgoing: keys.toClient, incoming: keys.toHost))
    }
}

/// One connection's two directions. Each frame's nonce is its position in its
/// direction, so frames must open in the order they were sealed; after a
/// failure the connection is unusable.
public struct SealedChannel: Sendable {
    public static let tagBytes = 16
    private let outgoing: SymmetricKey, incoming: SymmetricKey
    private var sent: UInt64 = 0, received: UInt64 = 0

    init(outgoing: SymmetricKey, incoming: SymmetricKey) { self.outgoing = outgoing; self.incoming = incoming }

    private static func nonce(_ counter: UInt64) throws -> ChaChaPoly.Nonce {
        var bytes = Data(count: 4)
        withUnsafeBytes(of: counter.bigEndian) { bytes.append(contentsOf: $0) }
        return try ChaChaPoly.Nonce(data: bytes)
    }

    public mutating func seal(_ plaintext: Data) throws -> Data {
        guard sent < .max else { throw SealedChannelFailure.exhausted }
        let box = try ChaChaPoly.seal(plaintext, using: outgoing, nonce: Self.nonce(sent))
        sent += 1
        return Data(box.ciphertext) + box.tag
    }

    public mutating func open(_ frame: Data) throws -> Data {
        guard received < .max else { throw SealedChannelFailure.exhausted }
        guard frame.count >= Self.tagBytes else { throw SealedChannelFailure.frame }
        let split = frame.endIndex - Self.tagBytes
        do {
            let box = try ChaChaPoly.SealedBox(nonce: Self.nonce(received), ciphertext: frame[frame.startIndex..<split], tag: frame[split...])
            let plaintext = try ChaChaPoly.open(box, using: incoming)
            received += 1
            return plaintext
        } catch { throw SealedChannelFailure.frame }
    }
}
