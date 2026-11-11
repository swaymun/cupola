import Foundation

/// What pairing does when asking the computer for the session fails while the
/// owner confirms the request there.
public enum EnrollmentPoll {
    public enum Decision: Equatable, Sendable { case keepPolling, fail }

    /// A locked phone or a dropped link must not abandon an offer the computer
    /// still holds: only a definite refusal, or the offer's end, stops polling.
    public static func decision(after error: Error, nowMs: UInt64, expiresAtMs: UInt64) -> Decision {
        guard nowMs < expiresAtMs else { return .fail }
        switch error {
        case is URLError: return .keepPolling
        case PairingFailure.response(let status): return status == 409 || status >= 500 ? .keepPolling : .fail
        default: return .fail
        }
    }

    /// A computer that no longer knows this device answers its renewal with
    /// 404 or 410. That ends access exactly like a 401 does.
    public static func renewalFailure(_ error: Error) -> Error {
        if case PairingFailure.response(let status) = error, status == 404 || status == 410 { return PairingFailure.response(401) }
        return error
    }
}
