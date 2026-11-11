import Foundation
import CryptoKit

// The host supervisor's recovery endpoint, for when the computer's main route
// is down but the supervisor still answers: read its state, ask for a restart,
// fetch redacted diagnostics. Three fixed commands, signed by this device with
// the same `wonder-action-v1` transcript as other consequential actions. The
// host half is `wonderd::rescue`; `crates/wonder-api/src/rescue.rs` owns the
// shapes.

/// Where the host stands, as its supervisor reports it.
public enum RescueState: String, Decodable, Equatable, Sendable {
    case running, starting, stopped, gaveUp
}

public struct RescueStatus: Decodable, Equatable, Sendable {
    public let state: RescueState
    /// Why the daemon last stopped, in the host's words; nil before it did.
    public let lastExit: String?
    public let crashCount: Int
    public let version: String

    public init(state: RescueState, lastExit: String? = nil, crashCount: Int = 0, version: String = "") {
        self.state = state; self.lastExit = lastExit; self.crashCount = crashCount; self.version = version
    }
}

public struct RescueRestartReply: Decodable, Equatable, Sendable {
    public let accepted: Bool
    public let state: RescueState
}

public struct RescueDiagnostics: Decodable, Equatable, Sendable {
    public let status: RescueStatus
    public let generatedAtMs: UInt64
    /// The newest host log lines, already redacted by the host, oldest first.
    public let logLines: [String]

    public init(status: RescueStatus, generatedAtMs: UInt64, logLines: [String]) {
        self.status = status; self.generatedAtMs = generatedAtMs; self.logLines = logLines
    }

    /// What the person reads and shares: a short summary, then the lines.
    public func report(computer: String) -> String {
        let stamp = ISO8601DateFormatter().string(from: Date(timeIntervalSince1970: TimeInterval(generatedAtMs) / 1000))
        var header = ["Cupola diagnostics for \(computer)", "Taken \(stamp)",
                      "Host: \(RescueNotice.describe(status.state))"]
        if !status.version.isEmpty { header.append("Version: \(status.version)") }
        if let exit = status.lastExit { header.append("Last stopped: \(exit)") }
        if status.crashCount > 0 { header.append("Recent crashes: \(status.crashCount)") }
        let lines = logLines.isEmpty ? ["No log lines."] : logLines
        return (header + ["", "Recent log"] + lines).joined(separator: "\n") + "\n"
    }
}

/// The host's refusal: `{error, detail, retryAfterSeconds?}`.
public struct RescueRefusal: Decodable, Equatable, Sendable {
    public let error: String
    public let detail: String
    public let retryAfterSeconds: Int?
}

/// Why a recovery request got no answer worth acting on.
public enum RescueFailure: Error, Equatable, Sendable {
    /// No recovery address answered: the supervisor is down too, or the phone is away from home.
    case unreachable
    /// The host does not accept this device (unpaired, revoked, bad signature).
    case refused
    /// The phone's clock is off by more than the host allows.
    case clockOff
    /// Too many restarts lately; try again after this many seconds.
    case limited(retryAfterSeconds: Int)
    /// This computer's supervisor can't do it.
    case unavailable
}

/// One request, as the host checks it.
public enum RescueCommand: String, CaseIterable, Sendable {
    case status, restart, diagnostics

    public var action: String { "rescue." + rawValue }
    public var path: String { "/api/v1/rescue/" + rawValue }

    /// SHA-256 of the empty string: a recovery request has no body to hash.
    static let bodySHA256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    /// A recovery request has no session (the daemon that issues sessions may be down).
    static let sessionBinding = "rescue-v1"

    /// What this device signs, as `RescueCommand::transcript_bytes` builds it on the host.
    public func transcript(connection: SavedConnection, nonce: String, issuedAtMs: UInt64) -> Data {
        Data(["wonder-action-v1", action, path, Self.bodySHA256, nonce, Self.sessionBinding,
              connection.credential.deviceId, connection.credential.hostInstallationId, String(issuedAtMs), ""]
            .joined(separator: "\n").utf8)
    }

    /// The request body: exactly these four fields, the host refuses any other.
    public func body(connection: SavedConnection, nonce: String, issuedAtMs: UInt64, signature: String) throws -> Data {
        try JSONSerialization.data(withJSONObject: [
            "deviceId": connection.credential.deviceId, "actionNonce": nonce,
            "issuedAtMs": issuedAtMs, "signature": signature,
        ])
    }
}

public enum RescueRoute {
    /// The supervisor listens one port above the home route and only on the
    /// home network, so only a home address leads to it.
    public static func origin(forHome route: ConnectionRoute) -> String? {
        guard route.kind == .home, let origin = try? PairingLink.origin(route.origin),
              var components = URLComponents(string: origin), let port = components.port, port < 65535 else { return nil }
        components.port = port + 1
        return components.string
    }

    public static func origins(_ connection: SavedConnection) -> [String] {
        connection.routes.compactMap(origin(forHome:))
    }
}

/// Asks the supervisor of a paired computer. It needs the computer's pinned
/// key: a recovery answer from an unproven address is never acted on.
public struct RescueClient: Sendable {
    private let api: PairingAPI
    private let sign: @Sendable (Data) async throws -> String
    private let now: @Sendable () -> UInt64
    private let makeNonce: @Sendable () -> String

    public init(api: PairingAPI, sign: @escaping @Sendable (Data) async throws -> String,
                now: @escaping @Sendable () -> UInt64 = { UInt64(Date().timeIntervalSince1970 * 1000) },
                nonce: @escaping @Sendable () -> String = { UUID().uuidString.replacingOccurrences(of: "-", with: "") }) {
        self.api = api; self.sign = sign; self.now = now; self.makeNonce = nonce
    }

    public func status(_ connection: SavedConnection) async throws -> RescueStatus {
        try await call(.status, connection)
    }

    public func restart(_ connection: SavedConnection) async throws -> RescueRestartReply {
        try await call(.restart, connection)
    }

    public func diagnostics(_ connection: SavedConnection) async throws -> RescueDiagnostics {
        try await call(.diagnostics, connection)
    }

    /// Either the command's answer or the host's refusal, whichever the status says.
    private enum Reply<T: Decodable & Sendable>: Decodable, Sendable {
        case answer(T), refusal(RescueRefusal)
        init(from decoder: Decoder) throws {
            if let refusal = try? RescueRefusal(from: decoder) { self = .refusal(refusal) }
            else { self = .answer(try T(from: decoder)) }
        }
    }

    private func call<T: Decodable & Sendable>(_ command: RescueCommand, _ connection: SavedConnection) async throws -> T {
        guard connection.hostKeyPin != nil, !connection.requiresPairing else { throw RescueFailure.unreachable }
        var failure: Error = RescueFailure.unreachable
        for origin in RescueRoute.origins(connection) {
            // A fresh nonce and time for each address: the host accepts each once.
            let nonce = makeNonce(), issued = now()
            let signature = try await sign(command.transcript(connection: connection, nonce: nonce, issuedAtMs: issued))
            let body = try command.body(connection: connection, nonce: nonce, issuedAtMs: issued, signature: signature)
            do {
                let reply: Reply<T> = try await api.request(command.path, origin: origin, body: body,
                                                             decodingStatuses: [400, 401, 429, 501, 503])
                switch reply {
                case .answer(let value): return value
                case .refusal(let refusal): throw Self.failure(refusal)
                }
            } catch let error as RescueFailure where error == .unreachable {
                failure = error
            } catch let error as RescueFailure {
                throw error
            } catch is CancellationError {
                throw CancellationError()
            } catch let error as URLError where error.code == .cancelled {
                throw CancellationError()
            } catch {
                // Another address may still answer.
                failure = RescueFailure.unreachable
            }
        }
        throw failure
    }

    static func failure(_ refusal: RescueRefusal) -> RescueFailure {
        switch refusal.error {
        case "unauthorized": .refused
        case "stale": .clockOff
        case "rate_limited": .limited(retryAfterSeconds: refusal.retryAfterSeconds ?? 60)
        case "unavailable": .unavailable
        default: .unreachable
        }
    }
}

/// What the notice says and offers, in the person's words. The phone shows no
/// protocol: the host's state becomes one of a few plain sentences.
public struct RescueNotice: Hashable, Sendable {
    /// Why the notice is there.
    public enum Cause: Hashable, Sendable {
        /// The host reports it is stopped (or gave up restarting).
        case stopped
        /// The host says it is running, yet the main route does not answer.
        case notResponding
    }

    /// Where a restart stands.
    public enum Phase: Hashable, Sendable {
        case ready
        case restarting
        /// A restart was accepted and the host still isn't up.
        case stillDown
        /// The host won't restart again yet.
        case limited(minutes: Int)
        /// Restart couldn't be asked: the recovery address stopped answering.
        case unreachable
        /// The host doesn't accept this device.
        case refused
        /// This device's clock is too far off for the host to accept a request.
        case clockOff
        /// This computer's supervisor can't restart Cupola.
        case unavailable
    }

    public var cause: Cause
    public var phase: Phase

    public init(cause: Cause, phase: Phase = .ready) { self.cause = cause; self.phase = phase }

    /// The notice for the host's reported state, or nil when the main route
    /// would be expected to work (it is starting) and nothing should interrupt.
    public init?(_ status: RescueStatus) {
        switch status.state {
        case .stopped, .gaveUp: self.init(cause: .stopped)
        case .running: self.init(cause: .notResponding)
        case .starting: return nil
        }
    }

    public func message(computer: String) -> String {
        switch phase {
        case .ready:
            switch cause {
            case .stopped: "Cupola stopped on \(computer)."
            case .notResponding: "Cupola isn’t responding."
            }
        case .restarting: "Restarting Cupola on \(computer)…"
        case .stillDown: "Cupola still isn’t running on \(computer)."
        case .limited(let minutes):
            "Cupola was restarted a few times already. Try again in \(minutes == 1 ? "a minute" : "\(minutes) minutes")."
        case .unreachable: "Couldn’t reach \(computer) to restart Cupola. Try again."
        case .refused: "\(computer) doesn’t accept this device. Pair it again."
        case .clockOff: "This device’s date and time are off. Set them to automatic, then try again."
        case .unavailable: "\(computer) can’t restart Cupola from here. Restart it on the computer."
        }
    }

    /// Restart is offered unless one is under way or the host won't take it.
    public var offersRestart: Bool {
        switch phase {
        case .ready, .stillDown, .unreachable, .clockOff: true
        case .restarting, .limited, .refused, .unavailable: false
        }
    }

    public var isRestarting: Bool { phase == .restarting }

    /// A restart that just failed to bring the host back reads differently
    /// from the first offer, so the person knows trying once more is a choice.
    public var restartTitle: String { phase == .stillDown || phase == .unreachable ? "Restart again" : "Restart" }

    public static func describe(_ state: RescueState) -> String {
        switch state {
        case .running: "running"
        case .starting: "starting"
        case .stopped: "stopped"
        case .gaveUp: "stopped after repeated crashes"
        }
    }

    /// The phase a refusal or miss on Restart leaves behind.
    public static func phase(after failure: RescueFailure) -> Phase {
        switch failure {
        case .limited(let seconds): .limited(minutes: max(1, (seconds + 59) / 60))
        case .refused: .refused
        case .clockOff: .clockOff
        case .unavailable: .unavailable
        case .unreachable: .unreachable
        }
    }
}

/// Get diagnostics: asking, the report to read and share, or why there is none.
public enum RescueDiagnosticsState: Equatable, Sendable {
    case idle, loading, ready(String), failed
}
