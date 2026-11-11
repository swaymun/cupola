import ActivityKit
import AppIntents
import Foundation

/// The Live Activity while Cupola Voice talks, so the owner can mute or end it
/// with the screen locked or another app in front. Built into both the app
/// and the widget extension; the phone starts and updates it locally (no
/// push), and its buttons run in the app, which owns the session.
struct VoiceActivityAttributes: ActivityAttributes {
    struct ContentState: Codable, Hashable, Sendable {
        var muted: Bool
        /// When the phone ends the session (its limit, or OpenAI's hour).
        var endsAt: Date
    }
    /// The paired computer voice acts on.
    var computer: String
}

/// What the Live Activity's buttons do; set by the running voice session.
@MainActor enum VoiceActivityControls {
    static var mute: (() -> Void)?
    static var end: (() -> Void)?
}

struct VoiceMuteIntent: LiveActivityIntent {
    static let title: LocalizedStringResource = "Mute or unmute Cupola voice"
    static let isDiscoverable = false

    @MainActor func perform() async throws -> some IntentResult {
        VoiceActivityControls.mute?()
        return .result()
    }
}

struct VoiceEndIntent: LiveActivityIntent {
    static let title: LocalizedStringResource = "End Cupola voice"
    static let isDiscoverable = false

    @MainActor func perform() async throws -> some IntentResult {
        VoiceActivityControls.end?()
        return .result()
    }
}
