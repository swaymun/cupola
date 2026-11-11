import Foundation

/// One SF Symbol per concept, shared by the app, Shortcuts and the share sheet
/// (the widget, which does not link this package, repeats them), so a computer
/// or a thread looks the same on every screen. Keep real distinctions, such as
/// a laptop the Mac reported, out of this table.
public enum ConceptSymbol {
    /// A paired computer of any kind.
    public static let computer = "desktopcomputer"
    /// A Project thread.
    public static let thread = "bubble.left.and.text.bubble.right"
    /// Starting a new thread.
    public static let newThread = "square.and.pencil"
}

/// A status pill above the composer. Which ones show, and in what order, is
/// this device's choice (Settings → Pills); a pill only appears when its
/// feature has something to show, whatever is chosen here.
public enum ComposerPill: String, CaseIterable, Identifiable, Sendable {
    case computer, simulators, files, terminal, preview, attention, goal, agentTasks
    case pullRequests

    public var id: String { rawValue }

    /// The pills Settings → Pills lists and the composer can show. Pull
    /// requests is a tool the chat's title menu opens, not a pill; the case
    /// stays so the tool keeps its name and symbol and saved layouts naming it still load.
    public static let layoutCases: [ComposerPill] = allCases.filter { $0 != .pullRequests }

    public var title: String {
        switch self {
        case .computer: "Computer"
        case .simulators: "Simulators"
        case .files: "Files"
        case .terminal: "Terminal"
        case .preview: "Preview"
        case .attention: "Questions and approvals"
        case .goal: "Goal"
        case .agentTasks: "Agent tasks"
        case .pullRequests: "Pull requests"
        }
    }

    public var systemImage: String {
        switch self {
        case .computer: ConceptSymbol.computer
        case .simulators: "iphone"
        case .files: "folder"
        case .terminal: "apple.terminal"
        case .preview: "globe"
        case .attention: "questionmark.bubble"
        case .goal: "scope"
        case .agentTasks: "circle.dotted"
        case .pullRequests: "arrow.triangle.pull"
        }
    }

    /// An agent waits on what this pill holds, so it cannot be turned off.
    public var alwaysShown: Bool { self == .attention }

    /// The pill row scrolls when it is wider than the screen; these pills stay
    /// outside it, so what an agent waits on cannot scroll out of sight.
    public var pinned: Bool { self == .attention }
}

/// The visible pills as the scrolling row lays them out: pinned pills at the
/// end of the row nearer their place in the owner's order, the rest scrolling
/// between them in that order.
public struct ComposerPillRow: Equatable, Sendable {
    public var leading: [ComposerPill] = []
    public var scrolling: [ComposerPill] = []
    public var trailing: [ComposerPill] = []

    public init(visible: [ComposerPill]) {
        for (index, pill) in visible.enumerated() {
            if !pill.pinned {
                scrolling.append(pill)
            } else if index < visible.count - 1 - index {
                leading.append(pill)
            } else {
                trailing.append(pill)
            }
        }
    }
}

/// The owner's pill order and the pills they turned off. Parsed once from
/// its stored text; views read `visible`.
public struct ComposerPillLayout: Equatable, Sendable {
    public static let storageKey = "composerPills"
    public static let standard = ComposerPillLayout(order: ComposerPill.layoutCases, hidden: [])

    public private(set) var order: [ComposerPill]
    public private(set) var hidden: Set<ComposerPill>
    /// The order with the hidden pills left out.
    public private(set) var visible: [ComposerPill]
    /// `visible` as the scrolling row above the composer lays it out.
    public private(set) var row: ComposerPillRow

    public init(order: [ComposerPill], hidden: Set<ComposerPill>) {
        var seen = Set<ComposerPill>()
        var order = order.filter { $0 != .pullRequests && seen.insert($0).inserted }
        // A pill this device has not stored yet (a newer version's) takes its
        // place after the pill it follows by default.
        for (index, pill) in ComposerPill.layoutCases.enumerated() where !seen.contains(pill) {
            let after = ComposerPill.layoutCases[..<index].last(where: { order.contains($0) })
            order.insert(pill, at: after.flatMap { order.firstIndex(of: $0) }.map { $0 + 1 } ?? 0)
        }
        let hidden = hidden.filter { !$0.alwaysShown && $0 != .pullRequests }
        self.order = order
        self.hidden = hidden
        visible = order.filter { !hidden.contains($0) }
        row = ComposerPillRow(visible: visible)
    }

    /// From the stored text: names in order, a hidden one prefixed with "-".
    /// Unknown names are skipped; missing or unreadable text is the standard layout.
    public init(stored: String?) {
        guard let stored, !stored.isEmpty else { self = .standard; return }
        var order: [ComposerPill] = []
        var hidden = Set<ComposerPill>()
        for entry in stored.split(separator: ",") {
            let off = entry.hasPrefix("-")
            let name = String(off ? entry.dropFirst() : entry[...])
            guard let pill = name == "git" ? .pullRequests : ComposerPill(rawValue: name) else { continue }
            order.append(pill)
            if off { hidden.insert(pill) }
        }
        self.init(order: order, hidden: hidden)
    }

    public var stored: String {
        order.map { (hidden.contains($0) ? "-" : "") + $0.rawValue }.joined(separator: ",")
    }

    public func isShown(_ pill: ComposerPill) -> Bool { !hidden.contains(pill) }

    /// Opening a tool from a menu or restoring it must retain its closing
    /// toggle, even when the owner normally hides that pill. Its stored order
    /// and preference are unchanged; closing restores the ordinary layout.
    public func showing(open pill: ComposerPill?) -> ComposerPillLayout {
        guard let pill, hidden.contains(pill) else { return self }
        return ComposerPillLayout(order: order, hidden: hidden.subtracting([pill]))
    }

    public mutating func setShown(_ pill: ComposerPill, _ shown: Bool) {
        var hidden = hidden
        if shown { hidden.remove(pill) } else { hidden.insert(pill) }
        self = ComposerPillLayout(order: order, hidden: hidden)
    }

    /// Moves the pills at `offsets` to before `destination`, as a list's move does.
    public mutating func move(fromOffsets offsets: IndexSet, toOffset destination: Int) {
        let moving = offsets.filter { order.indices.contains($0) }.map { order[$0] }
        var rest = order.enumerated().filter { !offsets.contains($0.offset) }.map(\.element)
        let before = offsets.filter { $0 < destination }.count
        rest.insert(contentsOf: moving, at: min(max(destination - before, 0), rest.count))
        self = ComposerPillLayout(order: rest, hidden: hidden)
    }
}
