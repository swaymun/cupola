import Foundation

/// A chat's notification choice, stored on its computer (`threads.setNotifications`).
/// Raw values are the host's wire values.
public enum ChatNotificationLevel: String, Codable, CaseIterable, Sendable {
    case `default`, follow, mute

    public var title: String {
        switch self {
        case .default: "Default"
        case .follow: "Follow"
        case .mute: "Mute"
        }
    }

    /// One line for the menu: what this choice does.
    public var explanation: String {
        switch self {
        case .default: "Finished, failed and needs-you alerts"
        case .follow: "Also alerts when its helper tasks finish"
        case .mute: "No alerts; requests still wait in the app"
        }
    }

    /// The glyph for the choice itself.
    public var symbolName: String {
        switch self {
        case .default: "bell"
        case .follow: "bell.badge"
        case .mute: "bell.slash"
        }
    }

    /// The header and sidebar badge. Default is the quiet norm and shows nothing.
    public var badgeSymbolName: String? {
        switch self {
        case .default: nil
        case .follow: "bell.fill"
        case .mute: "bell.slash.fill"
        }
    }

    /// Spoken with the badge; nil for Default.
    public var badgeLabel: String? {
        switch self {
        case .default: nil
        case .follow: "Following notifications"
        case .mute: "Notifications muted"
        }
    }
}

/// What the app knows of each chat's choice on one computer, and the change in
/// flight. A change shows at once and is undone if the computer does not keep it.
public struct ChatNotificationChoices: Equatable, Sendable {
    public struct Change: Equatable, Sendable {
        public let chat: String
        public let previous: ChatNotificationLevel
        public let requested: ChatNotificationLevel
        fileprivate let token: Int
    }

    private var saved: [String: ChatNotificationLevel] = [:]
    private var changes: [String: Change] = [:]
    private var nextToken = 0

    public init() {}

    /// What to show: the requested choice while it saves, else the saved one.
    public func level(_ chat: String) -> ChatNotificationLevel {
        changes[chat]?.requested ?? saved[chat] ?? .default
    }

    public func isSaving(_ chat: String) -> Bool { changes[chat] != nil }
    public func isKnown(_ chat: String) -> Bool { saved[chat] != nil || changes[chat] != nil }

    /// Starts a change; nil when this chat is already saving or already has the choice.
    public mutating func begin(_ chat: String, to requested: ChatNotificationLevel) -> Change? {
        let current = level(chat)
        guard changes[chat] == nil, current != requested else { return nil }
        nextToken += 1
        let change = Change(chat: chat, previous: current, requested: requested, token: nextToken)
        changes[chat] = change
        return change
    }

    /// The computer kept the choice (`confirmed` is what it answered).
    public mutating func finish(_ change: Change, savedAs confirmed: ChatNotificationLevel) {
        guard changes[change.chat] == change else { return }
        changes[change.chat] = nil
        saved[change.chat] = confirmed
    }

    /// The computer did not keep it: show the old choice again.
    public mutating func fail(_ change: Change) {
        guard changes[change.chat] == change else { return }
        changes[change.chat] = nil
    }

    /// A choice read from the computer. A change in flight wins until it ends.
    public mutating func learn(_ chat: String, _ level: ChatNotificationLevel) {
        guard changes[chat] == nil else { return }
        saved[chat] = level
    }

    public mutating func forgetAll() {
        self = ChatNotificationChoices()
    }

    /// The plain message after a failed change.
    public static func failureMessage(_ change: Change) -> String {
        "Couldn’t set notifications to \(change.requested.title). Still \(change.previous.title)."
    }
}
