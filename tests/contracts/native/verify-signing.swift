// Standalone CryptoKit interoperability check; no app identity or real keys.
import Foundation
import CryptoKit

func unhex(_ value: String) -> Data {
    precondition(value.count % 2 == 0)
    var data = Data()
    var index = value.startIndex
    while index < value.endIndex {
        let end = value.index(index, offsetBy: 2)
        data.append(UInt8(value[index..<end], radix: 16)!)
        index = end
    }
    return data
}
func base64url(_ value: String) -> Data {
    var text = value.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
    text += String(repeating: "=", count: (4 - text.count % 4) % 4)
    return Data(base64Encoded: text)!
}
struct Fixture: Decodable {
    let fixtureVersion: Int
    let testPrivateKeyHex: String
    let publicKeyX963Hex: String
    let fixtures: [Vector]
    struct Vector: Decodable { let transcriptHex: String; let signature: String }
}
let fixture = try JSONDecoder().decode(Fixture.self, from: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1])))
precondition(fixture.fixtureVersion == 1)
let key = try P256.Signing.PrivateKey(rawRepresentation: unhex(fixture.testPrivateKeyHex))
let publicKey = try P256.Signing.PublicKey(x963Representation: unhex(fixture.publicKeyX963Hex))
let transcripts = [
    ["wonder-session-v1", "device-01", "challenge-01", "nonce-01", "https://wonder.example.ts.net", "install-1", String(UInt64(1000)), String(UInt64(61000))],
    ["wonder-action-v1", "approval.resolve", "/api/v1/approvals/approval-1/resolve", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "nonce-1", "session-1", "device-01", "install-1", String(UInt64(1000)), "pending"]
].map { Data($0.joined(separator: "\n").utf8) }
precondition(fixture.fixtures.count == transcripts.count)
var signatures: [String] = []
for (bytes, vector) in zip(transcripts, fixture.fixtures) {
    precondition(bytes == unhex(vector.transcriptHex), "Swift/Rust transcript mismatch")
    let signature = try P256.Signing.ECDSASignature(rawRepresentation: base64url(vector.signature))
    precondition(publicKey.isValidSignature(signature, for: bytes), "Rust signature rejected")
    var changed = bytes; changed.append(0)
    precondition(!publicKey.isValidSignature(signature, for: changed), "Modified transcript accepted")
    signatures.append(try key.signature(for: bytes).rawRepresentation.base64EncodedString()
        .replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: ""))
}
try JSONEncoder().encode(signatures).write(to: URL(fileURLWithPath: CommandLine.arguments[2]))
print("Swift matched both transcripts, verified Rust signatures, and rejected modified bytes")
