import ActivityKit
import Foundation

/// The one Live Activity for agent work on this phone's paired computers: a
/// status row per chat with work. Built into both the app and the widget
/// extension.
///
/// The push service merges every computer's chats and pushes one
/// `ContentState` through APNs as a Live Activity push, which iOS renders
/// itself: no extension can decrypt it. The content holds a state, counts and up
/// to five chat rows (a short title and Project name, a status, a start time, the
/// computer's registration and a random route only this phone can resolve with
/// that computer). The activity is status only: no buttons, and a tap on a row
/// opens its chat. The phone never adds rows itself.
struct AgentActivityAttributes: ActivityAttributes {
    enum Phase: String, Codable, Hashable, Sendable {
        case running, needsApproval, needsInput, done, failed, stopped

        var isFinished: Bool { self == .done || self == .failed || self == .stopped }

        var symbol: String {
            switch self {
            case .running: "ellipsis"
            case .needsApproval: "hand.raised.fill"
            case .needsInput: "questionmark.bubble.fill"
            case .done: "checkmark.circle.fill"
            case .failed: "exclamationmark.triangle.fill"
            case .stopped: "stop.circle.fill"
            }
        }
    }

    /// One chat's row, with the computer it runs on named by its push registration.
    struct ThreadRow: Codable, Hashable, Sendable {
        var title: String
        var project: String
        var status: Phase
        /// Seconds since 1970 when the chat began work (or ended, once finished).
        var startedAt: Double
        var routeId: String?
        var registrationId: String?
    }

    struct ContentState: Codable, Hashable, Sendable {
        var phase: Phase
        /// Active parent conversations, including those waiting on the owner.
        var running: Int
        /// Chats waiting on approval or input. Nil only while awaiting the
        /// first state or decoding an older activity.
        var needsInput: Int? = 0
        /// Rows for chats with work, most urgent first; nil before the first push.
        var threads: [ThreadRow]? = nil
        /// Chats beyond the rows.
        var more: Int? = nil
    }

    /// The paired Mac, when the phone started the activity; nil for one the
    /// service push-started, which names only a push registration.
    var hostID: String?
    var registrationId: String?
    /// Set on the phone only, never pushed: the computer's name, and the
    /// thread the activity started from (a tap's fallback). Thread names are
    /// not shown: the aggregate's threads change while the activity runs, and
    /// a push cannot carry a name.
    var computer: String?
    var conversationID: String?

    /// Bounded labels: the shared Activity payload is not a place for long text.
    static func label(_ raw: String, limit: Int = 60) -> String? {
        let scalars = raw.unicodeScalars.filter { !CharacterSet.controlCharacters.contains($0) }
        let label = String(String.UnicodeScalarView(scalars.prefix(limit))).trimmingCharacters(in: .whitespacesAndNewlines)
        return label.isEmpty ? nil : label
    }
}

/// What the Lock Screen and the Dynamic Island say for a state. Pure, so tests
/// own it: a stale activity never claims work is running.
struct AgentActivityPresentation: Equatable {
    var headline: String
    var detail: String
    /// The compact/minimal count, matching the phase icon beside it: agents
    /// working while running, agents waiting on you while waiting. Empty for
    /// finished phases, which show the icon only.
    var shortLabel: String
    var symbol: String
    /// Per-thread rows; none while stale, when the counts are unknown.
    var rows: [AgentActivityAttributes.ThreadRow] = []
    /// Chats beyond the rows the service sent.
    var more = 0

    /// What VoiceOver reads for the compact and minimal Island, in every
    /// phase: the icon and count are hidden, so a finished state still says
    /// "Done", "Stopped" or why it couldn't finish.
    var accessibilityLabel: String { detail.isEmpty ? "Cupola" : "Cupola, \(detail)" }

    init(attributes: AgentActivityAttributes, state: AgentActivityAttributes.ContentState, isStale: Bool) {
        // Rows can come from several computers, so the title is the app's.
        headline = (state.threads ?? []).isEmpty ? attributes.computer ?? "Cupola" : "Cupola"
        symbol = state.phase.symbol
        shortLabel = ""
        if !(isStale || state.needsInput == nil) || state.phase.isFinished {
            rows = state.threads ?? []
            more = max(0, state.more ?? 0)
        }
        if (isStale || state.needsInput == nil) && !state.phase.isFinished {
            // Cached or incomplete counts must not claim current work.
            detail = "Open Cupola for the latest"
            shortLabel = "–"
            symbol = "arrow.clockwise"
            return
        }
        switch state.phase {
        case .running, .needsApproval, .needsInput:
            let needsInput = max(0, state.needsInput ?? 0)
            let working = max(0, state.running - needsInput)
            var parts: [String] = []
            if working > 0 { parts.append("\(working) \(working == 1 ? "agent" : "agents") working") }
            if needsInput > 0 { parts.append("\(needsInput) \(needsInput == 1 ? "needs" : "need") input") }
            detail = parts.joined(separator: " · ")
            // A waiting phase's orange hand or question counts the agents
            // waiting on you, never "0" working beside it.
            let waiting = state.phase != .running && needsInput > 0
            shortLabel = String(waiting ? needsInput : working)
        case .done:
            detail = "Agent work completed"
        case .failed:
            detail = "Agent work failed"
        case .stopped:
            detail = "Stopped"
        }
    }
}

extension AgentActivityAttributes.ThreadRow {
    /// What VoiceOver reads for the row.
    var accessibilityLabel: String {
        let state: String = switch status {
        case .running: "working"
        case .needsApproval: "needs your approval"
        case .needsInput: "needs your input"
        case .done: "done"
        case .failed: "couldn’t finish"
        case .stopped: "stopped"
        }
        return [title, project].filter { !$0.isEmpty }.joined(separator: ", ") + ", " + state
    }
}

enum AgentActivityLink {
    /// A row's chat: its computer's registration and the route that computer made for this phone.
    static func url(attributes: AgentActivityAttributes, row: AgentActivityAttributes.ThreadRow, bundleIdentifier: String?) -> URL? {
        guard let identity = ProjectWidgetIdentity(bundleIdentifier: bundleIdentifier),
              let route = row.routeId, ProjectWidgetLink.validID(route),
              let registration = row.registrationId, ProjectWidgetLink.validID(registration) else { return nil }
        return URL(string: "\(identity.linkScheme)://v1/registrations/\(registration)/routes/\(route)")
    }

    /// The most urgent chat's row, else the chat the activity started from, else the app.
    static func url(attributes: AgentActivityAttributes, state: AgentActivityAttributes.ContentState, bundleIdentifier: String?) -> URL? {
        guard let identity = ProjectWidgetIdentity(bundleIdentifier: bundleIdentifier) else { return nil }
        if let first = state.threads?.first, let url = url(attributes: attributes, row: first, bundleIdentifier: bundleIdentifier) { return url }
        if state.running <= 1, let host = attributes.hostID, let chat = attributes.conversationID {
            return ProjectWidgetLink.chat(hostID: host, chatID: chat, identity: identity)
        }
        return nil
    }
}
