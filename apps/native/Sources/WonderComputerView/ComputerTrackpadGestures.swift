import CoreGraphics
import Foundation

/// The separate trackpad under the picture (iPhone Duo laptop pose). Touches in,
/// pointer actions out, following Moonlight's touchpad rules
/// (moonlight-ios, GPL-3.0; rules only, no code): one finger moves the pointer,
/// a tap clicks, a two-finger tap right-clicks, two fingers scroll, and holding
/// one finger still for 650 ms presses the button so the next movement drags.
/// A two-finger scroll flicked off the pad coasts, like scrolling on the picture.
///
/// Pure value: the view feeds touch events and a timer tick at `holdDeadline`.
public struct ComputerTrackpadGestures: Sendable {
    public static let holdToDrag: TimeInterval = 0.65
    /// A finger that moves less than this is still a tap or a hold.
    public static let slop: CGFloat = 10
    /// Two fingers lifted within this of the second touching count as a right click.
    public static let rightTapLimit: TimeInterval = 0.5
    /// A scroll whose last movement is older than this at lift-off does not coast.
    public static let coastWindow: TimeInterval = 0.1

    public enum Output: Equatable, Sendable {
        case move(CGSize)
        case click(button: String)
        case scroll(CGSize)
        /// Fingers left the pad mid-scroll at this velocity (points per second);
        /// the pad coasts with `ComputerScrollMomentum` until a touch or cancel.
        case coast(CGVector)
        case dragBegan
        case dragEnded
    }

    private enum Phase: Equatable, Sendable {
        case idle
        /// One finger down, not yet moved or held.
        case pending(id: Int, origin: CGPoint, start: TimeInterval)
        case moving(id: Int, last: CGPoint, time: TimeInterval)
        case dragging(id: Int, last: CGPoint, time: TimeInterval)
        case twoFinger(origin: CGPoint, last: CGPoint, start: TimeInterval, scrolling: Bool, tapEligible: Bool,
                       scroll: ScrollSample = ScrollSample())
        /// The gesture finished; ignore the remaining fingers until all lift.
        case finishing
    }

    /// The smoothed two-finger scroll velocity and when it last moved.
    struct ScrollSample: Equatable, Sendable {
        var velocity = CGVector.zero
        var time: TimeInterval?
        var point = CGPoint.zero
        /// The sample before the current timestamp: fingers UIKit reports at one
        /// instant count once, measured from here. No velocity before the first.
        var base: Base?

        struct Base: Equatable, Sendable {
            var velocity: CGVector?
            var time: TimeInterval
            var point: CGPoint
        }
    }

    private var phase: Phase = .idle
    private var touches: [Int: CGPoint] = [:]

    public init() {}

    /// When the view should call `tick(time:)` to start a drag.
    public var holdDeadline: TimeInterval? {
        if case let .pending(_, _, start) = phase { return start + Self.holdToDrag }
        return nil
    }

    public var isDragging: Bool {
        if case .dragging = phase { return true }
        return false
    }

    public mutating func began(id: Int, at point: CGPoint, time: TimeInterval) -> [Output] {
        guard point.x.isFinite, point.y.isFinite else { return [] }
        touches[id] = point
        switch phase {
        case .idle:
            phase = touches.count == 1 ? .pending(id: id, origin: point, start: time) : .finishing
        case .pending, .moving:
            // A second finger turns a tap or a move into a two-finger gesture;
            // only an untouched first finger can still become a right click.
            let tapEligible: Bool
            if case .pending = phase { tapEligible = true } else { tapEligible = false }
            let center = centroid
            phase = .twoFinger(origin: center, last: center, start: time, scrolling: false, tapEligible: tapEligible)
        case .twoFinger(let origin, let last, let start, let scrolling, _, let scroll):
            // A third finger is not a right click.
            phase = .twoFinger(origin: origin, last: last, start: start, scrolling: scrolling, tapEligible: false,
                               scroll: scroll)
        case .dragging, .finishing:
            break
        }
        return []
    }

    public mutating func moved(id: Int, to point: CGPoint, time: TimeInterval) -> [Output] {
        guard touches[id] != nil, point.x.isFinite, point.y.isFinite else { return [] }
        touches[id] = point
        switch phase {
        case let .pending(primary, origin, start) where primary == id:
            guard distance(point, origin) > Self.slop else { return [] }
            phase = .moving(id: id, last: point, time: time)
            return [.move(Self.accelerated(from: origin, to: point, interval: time - start))]
        case let .moving(primary, last, previous) where primary == id:
            phase = .moving(id: id, last: point, time: time)
            return Self.moveOutput(from: last, to: point, interval: time - previous)
        case let .dragging(primary, last, previous) where primary == id:
            phase = .dragging(id: id, last: point, time: time)
            return Self.moveOutput(from: last, to: point, interval: time - previous)
        case let .twoFinger(origin, last, start, scrolling, tapEligible, scroll):
            let center = centroid
            if !scrolling {
                guard distance(center, origin) > Self.slop else { return [] }
                phase = .twoFinger(origin: origin, last: center, start: start, scrolling: true, tapEligible: false,
                                   scroll: Self.sampled(scroll, at: center, time: time, origin: origin, start: start))
                return [.scroll(CGSize(width: center.x - origin.x, height: center.y - origin.y))]
            }
            phase = .twoFinger(origin: origin, last: center, start: start, scrolling: true, tapEligible: tapEligible,
                               scroll: Self.sampled(scroll, at: center, time: time, origin: origin, start: start))
            let delta = CGSize(width: center.x - last.x, height: center.y - last.y)
            return delta == .zero ? [] : [.scroll(delta)]
        default:
            return []
        }
    }

    public mutating func ended(id: Int, at point: CGPoint, time: TimeInterval) -> [Output] {
        guard touches.removeValue(forKey: id) != nil else { return [] }
        var output: [Output] = []
        switch phase {
        case let .pending(primary, _, _) where primary == id:
            output = [.click(button: "left")]
            phase = .finishing
        case let .moving(primary, _, _) where primary == id:
            phase = .finishing
        case let .dragging(primary, _, _) where primary == id:
            output = [.dragEnded]
            phase = .finishing
        case let .twoFinger(_, _, start, scrolling, tapEligible, scroll):
            if !scrolling, tapEligible, time - start <= Self.rightTapLimit {
                output = [.click(button: "right")]
            } else if scrolling, let last = scroll.time, time - last <= Self.coastWindow, scroll.velocity != .zero {
                output = [.coast(scroll.velocity)]
            }
            phase = .finishing
        default:
            break
        }
        if touches.isEmpty { phase = .idle }
        return output
    }

    /// A touch the system took away (an incoming call, a system gesture).
    /// Nothing clicks; a held button is released.
    public mutating func cancelled() -> [Output] {
        let wasDragging = isDragging
        touches.removeAll()
        phase = .idle
        return wasDragging ? [.dragEnded] : []
    }

    public mutating func tick(time: TimeInterval) -> [Output] {
        guard case let .pending(id, _, start) = phase, time - start >= Self.holdToDrag,
              let point = touches[id] else { return [] }
        phase = .dragging(id: id, last: point, time: time)
        return [.dragBegan]
    }

    /// Smooths the scroll velocity over recent movements, as a pan recognizer does.
    private static func sampled(_ sample: ScrollSample, at point: CGPoint, time: TimeInterval,
                                origin: CGPoint, start: TimeInterval) -> ScrollSample {
        var sample = sample
        if let previousTime = sample.time, previousTime != time {
            sample.base = ScrollSample.Base(velocity: sample.velocity, time: previousTime, point: sample.point)
        } else if sample.time == nil {
            sample.base = ScrollSample.Base(velocity: nil, time: start, point: origin)
        }
        sample.time = time
        sample.point = point
        guard let base = sample.base else { return sample }
        let interval = time - base.time
        guard interval.isFinite, interval > 0 else { return sample }
        let seconds = CGFloat(max(interval, 1.0 / 240))
        let instant = CGVector(dx: (point.x - base.point.x) / seconds, dy: (point.y - base.point.y) / seconds)
        guard let previous = base.velocity else {
            sample.velocity = instant
            return sample
        }
        let weight: CGFloat = 0.6
        sample.velocity = CGVector(dx: instant.dx * weight + previous.dx * (1 - weight),
                                   dy: instant.dy * weight + previous.dy * (1 - weight))
        return sample
    }

    private var centroid: CGPoint {
        guard !touches.isEmpty else { return .zero }
        let sum = touches.values.reduce(CGPoint.zero) { CGPoint(x: $0.x + $1.x, y: $0.y + $1.y) }
        return CGPoint(x: sum.x / CGFloat(touches.count), y: sum.y / CGFloat(touches.count))
    }

    private func distance(_ a: CGPoint, _ b: CGPoint) -> CGFloat {
        ((a.x - b.x) * (a.x - b.x) + (a.y - b.y) * (a.y - b.y)).squareRoot()
    }

    private static func accelerated(from a: CGPoint, to b: CGPoint, interval: TimeInterval) -> CGSize {
        ComputerPointerAcceleration.accelerated(CGSize(width: b.x - a.x, height: b.y - a.y), interval: interval)
    }

    private static func moveOutput(from a: CGPoint, to b: CGPoint, interval: TimeInterval) -> [Output] {
        a == b ? [] : [.move(accelerated(from: a, to: b, interval: interval))]
    }
}
