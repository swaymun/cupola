import Foundation

/// An app identified by native computer-use metadata, optionally paired with a
/// window explicitly selected from the Mac's native window inventory.
public struct ComputerAgentWindowTarget: Hashable, Sendable, Identifiable {
    /// Native app identity; window selections also retain its exact bundle ID.
    public let app: String
    public let windowID: UInt32?
    public let windowTitle: String?
    public var id: String { sourceID }

    public static let chrome = "com.google.Chrome"

    public init?(app raw: String, windowID: UInt32? = nil, windowTitle: String? = nil) {
        let app = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !app.isEmpty, app.utf8.count <= 150, !app.contains("#"),
              !app.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }),
              Self.displayName(for: app) != nil, windowID != 0 else { return nil }
        self.app = app
        self.windowID = windowID
        self.windowTitle = windowTitle.map { String($0.prefix(160)) }
    }

    public init?(source: ComputerSource) {
        guard source.kind == "window", let app = source.appId, let id = source.id,
              id.hasPrefix("app:\(app)#"),
              id.dropFirst("app:\(app)#".count).allSatisfy({ $0.isASCII && $0.isNumber }),
              let windowID = UInt32(id.dropFirst("app:\(app)#".count)) else { return nil }
        self.init(app: app, windowID: windowID, windowTitle: source.name)
    }

    /// Only a source with a window number pins capture to an exact window.
    public var sourceID: String {
        if let windowID { return "app:\(app)#\(windowID)" }
        return "app:" + app
    }

    /// "TextEdit" for `com.apple.TextEdit`, `/Applications/TextEdit.app` or `TextEdit`.
    public var displayName: String { windowTitle ?? Self.displayName(for: app) ?? app }

    /// Distinct bundle identities remain distinct even when their names match.
    var matchKey: String { app }

    static func displayName(for app: String) -> String? {
        if app.hasPrefix("/") {
            let parts = app.split(separator: "/")
            if let bundle = parts.last(where: { $0.lowercased().hasSuffix(".app") }) {
                return String(bundle.dropLast(4))
            }
            return parts.last.map(String.init)
        }
        if app.lowercased().hasSuffix(".app") { return String(app.dropLast(4)) }
        // A reverse-DNS bundle ID has no spaces and at least two dots.
        if !app.contains(" "), app.filter({ $0 == "." }).count >= 2,
           let last = app.split(separator: ".").last, !last.isEmpty {
            return String(last)
        }
        // A plain app name: short, and made of name characters.
        guard app.count <= 60,
              app.unicodeScalars.allSatisfy({ CharacterSet.alphanumerics.contains($0) || " -_.&+'".unicodeScalars.contains($0) }),
              app.unicodeScalars.contains(where: { CharacterSet.letters.contains($0) }) else { return nil }
        return app
    }
}

/// What an agent is doing with the computer in a conversation's latest turn.
public struct ComputerAgentActivity: Equatable, Sendable {
    /// The latest turn is running and has used computer tools.
    public let isUsingComputer: Bool
    /// A computer tool call is in flight right now. A running turn that is only
    /// thinking between calls does not hold the screen.
    public let isActing: Bool
    /// Apps the running turn operated, most recent first.
    public let targets: [ComputerAgentWindowTarget]
    /// Recent-window retention is UI state, not a claim about an agent's input lease.
    private let expirations: [String: Date]
    public var nextExpiration: Date? { expirations.values.min() }

    public static let idle = ComputerAgentActivity(isUsingComputer: false, targets: [])
    static let maximumTargets = 4

    public init(isUsingComputer: Bool, isActing: Bool = false, targets: [ComputerAgentWindowTarget],
                expirations: [String: Date] = [:]) {
        self.isUsingComputer = isUsingComputer
        self.isActing = isActing && isUsingComputer
        self.targets = targets
        self.expirations = expirations
    }

    /// Drop a recent window five minutes after its last observed use. A native
    /// running tool remains present, and a completed turn clears everything.
    public func current(at now: Date) -> Self {
        let live = targets.filter { expirations[$0.id].map { $0 > now } ?? true }
        let retained = Set(live.map(\.id))
        return Self(isUsingComputer: isUsingComputer && (targets.isEmpty || !live.isEmpty), isActing: isActing, targets: live,
                    expirations: expirations.filter { retained.contains($0.key) })
    }

    private static func expires(after item: ReadItem) -> Date {
        let seconds = Double(item.createdAt).map { $0 / 1000 } ?? ReadTimestamp.seconds(item.createdAt)
        // Missing or malformed timestamps cannot justify retaining an old window.
        guard let seconds, seconds.isFinite, seconds >= 0,
              seconds <= Date.distantFuture.timeIntervalSince1970 - 300 else { return .distantPast }
        return Date(timeIntervalSince1970: seconds + 300)
    }

    public static func latest(in snapshot: ConversationSnapshot?) -> ComputerAgentActivity {
        guard let turn = snapshot?.thread.turns?.last, turn.isInProgress else { return .idle }
        return latest(in: turn)
    }

    public static func latest(in turn: ReadTurn) -> ComputerAgentActivity {
        guard turn.isInProgress else { return .idle }
        let calls = turn.items.filter(isComputerCall)
        // Reset destroys the native computer-use bindings; old apps must not
        // survive it merely because the broader agent turn is still running.
        let reset = calls.lastIndex { $0.payload?["tool"]?.string == "js_reset" && $0.state != "failed" }
        let active = calls.dropFirst(reset.map { $0 + 1 } ?? 0)
            .filter { !["failed", "cancelled"].contains($0.state) }
        guard !active.isEmpty else { return .idle }
        var seen = Set<String>()
        var targets: [ComputerAgentWindowTarget] = []
        var expirations: [String: Date] = [:]
        for item in active.reversed() {
            if let target = target(in: item), seen.insert(target.matchKey).inserted {
                if !["started", "streaming", "inProgress"].contains(item.state) {
                    expirations[target.id] = expires(after: item)
                }
                targets.append(target)
            }
            if targets.count >= maximumTargets { break }
        }
        let bounded = Array(targets.prefix(maximumTargets))
        let ids = Set(bounded.map(\.id))
        let acting = active.contains { ["started", "streaming", "inProgress"].contains($0.state) }
        return Self(isUsingComputer: true, isActing: acting, targets: bounded,
                    expirations: expirations.filter { ids.contains($0.key) })
    }

    static func isComputerCall(_ item: ReadItem) -> Bool {
        guard item.type == "mcpToolCall" || item.type == "dynamicToolCall" else { return false }
        return item.payload?["server"]?.string == "cua_repl" || item.payload?["tool"]?.string == "wonder_computer_use"
            || isChromeCall(item) || isCuaDriverCall(item)
    }

    /// Claude Code driving the Mac through CuaDriver (`mcp__cua_driver__*`).
    static func isCuaDriverCall(_ item: ReadItem) -> Bool {
        guard item.type == "mcpToolCall", let tool = item.payload?["tool"]?.string else { return false }
        return tool.hasPrefix("mcp__cua_driver__")
    }

    /// Claude Code's native Chrome integration (`mcp__claude-in-chrome__*`).
    static func isChromeCall(_ item: ReadItem) -> Bool {
        guard item.type == "mcpToolCall", let tool = item.payload?["tool"]?.string else { return false }
        return tool.hasPrefix("mcp__claude-in-chrome__")
    }

    /// Native results retain the app even when JavaScript reuses a variable.
    /// Code, titles and printed examples cannot establish a target.
    static func target(in item: ReadItem) -> ComputerAgentWindowTarget? {
        if isChromeCall(item) { return ComputerAgentWindowTarget(app: ComputerAgentWindowTarget.chrome) }
        if isCuaDriverCall(item) {
            // Only launch/activate calls name the app; the rest carry a pid, so the whole screen shows.
            guard case let .object(arguments) = item.payload?["arguments"],
                  let bundle = arguments["bundle_id"]?.string, validAppID(bundle) else { return nil }
            return ComputerAgentWindowTarget(app: bundle)
        }
        guard case let .object(result) = item.payload?["result"],
              case let .object(meta) = result["_meta"],
              case let .object(surface) = meta["codex/toolSurface"],
              surface["kind"]?.string == "computerUse",
              case let .object(app) = surface["app"],
              app["kind"]?.string == "appId",
              let appID = app["appId"]?.string,
              validAppID(appID) else { return nil }
        return ComputerAgentWindowTarget(app: appID)
    }

    private static func validAppID(_ id: String) -> Bool {
        id.utf8.allSatisfy { (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) || [45, 46, 95].contains($0) }
    }
}
