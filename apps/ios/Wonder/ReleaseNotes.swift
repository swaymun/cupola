import Foundation

/// The welcome carousel and every "What's new" sheet are data, kept here.
/// Add a version's notes to `whatsNew` to show them once after updating to it.
struct CarouselPage: Identifiable, Equatable {
    let symbol: String
    let title: String
    let body: String
    var id: String { title }
}

enum ReleaseNotes {
    static let welcome: [CarouselPage] = [
        CarouselPage(symbol: "bubble.left.and.text.bubble.right",
                     title: "Welcome to Cupola",
                     body: "Talk to Codex, Claude Code and other agents running on your computer, from your iPhone or iPad."),
        CarouselPage(symbol: "wifi",
                     title: "Set up on your Wi-Fi",
                     body: "Install Cupola on your computer, then pair this device while both are on the same Wi-Fi. To connect away from home, you can add Tailscale later."),
        CarouselPage(symbol: "folder",
                     title: "Your Projects, one conversation",
                     body: "Each Project keeps its threads together. Start a task, follow replies as they stream and pick up where you left off."),
        CarouselPage(symbol: "checkmark.shield",
                     title: "You stay in charge",
                     body: "Approve commands, answer questions and review code changes before they land. Your computer does the work; nothing runs on Cupola’s servers."),
        CarouselPage(symbol: "bell.badge",
                     title: "Know when it’s done",
                     body: "Sign in to a Cupola account to get notifications and Live Activities when an agent finishes or needs you, and to reach your computer through Cupola’s relay. Pairing on Wi-Fi or Tailscale and your chats work without one."),
    ]

    /// Notes per marketing version (`CFBundleShortVersionString`).
    static let whatsNew: [String: [CarouselPage]] = [
        "1.0": [
            CarouselPage(symbol: "bell.badge",
                         title: "Notifications need an account",
                         body: "Sign in with Apple in Settings → Notifications to keep getting notifications and Live Activities. Previews stay end-to-end encrypted."),
            CarouselPage(symbol: "sparkles.rectangle.stack",
                         title: "Try Cupola without a computer",
                         body: "A demo computer shows Projects, replies, approvals and code changes. Leave it any time from Chats or Settings."),
        ],
    ]

    /// What Settings replays: this version's notes, or the newest written for an
    /// earlier version when this one has none. Empty when nothing applies.
    static func latest(for version: String, notes: [String: [CarouselPage]] = whatsNew) -> [CarouselPage] {
        if let current = notes[version] { return current }
        let earlier = notes.keys.filter { $0.compare(version, options: .numeric) == .orderedAscending }
        guard let newest = earlier.max(by: { $0.compare($1, options: .numeric) == .orderedAscending }) else { return [] }
        return notes[newest] ?? []
    }
}
