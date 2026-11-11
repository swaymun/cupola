import CoreGraphics
import Foundation

/// Where a conversation's companion pane (the computer, Terminal, Files, a
/// browser…) and the chat go in a regular-width window: iPad, an unfolded
/// iPhone Duo, a large iPhone on its side.
///
/// The window's shape picks the direction: at least 1.2 times as wide as tall
/// puts the tool beside the chat, otherwise the tool sits above it. On an
/// unfolded iPhone Duo the split sits exactly on the fold (the reserved
/// `.division` region) and follows its direction; it is not resizable there.
/// Elsewhere a divider sets the pane's share, clamped so both sides stay usable.
public struct CompanionPaneLayout: Equatable, Sendable {
    public enum Direction: Equatable, Sendable {
        /// Chat and tool side by side.
        case sideBySide
        /// Tool above the chat (or below it, flipped).
        case stacked
    }

    public var direction: Direction
    public var chat: CGRect
    public var pane: CGRect
    /// The divider or the fold band between them. Only a resizable divider
    /// hosts a target; a physical fold stays empty. `.null` in expanded iPad mode.
    public var divider: CGRect
    /// The divider can be dragged (false on a fold and while expanded).
    public var resizable: Bool
    /// The split sits on a Duo's fold.
    public var onFold: Bool
    /// Swap sits on the split itself: a fold with panels side by side has no
    /// visible gap on the iPhone Duo. Elsewhere Swap stays in a header or on a divider strip.
    public var swapsOnDivider: Bool { onFold && direction == .sideBySide }

    /// Width at least this many times the height puts the panels side by side.
    public static let sideBySideAspect: CGFloat = 1.2
    /// The divider's touch strip between the panels.
    public static let dividerThickness: CGFloat = 16
    /// Below this the chat column is too narrow: the Chats sidebar tucks away.
    public static let minimumChatWidth: CGFloat = 360
    public static let minimumPaneWidth: CGFloat = 320
    public static let minimumChatHeight: CGFloat = 240
    public static let minimumPaneHeight: CGFloat = 200
    public static let defaultPaneFraction: CGFloat = 0.5
    /// With the on-screen keyboard up, a stacked pane gives the chat room.
    public static let keyboardPaneFraction: CGFloat = 0.35

    /// The direction for a window of this size, before any fold.
    public static func direction(window: CGSize) -> Direction {
        window.width >= window.height * sideBySideAspect ? .sideBySide : .stacked
    }

    /// Whether the Chats sidebar should step aside while a pane is open: on a
    /// fold (the split needs the whole window), while the tool fills the window,
    /// or when the chat column would be narrower than `minimumChatWidth`.
    public static func tucksSidebar(window: CGSize, sidebarWidth: CGFloat, hasFold: Bool, expanded: Bool) -> Bool {
        if hasFold || expanded { return true }
        let available = window.width - sidebarWidth
        switch direction(window: window) {
        case .sideBySide: return available - dividerThickness - minimumPaneWidth < minimumChatWidth
        case .stacked: return available < minimumChatWidth
        }
    }

    /// Lays out a container of `size`. `window` (the whole window, which may
    /// include a sidebar) picks the direction; `fold` is in the container's own
    /// coordinates. `paneFraction` is the pane's share of the space beside the
    /// divider; `flipped` swaps the panels.
    public static func resolve(size: CGSize, window: CGSize? = nil, fold: ComputerFoldLayout.Fold? = nil,
                               paneFraction: CGFloat = defaultPaneFraction, flipped: Bool = false,
                               expanded: Bool = false, keyboardVisible: Bool = false) -> Self {
        let bounds = CGRect(origin: .zero, size: CGSize(width: max(0, size.width), height: max(0, size.height)))
        var layout: Self
        if let band = foldBand(size: bounds.size, fold: fold) {
            layout = split(bounds, band: band, direction: band.direction, flipped: flipped)
            layout.onFold = true
            layout.resizable = false
        } else {
            let direction = direction(window: window ?? bounds.size)
            var fraction = clampedFraction(paneFraction, size: bounds.size, direction: direction)
            if direction == .stacked, keyboardVisible { fraction = min(fraction, keyboardPaneFraction) }
            layout = split(bounds, band: dividerBand(bounds, direction: direction, fraction: fraction, flipped: flipped),
                           direction: direction, flipped: flipped)
        }
        if expanded && !layout.onFold {
            layout.pane = bounds
            layout.divider = .null
            layout.resizable = false
        }
        return layout
    }

    /// Shared display rectangles for shell navigation and tools. The leading
    /// display owns navigation; the trailing display owns its selected content.
    /// A cover display has no division and returns nil.
    public static func displayPanels(size: CGSize, fold: ComputerFoldLayout.Fold?, trailingTopInset: CGFloat = 0) -> Self? {
        let layout = resolve(size: size, fold: fold)
        guard layout.onFold else { return nil }
        // Shell navigation always leads, including a horizontal fold.
        var panels = layout.direction == .sideBySide ? layout
            : resolve(size: size, fold: fold, flipped: true)
        // A sibling navigation stack owns its own bar. Reclaim the outer
        // stack's top inset on that display instead of stacking both bars.
        if panels.direction == .sideBySide, trailingTopInset.isFinite, trailingTopInset > 0 {
            panels.pane.origin.y -= trailingTopInset
            panels.pane.size.height += trailingTopInset
        }
        return panels
    }

    /// The pane's share for a divider whose centre is at `position` along the
    /// split (x side by side, y stacked), clamped like `resolve`.
    public static func fraction(dividerAt position: CGFloat, size: CGSize, direction: Direction, flipped: Bool) -> CGFloat {
        let length = (direction == .sideBySide ? size.width : size.height) - dividerThickness
        guard length > 0 else { return defaultPaneFraction }
        let before = position - dividerThickness / 2
        // Unflipped: chat leading and tool trailing side by side; tool on top stacked.
        let paneLeads = (direction == .stacked) != flipped
        let pane = paneLeads ? before : length - before
        return clampedFraction(pane / length, size: size, direction: direction)
    }

    /// Keeps both panels at least their minimum; a window too small for both
    /// shares in proportion to the minimums.
    public static func clampedFraction(_ fraction: CGFloat, size: CGSize, direction: Direction) -> CGFloat {
        let length = (direction == .sideBySide ? size.width : size.height) - dividerThickness
        let minimumPane = direction == .sideBySide ? minimumPaneWidth : minimumPaneHeight
        let minimumChat = direction == .sideBySide ? minimumChatWidth : minimumChatHeight
        guard length > 0 else { return defaultPaneFraction }
        guard fraction.isFinite else { return clampedFraction(defaultPaneFraction, size: size, direction: direction) }
        if length < minimumPane + minimumChat { return minimumPane / (minimumPane + minimumChat) }
        return min(max(fraction, minimumPane / length), 1 - minimumChat / length)
    }

    private struct Band {
        var rect: CGRect
        var direction: Direction
    }

    /// A fold that divides the container with room on both sides.
    private static func foldBand(size: CGSize, fold: ComputerFoldLayout.Fold?) -> Band? {
        guard let fold, size.width > 0, size.height > 0 else { return nil }
        let frame = fold.frame
        guard frame.minX.isFinite, frame.minY.isFinite, frame.width.isFinite, frame.height.isFinite,
              frame.width >= 0, frame.height >= 0 else { return nil }
        if frame.height > frame.width {
            // Runs top to bottom: side by side.
            let minX = max(0, frame.minX), maxX = min(size.width, frame.maxX)
            guard maxX >= minX, minX > 0, size.width - maxX > 0 else { return nil }
            return Band(rect: CGRect(x: minX, y: 0, width: maxX - minX, height: size.height), direction: .sideBySide)
        }
        let minY = max(0, frame.minY), maxY = min(size.height, frame.maxY)
        guard maxY >= minY, minY > 0, size.height - maxY > 0 else { return nil }
        return Band(rect: CGRect(x: 0, y: minY, width: size.width, height: maxY - minY), direction: .stacked)
    }

    private static func dividerBand(_ bounds: CGRect, direction: Direction, fraction: CGFloat, flipped: Bool) -> Band {
        let length = (direction == .sideBySide ? bounds.width : bounds.height) - dividerThickness
        let paneLength = (max(0, length) * fraction).rounded()
        let paneLeads = (direction == .stacked) != flipped
        let start = paneLeads ? paneLength : max(0, length) - paneLength
        switch direction {
        case .sideBySide:
            return Band(rect: CGRect(x: start, y: 0, width: dividerThickness, height: bounds.height), direction: direction)
        case .stacked:
            return Band(rect: CGRect(x: 0, y: start, width: bounds.width, height: dividerThickness), direction: direction)
        }
    }

    private static func split(_ bounds: CGRect, band: Band, direction: Direction, flipped: Bool) -> Self {
        let leading: CGRect, trailing: CGRect
        switch direction {
        case .sideBySide:
            leading = CGRect(x: 0, y: 0, width: band.rect.minX, height: bounds.height)
            trailing = CGRect(x: band.rect.maxX, y: 0, width: max(0, bounds.width - band.rect.maxX), height: bounds.height)
        case .stacked:
            leading = CGRect(x: 0, y: 0, width: bounds.width, height: band.rect.minY)
            trailing = CGRect(x: 0, y: band.rect.maxY, width: bounds.width, height: max(0, bounds.height - band.rect.maxY))
        }
        let paneLeads = (direction == .stacked) != flipped
        return Self(direction: direction, chat: paneLeads ? trailing : leading, pane: paneLeads ? leading : trailing,
                    divider: band.rect, resizable: true, onFold: false)
    }
}
