import Foundation

/// The computer's `GET /api/v1/relay`: the owner's choice on the computer and
/// the relay's state.
public struct RelayStatus: Decodable, Equatable, Sendable {
    public let allowed: Bool
    public let enabled: Bool
    /// `off`, `starting`, `ready`, `unavailable` or `stopped`.
    public let state: String
    public let origin: String?
    public let detail: String?

    public init(allowed: Bool, enabled: Bool, state: String, origin: String? = nil, detail: String? = nil) {
        self.allowed = allowed; self.enabled = enabled; self.state = state; self.origin = origin; self.detail = detail
    }
}

/// What the "Reach from anywhere" setting shows. Only the computer's owner
/// allows it, on the computer; this iPhone then finishes turning it on with
/// its Cupola account. The phone can't turn it off.
public struct RemoteAccessPresentation: Equatable, Sendable {
    public enum Control: Equatable, Sendable {
        /// A switch the person can turn on.
        case available
        /// A switch shown off and disabled, with the reason in `note`.
        case unavailable
        /// On: a value, not a switch.
        case on(String)
    }
    public let control: Control
    public let note: String
    /// Pairing again with the QR code or link is what makes it available.
    public private(set) var pairAgain = false

    public static let name = "Reach from anywhere"

    /// Why a computer paired with a code can't turn it on: the code gives this
    /// device no saved key to check the computer against through the relay.
    public static func pairedWithCode(_ computer: String) -> String {
        "Pair with the QR code or link to use this. A pairing code doesn’t give this device \(computer)’s security key, which it needs to make sure Cupola’s relay reaches \(computer) and nothing else. On \(computer), run `cupola pair`."
    }

    /// `note` marks commands as inline code (`cupola relay on`) for Markdown display.
    /// `keyPinned` is false for a computer paired with a code: it has no saved
    /// security key, so the relay can't be checked or turned on from here.
    public init(status: RelayStatus?, signedIn: Bool, computer: String, usingRelay: Bool, keyPinned: Bool) {
        let privacy = "Messages stay end-to-end encrypted between this device and \(computer); Cloudflare carries them but can’t read them."
        guard let status else {
            control = .unavailable
            // Already through the relay, the network is fine: the check failed.
            note = usingRelay ? "Couldn’t check this setting on \(computer). Try again in a moment."
                : "Connect to \(computer) on the same Wi-Fi or Tailscale to check this setting."
            return
        }
        if status.enabled {
            let value = switch status.state {
            case "ready": usingRelay ? "On, in use" : "On"
            case "starting": "Starting…"
            case "stopped": "Paused"
            case "unavailable": "Not reachable now"
            default: "On"
            }
            control = .on(value)
            var lines: [String] = []
            if status.state == "stopped" { lines.append("It resumes when Cupola starts on \(computer).") }
            else if status.state == "unavailable" {
                lines.append(status.detail ?? "Cupola’s relay isn’t reachable right now. The home network and Tailscale still work.")
            }
            if keyPinned {
                lines.append("Used only when the home network and Tailscale aren’t available. \(privacy)")
            } else {
                // On for the computer, but this device has no saved key to
                // check the computer through the relay, so it can't use it.
                lines.append(Self.pairedWithCode(computer))
                pairAgain = true
            }
            lines.append("Only \(computer) can turn it off. On \(computer), run `cupola relay off`.")
            note = lines.joined(separator: " ")
        } else if !status.allowed {
            control = .unavailable
            note = "To use Cupola away from home without Tailscale, allow it on the computer. On \(computer), run `cupola relay on` first."
        } else if !keyPinned {
            control = .unavailable
            note = Self.pairedWithCode(computer)
            pairAgain = true
        } else if !signedIn {
            control = .unavailable
            note = "Sign in to your Cupola account in Notifications to turn this on. \(privacy)"
        } else {
            control = .available
            note = "Reaches \(computer) through Cupola’s relay when the home network and Tailscale aren’t available. \(privacy)"
        }
    }
}

/// Why this device can't start turning on the relay.
public enum RelayEnableFailure: Error, Equatable, Sendable {
    /// Paired with a code, so there is no saved host key to prove the computer against.
    case pairedWithCode
}

/// `relay.enable`, as the computer checks it: the grant and label signed by
/// this device, expecting the owner to have allowed the relay.
public enum RelayEnable {
    public static let path = "/api/v1/relay/enable"

    public static func transcript(grant: String, label: String, nonce: String, connection: SavedConnection, issuedAtMs: UInt64) throws -> Data {
        try SignedAction.transcript(action: "relay.enable", path: path, object: ["grant": grant, "label": label], nonce: nonce,
                                    connection: connection, issuedAtMs: issuedAtMs, expectedState: "allowed")
    }

    public static func body(grant: String, label: String, nonce: String, issuedAtMs: UInt64, signature: String) throws -> Data {
        try SignedAction.body(fields: ["grant": grant, "label": label], nonce: nonce, issuedAtMs: issuedAtMs, signature: signature)
    }
}
