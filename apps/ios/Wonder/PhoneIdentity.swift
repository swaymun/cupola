import Foundation
import CryptoKit
import Security
import WonderPairing

/// Keychain storage for the phone's pairing identity and saved connections.
/// Shared with the Share extension, which reads the saved connections only.
struct PhoneIdentity: Sendable {
    private let service = "com.saimun.wonder.native"
    func read(_ account: String) throws -> Data? {
        let query: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: account, kSecReturnData as String: true]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else { throw SigningIdentityFailure.keychain(status) }
        return data
    }
    func save(_ data: Data, account: String) throws {
        let query: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: account]
        let update = SecItemUpdate(query as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        if update == errSecItemNotFound {
            var item = query
            item[kSecValueData as String] = data
            item[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
            let status = SecItemAdd(item as CFDictionary, nil)
            guard status == errSecSuccess else { throw SigningIdentityFailure.keychain(status) }
        } else if update != errSecSuccess { throw SigningIdentityFailure.keychain(update) }
    }
    func delete(_ account: String) throws {
        let status = SecItemDelete([kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: account] as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw SigningIdentityFailure.keychain(status) }
    }
    func forgetConnection() throws {
        let status = SecItemDelete([kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: "connection"] as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw PairingFailure.missingIdentity }
    }
    static let signing: SigningIdentity = {
        let storage = PhoneIdentity()
        #if targetEnvironment(simulator)
        // Only simulator builds use a software key.
        let account = "simulator-key"
        @Sendable func wrap(_ key: P256.Signing.PrivateKey) -> EnrollmentSigningIdentity {
            EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation, sign: { try key.signature(for: $0) })
        }
        return SigningIdentity(read: { try storage.read(account) }, save: { try storage.save($0, account: account) },
                               restore: { try wrap(P256.Signing.PrivateKey(rawRepresentation: $0)) }, create: { wrap(P256.Signing.PrivateKey()) })
        #else
        let account = "enclave-key"
        @Sendable func wrap(_ key: SecureEnclave.P256.Signing.PrivateKey) -> EnrollmentSigningIdentity {
            EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.dataRepresentation, sign: { try key.signature(for: $0) })
        }
        return SigningIdentity(read: { try storage.read(account) }, save: { try storage.save($0, account: account) }, restore: {
            try wrap(SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: $0))
        }, create: {
            guard let access = SecAccessControlCreateWithFlags(nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, [.privateKeyUsage], nil) else {
                throw SigningIdentityFailure.keychain(errSecParam)
            }
            return try wrap(SecureEnclave.P256.Signing.PrivateKey(accessControl: access))
        })
        #endif
    }()
}
