import Foundation
@preconcurrency import WebRTC

/// One decoded frame, shared by replacement renderers while viewing recovers.
/// No transport, input lease or disk persistence survives with the picture.
public final class ComputerPicture: NSObject, RTCVideoRenderer, @unchecked Sendable {
    private let lock = NSRecursiveLock()
    private var frame: RTCVideoFrame?
    /// The pane and a full-screen cover can show the picture at once, so every
    /// attached renderer gets each frame; none steals it from another.
    private let renderers = NSHashTable<AnyObject>.weakObjects()
    private var renderedSize = CGSize.zero

    public var hasFrame: Bool { lock.withLock { frame != nil } }
    public var byteCost: Int { lock.withLock { frame.map { Int($0.width) * Int($0.height) * 4 } ?? 0 } }

    public func setSize(_ size: CGSize) {}

    private var attached: [RTCVideoRenderer] { renderers.allObjects.compactMap { $0 as? RTCVideoRenderer } }

    public func renderFrame(_ frame: RTCVideoFrame?) {
        // A peer clearing its track is not a new picture. Only the view's
        // explicit close (or its connection owner) discards the retained frame.
        guard let frame else { return }
        lock.withLock {
            self.frame = frame
            let size = CGSize(width: Int(frame.width), height: Int(frame.height))
            let resized = size != renderedSize
            renderedSize = size
            for renderer in attached {
                if resized { renderer.setSize(size) }
                renderer.renderFrame(frame)
            }
        }
    }

    func attach(_ renderer: RTCVideoRenderer) {
        lock.withLock {
            guard !renderers.contains(renderer) else { return }
            renderers.add(renderer)
            if let frame {
                renderer.setSize(CGSize(width: Int(frame.width), height: Int(frame.height)))
                renderer.renderFrame(frame)
            }
        }
    }

    /// Draws the retained frame again, e.g. once a hidden view is shown.
    func replay(to renderer: RTCVideoRenderer) {
        lock.withLock {
            guard let frame else { return }
            renderer.setSize(CGSize(width: Int(frame.width), height: Int(frame.height)))
            renderer.renderFrame(frame)
        }
    }

    func detach(_ renderer: RTCVideoRenderer) {
        lock.withLock {
            guard renderers.contains(renderer) else { return }
            renderers.remove(renderer)
            renderer.renderFrame(nil)
        }
    }

    public func clear() {
        lock.withLock {
            frame = nil
            renderedSize = .zero
            for renderer in attached { renderer.renderFrame(nil) }
        }
    }
}

/// Not a view: WebRTC calls renderFrame on its decode thread, so this stays
/// nonisolated and thread-safe and hops to the main queue once per picture.
/// A cleared picture (nil frame) re-arms it for the next first frame.
public final class FirstFrameListener: NSObject, RTCVideoRenderer, @unchecked Sendable {
    private let lock = NSLock()
    private var fired = false
    private let onFirstFrame: @Sendable () -> Void

    /// `onFirstFrame` runs on the main queue.
    public init(onFirstFrame: @escaping @Sendable () -> Void) {
        self.onFirstFrame = onFirstFrame
    }

    public func setSize(_ size: CGSize) {}

    public func renderFrame(_ frame: RTCVideoFrame?) {
        let shouldFire: Bool = lock.withLock {
            if frame == nil { fired = false; return false }
            if fired { return false }
            fired = true
            return true
        }
        guard shouldFire else { return }
        DispatchQueue.main.async { [onFirstFrame] in onFirstFrame() }
    }
}

/// A bounded recovery burst. Events never grant control: reopening is view-only.
public struct ComputerReconnect: Equatable, Sendable {
    public enum Phase: Equatable, Sendable { case viewing, reconnecting, paused, needsRetry, closed }
    public enum Event: Sendable { case interrupted, failed, live, pause, resume, tryAgain, close }
    public private(set) var phase: Phase = .viewing
    public private(set) var attempts = 0
    public init() {}

    /// True asks the owner to reopen the view. Two automatic attempts per outage;
    /// foregrounding and an explicit Try again begin a new burst.
    @discardableResult public mutating func receive(_ event: Event) -> Bool {
        if event == .close { phase = .closed; return false }
        guard phase != .closed else { return false }
        switch event {
        case .pause: phase = .paused; return false
        case .resume, .tryAgain:
            attempts = 0
            phase = .reconnecting
            return true
        case .live:
            guard phase != .paused else { return false }
            attempts = 0
            phase = .viewing
            return false
        case .interrupted:
            guard phase != .paused else { return false }
            phase = .reconnecting
            return false
        case .failed:
            guard phase != .paused else { return false }
            guard attempts < 2 else { phase = .needsRetry; return false }
            attempts += 1
            phase = .reconnecting
            return true
        case .close: return false
        }
    }
}
