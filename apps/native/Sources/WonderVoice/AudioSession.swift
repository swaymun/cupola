import Foundation

// One owner for the phone's shared audio session. Dictation, voice mode and
// audio or video in a conversation each claim it for what they need and give
// it back when done. A claim made earlier and stopped later (a stale capture
// completion, a late stop) never deactivates or reconfigures a newer owner.

/// What the shared audio session is being used for.
public enum AudioSessionUse: Equatable, Sendable {
    /// Dictation in the composer: recording only.
    case dictation
    /// Voice mode: recording and playback with echo cancellation.
    case voice
    /// Audio or video in a conversation: playback only.
    case media
    /// A reply read aloud: spoken playback that ducks other audio.
    case speech

    /// Dictation and voice use the microphone.
    public var captures: Bool { self == .dictation || self == .voice }
}

/// The system audio session (AVAudioSession on iOS; a recorder in tests).
public protocol AudioSessionDevice: AnyObject, Sendable {
    /// Sets the category for `use` and activates the session.
    func activate(for use: AudioSessionUse) throws
    /// Deactivates the session so other apps' audio can resume.
    func deactivate()
}

public final class AudioSessionOwner: @unchecked Sendable {
    public struct Lease: Hashable, Sendable {
        public let use: AudioSessionUse
        let id: Int
    }

    public enum Refusal: Error, Equatable {
        /// Playback cannot take the session from dictation or voice.
        case captureInProgress
    }

    private let device: any AudioSessionDevice
    private let lock = NSLock()
    private var holder: (lease: Lease, preempted: @Sendable (Lease) -> Void)?
    private var next = 0

    public init(device: any AudioSessionDevice) { self.device = device }

    public var current: Lease? { lock.withLock { holder?.lease } }

    public func holds(_ lease: Lease?) -> Bool {
        guard let lease else { return false }
        return lock.withLock { holder?.lease == lease }
    }

    /// Claims the session for `use` and configures it. Dictation and voice
    /// take it from anyone; media and speech take it only from each other. The
    /// previous owner hears through its `preempted` handler, and its later
    /// release does nothing. If configuring fails, the previous owner keeps
    /// it with its own configuration put back; if that fails too, its claim
    /// ends (it hears through `preempted`) and the session is deactivated, so
    /// no owner is left holding a session set up for another use.
    public func acquire(_ use: AudioSessionUse, preempted: @escaping @Sendable (Lease) -> Void) throws -> Lease {
        typealias Holder = (lease: Lease, preempted: @Sendable (Lease) -> Void)
        let (outcome, ended) = lock.withLock { () -> (Result<Lease, any Error>, Holder?) in
            if !use.captures, holder?.lease.use.captures == true { return (.failure(Refusal.captureInProgress), nil) }
            do {
                try device.activate(for: use)
            } catch {
                // The device may be half changed (category set, activation refused).
                guard let previous = holder else { return (.failure(error), nil) }
                if (try? device.activate(for: previous.lease.use)) != nil { return (.failure(error), nil) }
                holder = nil
                device.deactivate()
                return (.failure(error), previous)
            }
            next += 1
            let lease = Lease(use: use, id: next)
            let previous = holder
            holder = (lease, preempted)
            return (.success(lease), previous)
        }
        ended.map { $0.preempted($0.lease) }
        return try outcome.get()
    }

    /// Activates the session again for an automatic resume (an interruption
    /// ending, a new route). Returns false, changing nothing, when a newer
    /// owner holds it.
    public func reactivate(_ lease: Lease) throws -> Bool {
        try lock.withLock {
            guard holder?.lease == lease else { return false }
            try device.activate(for: lease.use)
            return true
        }
    }

    /// Gives the session up if `lease` still holds it, so other apps' audio
    /// resumes. A stale or missing lease changes nothing.
    @discardableResult public func release(_ lease: Lease?) -> Bool {
        lock.withLock {
            guard let lease, holder?.lease == lease else { return false }
            holder = nil
            device.deactivate()
            return true
        }
    }
}

/// Where paused conversation audio and video stopped, so a row that scrolls
/// away and returns resumes there. Bounded; the least recently kept go first.
public struct MediaResumePositions: Sendable {
    public let limit: Int
    private var order: [String] = []
    private var positions: [String: Double] = [:]

    public init(limit: Int = 32) { self.limit = max(1, limit) }

    public var count: Int { order.count }

    public subscript(key: String) -> Double? { positions[key] }

    /// Keeps `seconds` for `key`. The start or the last half second (a
    /// finished item) is not kept, so the item plays from the beginning.
    public mutating func keep(_ seconds: Double, for key: String, duration: Double) {
        remove(key)
        guard seconds.isFinite, seconds > 0.5, duration <= 0 || seconds < duration - 0.5 else { return }
        positions[key] = seconds
        order.append(key)
        while order.count > limit { positions[order.removeFirst()] = nil }
    }

    public mutating func remove(_ key: String) {
        guard positions.removeValue(forKey: key) != nil else { return }
        order.removeAll { $0 == key }
    }

    public mutating func removeAll() { order = []; positions = [:] }
}
