import CryptoKit
import XCTest
@testable import WonderPairing

final class RemoteAccessTests: XCTestCase {

    /// The computer hashes `{"grant","label"}` as serde_json writes it (keys
    /// sorted, no spaces) and expects the relay to be `allowed`.
    func testTheEnableSignatureCoversTheGrantTheComputerChecks() throws {
        let credential = try JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"s","deviceId":"phone","csrfToken":"csrf","hostInstallationId":"host"}"#.utf8))
        let saved = SavedConnection(origin: "https://studio.invalid", credential: credential)
        let transcript = try RelayEnable.transcript(grant: "g/1", label: "k7xq2abcdefghijklmnopqrstu", nonce: "n",
                                                    connection: saved, issuedAtMs: 7)
        let hash = SHA256.hash(data: Data(#"{"grant":"g/1","label":"k7xq2abcdefghijklmnopqrstu"}"#.utf8)).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(String(decoding: transcript, as: UTF8.self),
                       ["wonder-action-v1", "relay.enable", "/api/v1/relay/enable", hash, "n", "csrf", "phone", "host", "7", "allowed"].joined(separator: "\n"))
        let body = try JSONSerialization.jsonObject(with: RelayEnable.body(grant: "g/1", label: "l", nonce: "n", issuedAtMs: 7, signature: "sig")) as? [String: Any]
        XCTAssertEqual(Set(body?.keys.map { $0 } ?? []), ["grant", "label", "actionNonce", "issuedAtMs", "signature"])
    }

    /// The relay service wants the host key as its raw point, which the phone
    /// reads from the host's pinned proof.
    func testTheProofsKeyBecomesTheRawPointTheRelayServiceNames() throws {
        let vector = try SealedChannelTests.vector()
        struct Welcome: Decodable { let proof: HostProof }
        let proof = try JSONDecoder().decode(Welcome.self, from: Data(vector.welcome.utf8)).proof
        let raw = try XCTUnwrap(proof.rawHostKey)
        XCTAssertEqual(raw.count, 87)
        XCTAssertEqual(try HostProof.decode(raw).first, 0x04)
    }
}
