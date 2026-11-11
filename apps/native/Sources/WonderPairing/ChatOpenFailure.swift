import Foundation

/// The computer's refusal and the recovery that can actually help. Shared by
/// links, search and Chats so none of them drops a useful host explanation.
public struct ChatOpenFailure: Equatable, Sendable {
    public let message: String
    public let canRetry: Bool

    public init(_ error: Error, computer: String) {
        if let failure = error as? PairingFailure {
            switch failure {
            case .hostMessage(let status, let reason):
                let reason = reason.trimmingCharacters(in: .whitespacesAndNewlines)
                if !reason.isEmpty {
                    canRetry = status == 0 || status == 408 || status == 425 || status == 429 || status >= 500
                    if status == 401 {
                        message = reason + " Pair this computer again in Settings."
                    } else if status == 403 {
                        message = reason + " Open this chat on \(computer), or check its Project's folders there."
                    } else {
                        message = reason
                    }
                    return
                }
                self = Self(status: status, computer: computer)
                return
            case .response(let status):
                self = Self(status: status, computer: computer)
                return
            default:
                message = failure.localizedDescription
                canRetry = false
                return
            }
        }
        if (error as? URLError)?.code == .timedOut {
            message = "Opening this chat timed out. Try again when \(computer) is connected."
        } else {
            message = "This chat couldn't be opened. Check the connection to \(computer) and try again."
        }
        canRetry = true
    }

    private init(status: Int, computer: String) {
        switch status {
        case 401:
            message = "\(computer) no longer accepts this device. Pair this computer again in Settings."
            canRetry = false
        case 403:
            message = "\(computer) refused to open this chat. Open it on that computer and check its Project."
            canRetry = false
        case 404:
            message = "This chat is no longer on your computer. Choose another chat."
            canRetry = false
        case 400, 409, 410, 422:
            message = "This chat can't be opened here. Open it on \(computer) and check its Project."
            canRetry = false
        default:
            canRetry = status == 0 || status == 408 || status == 425 || status == 429 || status >= 500
            message = canRetry ? "This chat couldn't be opened. Check the connection to \(computer) and try again."
                : "This chat can't be opened here. Open it on \(computer)."
        }
    }
}
