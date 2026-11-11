import CryptoKit
import XCTest
@testable import WonderPairing

/// The phone's half of the relay's sealed channel, against the host's own
/// vectors (`tests/contracts/native/sealed-v1.json`, checked by wonderd's
/// `the_client_test_vectors_match_this_channel`).
final class SealedChannelTests: XCTestCase {
    struct Vector: Decodable {
        struct Frame: Decodable { let direction, plaintext, sealed: String }
        let version, origin, otherOrigin, clientPrivate, serverPrivate, clientKey, serverKey, sharedSecret: String
        let handshakeNonce, hello, hostKeyPin, welcome, clientToHostKey, hostToClientKey: String
        let frames: [Frame]
    }

    static func vector() throws -> Vector {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        return try JSONDecoder().decode(Vector.self, from: Data(contentsOf: root.appendingPathComponent("tests/contracts/native/sealed-v1.json")))
    }

    static func hex(_ text: String) -> Data {
        Data(stride(from: 0, to: text.count, by: 2).map { offset in
            let start = text.index(text.startIndex, offsetBy: offset)
            return UInt8(text[start..<text.index(start, offsetBy: 2)], radix: 16)!
        })
    }

    private func initiator(_ vector: Vector) throws -> SealedInitiator {
        SealedInitiator(privateKey: try .init(rawRepresentation: Self.hex(vector.clientPrivate)))
    }

    func testX25519KeyScheduleAndHelloMatchTheHost() throws {
        let vector = try Self.vector()
        XCTAssertEqual(vector.version, SealedInitiator.version)
        let client = try initiator(vector)
        let server = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: Self.hex(vector.serverPrivate))
        XCTAssertEqual(client.clientKey.base64URL, vector.clientKey)
        XCTAssertEqual(server.publicKey.rawRepresentation.base64URL, vector.serverKey)
        let secret = try client.privateKey.sharedSecretFromKeyAgreement(with: server.publicKey).withUnsafeBytes { Data($0) }
        XCTAssertEqual(secret, Self.hex(vector.sharedSecret))
        XCTAssertEqual(client.hello, vector.hello)
        let serverKey = server.publicKey.rawRepresentation
        XCTAssertEqual(SealedInitiator.handshakeNonce(clientKey: client.clientKey, serverKey: serverKey), vector.handshakeNonce)
        let keys = SealedInitiator.schedule(secret: secret, clientKey: client.clientKey, serverKey: serverKey)
        XCTAssertEqual(keys.toHost.withUnsafeBytes { Data($0) }, Self.hex(vector.clientToHostKey))
        XCTAssertEqual(keys.toClient.withUnsafeBytes { Data($0) }, Self.hex(vector.hostToClientKey))
    }

    func testTheHostsWelcomeOpensAChannelWhoseFramesMatchTheHosts() throws {
        let vector = try Self.vector()
        var channel = try initiator(vector).finish(welcome: vector.welcome, pin: vector.hostKeyPin, origin: vector.origin)
        for frame in vector.frames {
            let sealed = try HostProof.decode(frame.sealed)
            if frame.direction == "clientToHost" {
                XCTAssertEqual(try channel.seal(Data(frame.plaintext.utf8)), sealed, frame.plaintext)
            } else {
                XCTAssertEqual(try channel.open(sealed), Data(frame.plaintext.utf8))
            }
        }
    }

    func testAnotherKeyOriginOrHandshakeFailsThePin() throws {
        let vector = try Self.vector()
        let client = try initiator(vector)
        // The proof names the relay origin the phone dialed, not another one.
        XCTAssertThrowsError(try client.finish(welcome: vector.welcome, pin: vector.hostKeyPin, origin: vector.otherOrigin)) {
            XCTAssertEqual($0 as? SealedChannelFailure, .hostProof)
        }
        // A computer with another key: the pin from pairing does not match.
        let otherPin = Data(SHA256.hash(data: P256.Signing.PrivateKey().publicKey.derRepresentation)).base64URL
        XCTAssertThrowsError(try client.finish(welcome: vector.welcome, pin: otherPin, origin: vector.origin)) {
            XCTAssertEqual($0 as? SealedChannelFailure, .hostProof)
        }
        // A welcome recorded for another client's key signs other keys.
        XCTAssertThrowsError(try SealedInitiator().finish(welcome: vector.welcome, pin: vector.hostKeyPin, origin: vector.origin)) {
            XCTAssertEqual($0 as? SealedChannelFailure, .hostProof)
        }
        for welcome in ["", "{}", #"{"serverKey":"short","proof":{"hostKey":"a","signature":"b"}}"#, String(repeating: "x", count: 5000)] {
            XCTAssertThrowsError(try client.finish(welcome: welcome, pin: vector.hostKeyPin, origin: vector.origin)) {
                XCTAssertEqual($0 as? SealedChannelFailure, .handshake)
            }
        }
    }

    func testAlteredReorderedReplayedOrShortFramesDoNotOpen() throws {
        let vector = try Self.vector()
        let reply = try HostProof.decode(vector.frames.first { $0.direction == "hostToClient" }!.sealed)
        func fresh() throws -> SealedChannel {
            try initiator(vector).finish(welcome: vector.welcome, pin: vector.hostKeyPin, origin: vector.origin)
        }
        for bit in [0, reply.count * 8 - 1] {
            var channel = try fresh(), altered = reply
            altered[bit / 8] ^= 1 << (bit % 8)
            XCTAssertThrowsError(try channel.open(altered)) { XCTAssertEqual($0 as? SealedChannelFailure, .frame) }
        }
        var channel = try fresh()
        XCTAssertThrowsError(try channel.open(reply.prefix(SealedChannel.tagBytes - 1)))
        XCTAssertEqual(try channel.open(reply), Data(#"{"jsonrpc":"2.0","id":1,"result":{}}"#.utf8))
        XCTAssertThrowsError(try channel.open(reply), "replayed") { XCTAssertEqual($0 as? SealedChannelFailure, .frame) }
        // A frame of ours reflected back does not open as the host's.
        var reflected = try fresh()
        let own = try reflected.seal(Data("x".utf8))
        XCTAssertThrowsError(try reflected.open(own))
    }

    func testTheRelayAddressShapeIsRecognized() {
        XCTAssertTrue(SealedRoutes.looksLikeRelay("https://r-abcdefghijklmnopqrstuvwxyz.getcupola.app"))
        XCTAssertTrue(SealedRoutes.looksLikeRelay("https://r-a234567abcdefghijklmnopqrs.example.org"))
        XCTAssertFalse(SealedRoutes.looksLikeRelay("https://r-short.getcupola.app"))
        XCTAssertFalse(SealedRoutes.looksLikeRelay("https://studio.tail000000.ts.net"))
        XCTAssertFalse(SealedRoutes.looksLikeRelay("https://192.168.1.4:3778"))
        XCTAssertFalse(SealedRoutes.looksLikeRelay("https://r-ABCDEFGHIJKLMNOPQRSTUVWXYZ.getcupola.app"))
    }
}
