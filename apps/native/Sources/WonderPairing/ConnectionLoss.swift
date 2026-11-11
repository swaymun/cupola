import Foundation
import CryptoKit
import Security

/// Why this device can't use a paired computer, in words the owner can act
/// on. Each case says what happened and what fixes it; an unreachable
/// computer is not a loss and keeps its own retry wording.
public enum ConnectionLoss: Equatable, Sendable {
    /// The computer answered with another security key than the one this
    /// device paired with, so nothing was sent to it.
    case keyChanged
    /// The computer refuses this device: disconnected there, or its Cupola
    /// data was reset.
    case accessEnded
    /// This device's own pairing key is gone.
    case deviceKeyLost

    public func message(computer: String) -> String {
        switch self {
        case .keyChanged:
            "\(computer) answered with a different security key than when you paired, so Cupola sent it nothing. If Cupola was reinstalled or reset on it, pair again. Otherwise, check that you’re on your own network."
        case .accessEnded:
            "\(computer) no longer accepts this device. It was disconnected there, or Cupola’s data on it was reset. Pair again to reconnect."
        case .deviceKeyLost:
            "This device’s pairing key is gone, which happens after restoring from a backup or moving to a new device. Pair again from \(computer)."
        }
    }
}

/// Why the computers saved on this device couldn't be opened.
public enum SavedComputersFailure: Equatable, Sendable {
    /// Protected until the device is unlocked; retrying after unlock works.
    case locked
    /// Stored, but in a form this version can't read. Retrying won't help.
    case unreadable
    /// Any other storage failure.
    case unavailable

    public init(_ error: Error) {
        if error is DecodingError { self = .unreadable; return }
        if case SigningIdentityFailure.keychain(let status) = error, status == errSecInteractionNotAllowed {
            self = .locked; return
        }
        self = .unavailable
    }

    public var message: String {
        switch self {
        case .locked:
            "Your saved computers open once this device is unlocked. Unlock it, then try again."
        case .unreadable:
            "Cupola couldn’t read the computers saved on this device. Your chats are still on your computers. Remove the saved computers, then pair again."
        case .unavailable:
            "Your saved computers couldn’t be opened. Restart this device, then try again."
        }
    }
}

/// A signed owner action (`wonder-action-v1`), as the host checks it in
/// `verify_signed_action`. `values` are hashed in the order the host hashes them.
public enum SignedAction {
    public static func transcript(action: String, path: String, values: [Any], nonce: String,
                                  connection: SavedConnection, issuedAtMs: UInt64, expectedState: String = "active") throws -> Data {
        let canonical = try JSONSerialization.data(withJSONObject: values, options: [.withoutEscapingSlashes])
        return transcript(action: action, path: path, hash: canonical, nonce: nonce, connection: connection,
                          issuedAtMs: issuedAtMs, expectedState: expectedState)
    }

    /// The same for an object body, hashed with sorted keys as the host's
    /// `serde_json` writes it.
    public static func transcript(action: String, path: String, object: [String: Any], nonce: String,
                                  connection: SavedConnection, issuedAtMs: UInt64, expectedState: String = "active") throws -> Data {
        let canonical = try JSONSerialization.data(withJSONObject: object, options: [.withoutEscapingSlashes, .sortedKeys])
        return transcript(action: action, path: path, hash: canonical, nonce: nonce, connection: connection,
                          issuedAtMs: issuedAtMs, expectedState: expectedState)
    }

    private static func transcript(action: String, path: String, hash canonical: Data, nonce: String,
                                   connection: SavedConnection, issuedAtMs: UInt64, expectedState: String) -> Data {
        let hash = SHA256.hash(data: canonical).map { String(format: "%02x", $0) }.joined()
        return Data([
            "wonder-action-v1", action, path, hash, nonce, connection.credential.csrfToken,
            connection.credential.deviceId, connection.credential.hostInstallationId, String(issuedAtMs), expectedState,
        ].joined(separator: "\n").utf8)
    }

    public static func body(fields: [String: Any] = [:], nonce: String, issuedAtMs: UInt64, signature: String) throws -> Data {
        var body = fields
        body["actionNonce"] = nonce; body["issuedAtMs"] = issuedAtMs; body["signature"] = signature
        return try JSONSerialization.data(withJSONObject: body)
    }
}

/// This device's hardware model ("iPhone17,2"), so the computer's device list
/// can tell phones apart when their names are all "iPhone".
public enum DeviceModel {
    public static let identifier: String = {
        if let simulated = ProcessInfo.processInfo.environment["SIMULATOR_MODEL_IDENTIFIER"], !simulated.isEmpty { return simulated }
        var value = utsname(); uname(&value)
        return withUnsafeBytes(of: &value.machine) { String(decoding: $0.prefix { $0 != 0 }, as: UTF8.self) }
    }()
}
