import CoreGraphics
import Foundation

/// Finds the Chrome window that holds the tab Claude in Chrome reports.
///
/// Claude in Chrome's `tabId` is Chrome's own tab ID, the same number Chrome's
/// AppleScript `id of tab` returns (measured 2026-10-14). AppleScript names
/// windows by Chrome's window ID, not a `CGWindowID`, so a window is matched by
/// its bounds, and, when several Chrome windows share those bounds (maximized
/// windows do), by its active tab's title. Anything still not unique is
/// ambiguous and the person chooses from the window menu. Focus and z-order
/// never decide.
public enum BrowserTabWindow {
    public static let bundleIdentifier = "com.google.Chrome"

    public enum Match: Equatable, Sendable {
        case exact(UInt32)
        /// More than one window could hold the tab.
        case ambiguous
        /// No on-screen Chrome window holds the tab (closed, minimized or in another Space).
        case notFound

        public var status: String {
            switch self {
            case .exact: "exact"
            case .ambiguous: "ambiguous"
            case .notFound: "notFound"
            }
        }
    }

    /// One Chrome window as AppleScript reports it.
    public struct ScriptWindow: Equatable, Sendable {
        public let holdsTab: Bool
        public let bounds: CGRect
        public let minimized: Bool
        public let activeTitle: String

        public init(holdsTab: Bool, bounds: CGRect, minimized: Bool, activeTitle: String) {
            self.holdsTab = holdsTab; self.bounds = bounds; self.minimized = minimized; self.activeTitle = activeTitle
        }
    }

    /// One on-screen Chrome window as the window server reports it.
    public struct ScreenWindow: Equatable, Sendable {
        public let id: UInt32
        public let frame: CGRect
        public let title: String?

        public init(id: UInt32, frame: CGRect, title: String?) {
            self.id = id; self.frame = frame; self.title = title
        }
    }

    /// AppleScript printing one line per Chrome window: holds-tab flag,
    /// bounds, minimized and active tab title, tab-separated. It only reads;
    /// it never activates or launches Chrome.
    public static func script(tabID: UInt64) -> String {
        """
        set output to ""
        -- Inside Chrome's block `tab` names Chrome's tab class, not the character.
        set sep to tab
        set nl to linefeed
        tell application id "\(bundleIdentifier)"
            repeat with w in windows
                set b to bounds of w
                -- Chrome reports tab IDs as text.
                set holds to ((id of tabs of w) contains "\(tabID)")
                set output to output & holds & sep & (item 1 of b) & sep & (item 2 of b) & sep & (item 3 of b) & sep & (item 4 of b) & sep & (minimized of w) & sep & (title of active tab of w) & nl
            end repeat
        end tell
        return output
        """
    }

    /// Parses the script's output. Chrome's `bounds` are left, top, right,
    /// bottom in global points from the main display's top-left, the same
    /// space as `kCGWindowBounds` and `SCWindow.frame` (measured: a maximized
    /// window is {0, 33, 1512, 982} in AppleScript and 0,33 1512x949 on screen).
    public static func windows(fromScript output: String) -> [ScriptWindow] {
        output.split(separator: "\n").compactMap { line in
            let fields = line.split(separator: "\t", maxSplits: 6, omittingEmptySubsequences: false).map(String.init)
            guard fields.count == 7, let left = Double(fields[1]), let top = Double(fields[2]),
                  let right = Double(fields[3]), let bottom = Double(fields[4]), right > left, bottom > top else { return nil }
            return ScriptWindow(holdsTab: fields[0] == "true", bounds: CGRect(x: left, y: top, width: right - left, height: bottom - top),
                                minimized: fields[5] == "true", activeTitle: fields[6])
        }
    }

    public static func match(_ script: [ScriptWindow], screen: [ScreenWindow]) -> Match {
        guard let target = script.first(where: \.holdsTab), !target.minimized else { return .notFound }
        let sameBounds = script.filter { !$0.minimized && close($0.bounds, target.bounds) }
        var candidates = screen.filter { close($0.frame, target.bounds) }
        if sameBounds.count > 1 || candidates.count > 1 {
            // Several Chrome windows share these bounds: the active tab's title must single one out.
            guard sameBounds.filter({ $0.activeTitle == target.activeTitle }).count == 1 else { return .ambiguous }
            candidates = candidates.filter { $0.title.map { titleMatches($0, tabTitle: target.activeTitle) } ?? false }
            if candidates.isEmpty { return .notFound }
        }
        switch candidates.count {
        case 0: return .notFound
        case 1: return .exact(candidates[0].id)
        default: return .ambiguous
        }
    }

    static func close(_ a: CGRect, _ b: CGRect) -> Bool {
        abs(a.minX - b.minX) <= 1 && abs(a.minY - b.minY) <= 1 && abs(a.width - b.width) <= 1 && abs(a.height - b.height) <= 1
    }

    /// Chrome shortens a long window title in the middle with "…" (measured).
    static func titleMatches(_ windowTitle: String, tabTitle: String) -> Bool {
        guard !windowTitle.isEmpty else { return false }
        guard let ellipsis = windowTitle.range(of: "…") else { return windowTitle == tabTitle }
        let head = windowTitle[..<ellipsis.lowerBound], tail = windowTitle[ellipsis.upperBound...]
        return tabTitle.count > head.count + tail.count && tabTitle.hasPrefix(head) && tabTitle.hasSuffix(tail)
    }
}
