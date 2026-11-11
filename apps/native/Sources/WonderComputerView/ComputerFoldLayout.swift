import CoreGraphics
import Foundation

/// Where the computer view puts the picture and its controls, from the window
/// size and, on a hinged phone (iPhone Duo, iOS 27.1), the hinge and its fold.
///
/// Rules informed by Sill (https://github.com/Saffsanity/sill, Apache-2.0,
/// `DuoPosture.swift`), independently implemented: the fold's position comes
/// from the reserved `.division` region, whether it is in the way comes from the
/// hinge, and before the hinge's first report the region's own active flag
/// decides. Without a hinge or region the layout is today's size-only rule.
public enum ComputerFoldLayout: Equatable, Sendable {
    /// The picture fills the space above; controls sit in a row below it.
    case stacked
    /// Wide windows: controls in a column beside the picture.
    case side
    /// Half-folded with the fold across the screen (laptop pose): the computer
    /// above the fold, trackpad and keys below. Nothing interactive sits in `band`.
    case laptop(top: CGRect, band: CGRect, bottom: CGRect)

    public enum Hinge: Equatable, Sendable {
        case closed, partiallyOpen, fullyOpen
    }

    /// A reserved fold region in the layout's own coordinates.
    public struct Fold: Equatable, Sendable {
        public var frame: CGRect
        public var isActive: Bool
        public init(frame: CGRect, isActive: Bool) {
            self.frame = frame
            self.isActive = isActive
        }
    }

    /// Below these the halves are too small to use; keep the ordinary layout.
    public static let minimumTop: CGFloat = 160
    public static let minimumBottom: CGFloat = 200
    /// Wider than tall and at least this wide puts the controls beside the picture.
    public static let sideControlsWidth: CGFloat = 650

    public static func resolve(size: CGSize, hinge: Hinge?, fold: Fold?) -> Self {
        if let band = horizontalFold(size: size, hinge: hinge, fold: fold) {
            let top = CGRect(x: 0, y: 0, width: size.width, height: band.minY)
            let bottom = CGRect(x: 0, y: band.maxY, width: size.width, height: size.height - band.maxY)
            if top.height >= minimumTop, bottom.height >= minimumBottom {
                return .laptop(top: top, band: band, bottom: bottom)
            }
        }
        return size.width > size.height && size.width >= sideControlsWidth ? .side : .stacked
    }

    /// The fold band across the full width, when it divides the screen now.
    private static func horizontalFold(size: CGSize, hinge: Hinge?, fold: Fold?) -> CGRect? {
        guard let fold, size.width > 0, size.height > 0,
              fold.frame.width.isFinite, fold.frame.height.isFinite,
              fold.frame.width > fold.frame.height else { return nil }
        let inTheWay: Bool
        switch hinge {
        case .partiallyOpen: inTheWay = true
        case .fullyOpen, .closed: inTheWay = false
        case nil: inTheWay = fold.isActive
        }
        guard inTheWay else { return nil }
        let minY = max(0, fold.frame.minY)
        let maxY = min(size.height, fold.frame.maxY)
        guard maxY > minY else { return nil }
        return CGRect(x: 0, y: minY, width: size.width, height: maxY - minY)
    }

    public var isLaptop: Bool {
        if case .laptop = self { return true }
        return false
    }
}
