/// What a plain tap on Send does while a reply runs: steer by default,
/// or queue when the owner chooses it. Taking a thread over from the Mac
/// always goes through Send now, never a plain tap.
public enum FollowUpBehavior {
    public static let storageKey = "wonder.send.followUp"
    public static let queue = "queue"
    public static let steer = "steer"
    public static let defaultChoice = steer

    public static func queuesMessage(choice: String, waits: Bool, offersTakeOver: Bool, canSteer: Bool) -> Bool {
        waits && !plainSendSteers(choice: choice, offersTakeOver: offersTakeOver, canGuide: canSteer)
    }

    public static func placeholder(queues: Bool) -> String { queues ? "Queue a message" : "Message" }

    public static func plainSendSteers(choice: String, offersTakeOver: Bool, canGuide: Bool) -> Bool {
        choice == steer && !offersTakeOver && canGuide
    }
}

/// The composer's trailing controls. While a reply that can be stopped runs
/// and there is nothing to send (no typed or spoken words, no attachment),
/// Stop takes Send's place. With something to send, or a send being prepared
/// (Send shows its progress), Send stays and Stop sits beside it as its own
/// button, so the reply can be stopped without clearing the draft, even while
/// Send itself is blocked (an oversized draft, a usage limit).
public enum ComposerTrailingButton: Equatable, Sendable {
    case send, stop, sendAndStop

    public static func resolve(composer: ComposerIntent?, hearsWords: Bool, preparing: Bool, offersStop: Bool) -> Self {
        guard offersStop else { return .send }
        let hasContent = hearsWords || (composer?.attachmentCount ?? 0) > 0
            || !(composer?.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ?? true)
        return hasContent || preparing ? .sendAndStop : .stop
    }

    public var showsSend: Bool { self != .stop }
    public var showsStop: Bool { self != .send }
}
