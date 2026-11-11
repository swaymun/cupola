import Foundation
import os

/// Per-hop timestamps from asking for a view to its first decoded frame, in
/// milliseconds since `begin`. Written to the unified log (subsystem
/// `com.swaymun.wonder`, category `open-latency`) so a scenario or a person
/// can read where an opening spent its time; it carries no content or IDs.
/// Off until the app turns it on, which only Diagnostics builds do.
public final class ComputerOpenTimeline: @unchecked Sendable {
    public static let shared = ComputerOpenTimeline()
    private let logger = Logger(subsystem: "com.swaymun.wonder", category: "open-latency")
    private let lock = NSLock()
    private var origin: UInt64?
    private var marks: Set<String> = []
    private var isEnabled = false

    public func enable() { lock.withLock { isEnabled = true } }

    /// A new opening starts the clock again (one opening at a time is timed).
    public func begin() {
        guard lock.withLock({ isEnabled }) else { return }
        lock.withLock { origin = DispatchTime.now().uptimeNanoseconds; marks.removeAll() }
        logger.notice("open-latency begin")
    }

    /// Logs the first time each hop is reached in this opening.
    public func mark(_ hop: String) {
        let elapsed: Double? = lock.withLock {
            guard isEnabled, let origin, marks.insert(hop).inserted else { return nil }
            return Double(DispatchTime.now().uptimeNanoseconds - origin) / 1_000_000
        }
        if let elapsed { logger.notice("open-latency \(hop, privacy: .public) \(Int(elapsed), privacy: .public) ms") }
    }
}
