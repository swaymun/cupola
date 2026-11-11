import CoreGraphics
import Foundation
import WonderPairing

/// Where a simulator's screen sits in the phone's view, and how a point on it
/// maps to the simulator's native display. The host's touch coordinates are
/// normalized 0...1 in the native (unrotated) framebuffer, so letterboxing and
/// any quarter turn the person applied to the picture are undone here.
public struct SimulatorScreenGeometry: Equatable, Sendable {
    /// The displayed screen, in view coordinates (after rotation).
    public let screen: CGRect
    /// The screen's size before rotation: the native display's aspect.
    public let unrotatedSize: CGSize
    /// Clockwise quarter turns, 0...3.
    public let quarterTurns: Int

    /// - Parameters:
    ///   - viewport: the area the screen fits in.
    ///   - nativeSize: the simulator's framebuffer size (width × height).
    ///   - quarterTurns: clockwise turns applied to the picture.
    ///   - inset: space kept clear on every side.
    public init(viewport: CGSize, nativeSize: CGSize, quarterTurns: Int, inset: CGFloat = 0) {
        let turns = ((quarterTurns % 4) + 4) % 4
        self.quarterTurns = turns
        let nativeWidth = nativeSize.width.isFinite && nativeSize.width > 0 ? nativeSize.width : 9
        let nativeHeight = nativeSize.height.isFinite && nativeSize.height > 0 ? nativeSize.height : 19.5
        let sideways = turns % 2 == 1
        let aspect = sideways ? nativeHeight / nativeWidth : nativeWidth / nativeHeight
        let room = CGSize(width: max(1, (viewport.width.isFinite ? viewport.width : 1) - inset * 2),
                          height: max(1, (viewport.height.isFinite ? viewport.height : 1) - inset * 2))
        let width = min(room.width, room.height * aspect)
        let height = width / aspect
        screen = CGRect(x: (max(viewport.width, 1) - width) / 2, y: (max(viewport.height, 1) - height) / 2,
                        width: width, height: height)
        unrotatedSize = sideways ? CGSize(width: height, height: width) : CGSize(width: width, height: height)
    }

    /// The native display point under `location`, or nil outside the screen.
    /// `clamped` keeps a finger that slid off the edge on the nearest edge.
    public func devicePoint(_ location: CGPoint, clamped: Bool = false) -> CGPoint? {
        guard location.x.isFinite, location.y.isFinite, screen.width > 0, screen.height > 0 else { return nil }
        guard clamped || ((screen.minX...screen.maxX).contains(location.x)
                             && (screen.minY...screen.maxY).contains(location.y)) else { return nil }
        let dx = location.x - screen.midX, dy = location.y - screen.midY
        // Undo the clockwise turns (y grows downwards).
        let unrotated: CGPoint
        switch quarterTurns {
        case 1: unrotated = CGPoint(x: dy, y: -dx)
        case 2: unrotated = CGPoint(x: -dx, y: -dy)
        case 3: unrotated = CGPoint(x: -dy, y: dx)
        default: unrotated = CGPoint(x: dx, y: dy)
        }
        let x = unrotated.x / unrotatedSize.width + 0.5
        let y = unrotated.y / unrotatedSize.height + 0.5
        return CGPoint(x: min(max(x, 0), 1), y: min(max(y, 0), 1))
    }

    /// Where a native display point appears in the view.
    public func location(for point: CGPoint) -> CGPoint {
        let u = CGPoint(x: (point.x - 0.5) * unrotatedSize.width, y: (point.y - 0.5) * unrotatedSize.height)
        let rotated: CGPoint
        switch quarterTurns {
        case 1: rotated = CGPoint(x: -u.y, y: u.x)
        case 2: rotated = CGPoint(x: -u.x, y: -u.y)
        case 3: rotated = CGPoint(x: u.y, y: -u.x)
        default: rotated = u
        }
        return CGPoint(x: screen.midX + rotated.x, y: screen.midY + rotated.y)
    }
}

/// One finger on the simulator screen, streamed as it moves: down when it
/// lands, a move when it has travelled, up when it lifts. Each action goes in
/// its own input batch, so no batch holds more than one short pause and the
/// host's 1.5 s batch limit is never reached; while a batch is in flight the
/// input buffer keeps only the newest move, and the host draws the path
/// between delivered points. A hold is real time, so a long press is a finger
/// that stays down.
public struct SimulatorTouchTracker: Equatable, Sendable {
    /// A tap shorter than this is held this long on the simulator, so the
    /// guest sees a press rather than a zero-length touch.
    public static let minimumHoldMilliseconds: Double = 50
    /// Smaller movement (normalized) is jitter, not a drag.
    public static let moveThreshold: Double = 0.002

    public private(set) var isTouching = false
    private var startedAt: TimeInterval = 0
    private var last = CGPoint.zero

    public init() {}

    /// Nil when a finger is already down or the point is off the screen.
    public mutating func began(at point: CGPoint?, time: TimeInterval) -> ComputerInputAction? {
        guard !isTouching, let point = Self.normalized(point) else { return nil }
        isTouching = true
        startedAt = time
        last = point
        return .touch(x: point.x, y: point.y, phase: "down", delayMs: nil)
    }

    public mutating func moved(to point: CGPoint?) -> ComputerInputAction? {
        guard isTouching, let point = Self.normalized(point),
              hypot(point.x - last.x, point.y - last.y) >= Self.moveThreshold else { return nil }
        last = point
        return .touch(x: point.x, y: point.y, phase: "move", delayMs: nil)
    }

    /// The lift; a lift with no point (cancelled) ends where the finger was.
    public mutating func ended(at point: CGPoint?, time: TimeInterval) -> ComputerInputAction? {
        guard isTouching else { return nil }
        isTouching = false
        let end = Self.normalized(point) ?? last
        let held = (time - startedAt) * 1_000
        let delay = held.isFinite && held < Self.minimumHoldMilliseconds
            ? UInt32((Self.minimumHoldMilliseconds - max(held, 0)).rounded()) : nil
        return .touch(x: end.x, y: end.y, phase: "up", delayMs: delay)
    }

    private static func normalized(_ point: CGPoint?) -> CGPoint? {
        guard let point, point.x.isFinite, point.y.isFinite else { return nil }
        return CGPoint(x: min(max(point.x, 0), 1), y: min(max(point.y, 0), 1))
    }
}

/// Whole gestures for VoiceOver and Switch Control actions, each one input
/// batch within the host's limits (at most 1 s of pause, 1.5 s per batch).
public enum SimulatorGesture {
    public static let tapHoldMilliseconds: UInt32 = 50
    public static let longPressMilliseconds: UInt32 = 800

    public enum Direction: String, CaseIterable, Sendable {
        case up, down, left, right
    }

    public static func tap(at point: CGPoint) -> [ComputerInputAction] {
        [.touch(x: point.x, y: point.y, phase: "down", delayMs: nil),
         .touch(x: point.x, y: point.y, phase: "up", delayMs: tapHoldMilliseconds)]
    }

    public static func longPress(at point: CGPoint) -> [ComputerInputAction] {
        [.touch(x: point.x, y: point.y, phase: "down", delayMs: nil),
         .touch(x: point.x, y: point.y, phase: "up", delayMs: longPressMilliseconds)]
    }

    /// A swipe across the middle of the screen; up moves the content up
    /// (shows what is below), like a finger dragged upwards.
    public static func swipe(_ direction: Direction) -> [ComputerInputAction] {
        let (from, to): (CGPoint, CGPoint) = switch direction {
        case .up: (CGPoint(x: 0.5, y: 0.7), CGPoint(x: 0.5, y: 0.3))
        case .down: (CGPoint(x: 0.5, y: 0.3), CGPoint(x: 0.5, y: 0.7))
        case .left: (CGPoint(x: 0.8, y: 0.5), CGPoint(x: 0.2, y: 0.5))
        case .right: (CGPoint(x: 0.2, y: 0.5), CGPoint(x: 0.8, y: 0.5))
        }
        return [.touch(x: from.x, y: from.y, phase: "down", delayMs: nil),
                .touch(x: to.x, y: to.y, phase: "move", delayMs: nil),
                .touch(x: to.x, y: to.y, phase: "up", delayMs: nil)]
    }
}
