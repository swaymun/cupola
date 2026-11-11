import CoreGraphics
import Foundation
import WonderPairing

/// Where input actions go (the lease-bound queue in the app; a recorder in tests).
@MainActor public protocol ComputerInputChannel: AnyObject {
    func send(_ actions: [ComputerInputAction])
}

/// Everything this device pressed on the computer that must not outlive the
/// thing that was holding it: a mouse button held for a drag, the keys that
/// went down with it and the modifiers latched on screen. Each reason below ends
/// them all, in the order the press built up (button, then keys, then latches),
/// the way glkvm-mac's `releaseAll()` does when its window loses key focus or closes
/// (SessionWindowController.swift:71-103, KeyboardCapture.swift:48-57; ideas only).
public enum ComputerInputRelease {
    public enum Reason: String, CaseIterable, Sendable {
        /// The hidden keyboard went away (Hide keyboard, or the text view lost focus).
        case keyboardHidden
        /// The app became inactive (a call, the app switcher, Control Center).
        case focusLost
        /// The app moved to the background.
        case background
        /// The fold or the window changed shape.
        case poseChanged
    }

    /// The key-ups and button-ups that leave nothing held. Empty when nothing is.
    public static func actions(press: inout ComputerPointerPress, latch: inout ComputerModifierLatch) -> [ComputerInputAction] {
        // `end` already lets go of the one-shot latch after a held button.
        press.end(latch: &latch) + latch.release()
    }

    /// Sends them. Returns whether anything was held.
    @MainActor @discardableResult
    public static func release(_ reason: Reason, press: inout ComputerPointerPress, latch: inout ComputerModifierLatch,
                               to channel: ComputerInputChannel) -> Bool {
        let held = actions(press: &press, latch: &latch)
        guard !held.isEmpty else { return false }
        channel.send(held)
        return true
    }
}

/// Two-finger and trackpad scrolling in whole points. A scroll event smaller
/// than a point is not dropped: what is left over is carried into the next one,
/// the way glkvm-mac's wheel handler keeps `scrollRemainder` (RemoteView.swift:116-136;
/// idea only).
public struct ComputerScrollAccumulator: Sendable {
    public private(set) var remainder = CGSize.zero

    public init() {}

    /// The whole points to send now, or nil while less than a point has built up.
    public mutating func add(_ delta: CGSize) -> CGSize? {
        guard delta.width.isFinite, delta.height.isFinite else { return nil }
        let x = remainder.width + delta.width
        let y = remainder.height + delta.height
        let whole = CGSize(width: x.rounded(.towardZero), height: y.rounded(.towardZero))
        remainder = CGSize(width: x - whole.width, height: y - whole.height)
        return whole == .zero ? nil : whole
    }

    public mutating func reset() { remainder = .zero }
}
