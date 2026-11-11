import Foundation
import WonderPairing

/// The one message the computer view shows over its picture, and whether
/// Try again can help. Every failure source (opening the view, the session,
/// the stream) resolves here so a reason is never shown twice.
public struct ComputerViewNotice: Equatable, Sendable {
    public let message: String
    public let canRetry: Bool

    public init(message: String, canRetry: Bool) {
        self.message = message
        self.canRetry = canRetry
    }

    /// Nil while the picture is live and nothing has failed.
    public static func current(failure: String?, session: ComputerSession?, receiverState: ComputerReceiverState,
                               closed: Bool, dialogGone: Bool, hostHere: String) -> ComputerViewNotice? {
        if dialogGone { return Self(message: "That dialog has closed.", canRetry: false) }
        if closed { return Self(message: ComputerReceiverState.closed.message, canRetry: false) }
        if let failure, !failure.isEmpty { return Self(message: ComputerFailureText.plain(failure), canRetry: true) }
        guard let session else {
            if case .failed(let reason) = receiverState { return Self(message: ComputerFailureText.plain(reason), canRetry: true) }
            if case .retrying(let reason) = receiverState { return Self(message: reason, canRetry: true) }
            return Self(message: ComputerReceiverState.preparing.message, canRetry: false)
        }
        // Retrying cannot add viewing to a computer that doesn't offer it.
        if session.state == .unavailable {
            return Self(message: session.failureReason.map(ComputerFailureText.plain)
                            ?? "Live computer viewing is unavailable on \(hostHere).", canRetry: false)
        }
        if !session.capability.available {
            let reason = session.capability.reason
            return Self(message: reason.contains(" ") ? reason : "Live computer viewing is unavailable on \(hostHere).",
                        canRetry: false)
        }
        if [.failed, .ended, .stale].contains(session.state) {
            return Self(message: ComputerFailureText.plain(session.failureReason), canRetry: true)
        }
        switch receiverState {
        case .live: return nil
        case .failed(let reason): return Self(message: ComputerFailureText.plain(reason), canRetry: true)
        case .retrying(let reason): return Self(message: reason, canRetry: true)
        case .closed: return Self(message: ComputerReceiverState.closed.message, canRetry: true)
        default: return Self(message: receiverState.message, canRetry: false)
        }
    }

    /// The header's short status. Quality is named only when the view struggles.
    public static func liveStatus(receiverState: ComputerReceiverState, quality: ComputerStreamQuality,
                                  slowInput: Bool) -> String {
        guard receiverState == .live else { return receiverState.title }
        if let label = quality.label { return label }
        return slowInput ? "Slow to respond" : receiverState.title
    }
}
