import Foundation
import Testing
@testable import WonderVoice

/// Records what the owner asked of the system audio session.
private final class RecordedSession: AudioSessionDevice, @unchecked Sendable {
    enum Call: Equatable { case activate(AudioSessionUse), deactivate }
    private let lock = NSLock()
    private var recorded: [Call] = []
    private var configured: AudioSessionUse?
    private var failures = 0
    private var partial = false
    var calls: [Call] { lock.withLock { recorded } }
    /// What the session is set up for, as AVAudioSession would report it.
    var category: AudioSessionUse? { lock.withLock { configured } }
    /// The next `count` activations fail; with `afterCategory` they change
    /// the category first, as AVAudioSession does when setActive is refused.
    func fail(_ count: Int, afterCategory: Bool) { lock.withLock { failures = count; partial = afterCategory } }
    func activate(for use: AudioSessionUse) throws {
        try lock.withLock {
            if failures > 0 {
                failures -= 1
                if partial { configured = use }
                throw CocoaError(.featureUnsupported)
            }
            configured = use
            recorded.append(.activate(use))
        }
    }
    func deactivate() { lock.withLock { recorded.append(.deactivate) } }
}

private final class Preemptions: @unchecked Sendable {
    private let lock = NSLock()
    private var leases: [AudioSessionOwner.Lease] = []
    var all: [AudioSessionOwner.Lease] { lock.withLock { leases } }
    func record(_ lease: AudioSessionOwner.Lease) { lock.withLock { leases.append(lease) } }
}

@Suite struct AudioSessionTests {
    /// Media after dictation or voice configures playback itself instead of
    /// inheriting the recording category the capture left behind.
    @Test func mediaAfterCaptureConfiguresPlayback() throws {
        for capture in [AudioSessionUse.dictation, .voice] {
            let device = RecordedSession(), owner = AudioSessionOwner(device: device)
            let lease = try owner.acquire(capture) { _ in }
            #expect(owner.release(lease))
            let media = try owner.acquire(.media) { _ in }
            #expect(owner.holds(media))
            #expect(device.calls == [.activate(capture), .deactivate, .activate(.media)])
        }
    }

    /// A late stop from an earlier capture never deactivates a newer owner.
    @Test func staleReleaseKeepsNewerOwner() throws {
        let device = RecordedSession(), owner = AudioSessionOwner(device: device), preempted = Preemptions()
        let dictation = try owner.acquire(.dictation) { preempted.record($0) }
        let voice = try owner.acquire(.voice) { preempted.record($0) }
        #expect(preempted.all == [dictation])
        #expect(!owner.release(dictation))
        #expect(try !owner.reactivate(dictation))
        #expect(owner.holds(voice) && !owner.holds(dictation))
        #expect(!device.calls.contains(.deactivate))
        #expect(owner.release(voice))
        #expect(!owner.release(voice))
        #expect(device.calls == [.activate(.dictation), .activate(.voice), .deactivate])
    }

    /// Playing chat media or reading aloud during dictation or voice is
    /// refused without touching the session; a capture starting stops either.
    @Test func captureOutranksPlayback() throws {
        for playback in [AudioSessionUse.media, .speech] {
            let device = RecordedSession(), owner = AudioSessionOwner(device: device), preempted = Preemptions()
            let played = try owner.acquire(playback) { preempted.record($0) }
            let dictation = try owner.acquire(.dictation) { _ in }
            #expect(preempted.all == [played])
            #expect(throws: AudioSessionOwner.Refusal.captureInProgress) { try owner.acquire(playback) { _ in } }
            #expect(owner.holds(dictation))
            #expect(device.calls == [.activate(playback), .activate(.dictation)])
        }
    }

    /// Reading aloud and chat media hand the session to each other.
    @Test func speechAndMediaTakeTurns() throws {
        let device = RecordedSession(), owner = AudioSessionOwner(device: device), preempted = Preemptions()
        let speech = try owner.acquire(.speech) { preempted.record($0) }
        let media = try owner.acquire(.media) { preempted.record($0) }
        let again = try owner.acquire(.speech) { _ in }
        #expect(preempted.all == [speech, media])
        #expect(!owner.release(speech) && owner.holds(again))
        #expect(device.calls == [.activate(.speech), .activate(.media), .activate(.speech)])
    }

    /// Starting another item pauses the one that was playing: one at a time.
    @Test func newerMediaPreemptsOlder() throws {
        let owner = AudioSessionOwner(device: RecordedSession()), preempted = Preemptions()
        let first = try owner.acquire(.media) { preempted.record($0) }
        let second = try owner.acquire(.media) { preempted.record($0) }
        #expect(preempted.all == [first] && owner.holds(second))
    }

    /// A capture that could not configure the session leaves the previous
    /// owner in place and unnotified, with its own configuration.
    @Test func failedActivationKeepsPreviousOwner() throws {
        for afterCategory in [false, true] {
            let device = RecordedSession(), owner = AudioSessionOwner(device: device), preempted = Preemptions()
            let media = try owner.acquire(.media) { preempted.record($0) }
            device.fail(1, afterCategory: afterCategory)
            #expect(throws: (any Error).self) { try owner.acquire(.dictation) { _ in } }
            #expect(owner.holds(media) && preempted.all.isEmpty)
            #expect(device.category == .media, "Media keeps playback, not dictation's recording setup")
        }
    }

    /// When the previous owner's configuration can't be put back either, its
    /// claim ends and it hears so (media pauses), rather than holding a
    /// session set up for recording.
    @Test func failedHandoffThatCannotRestoreEndsThePreviousClaim() throws {
        let device = RecordedSession(), owner = AudioSessionOwner(device: device), preempted = Preemptions()
        let media = try owner.acquire(.media) { preempted.record($0) }
        device.fail(2, afterCategory: true)
        #expect(throws: (any Error).self) { try owner.acquire(.voice) { _ in } }
        #expect(!owner.holds(media) && owner.current == nil)
        #expect(preempted.all == [media])
        #expect(device.calls.last == .deactivate)
        #expect(!owner.release(media), "Its later release changes nothing")
        let next = try owner.acquire(.media) { _ in }
        #expect(owner.holds(next) && device.category == .media)
    }

    /// Paused positions survive a row leaving, are bounded, and a finished
    /// or barely started item starts again from the beginning.
    @Test func resumePositionsAreBounded() {
        var positions = MediaResumePositions(limit: 2)
        positions.keep(12, for: "a", duration: 60)
        positions.keep(20, for: "b", duration: 60)
        positions.keep(30, for: "c", duration: 60)
        #expect(positions["a"] == nil && positions["b"] == 20 && positions["c"] == 30 && positions.count == 2)
        positions.keep(59.8, for: "b", duration: 60)
        positions.keep(0.2, for: "c", duration: 60)
        #expect(positions["b"] == nil && positions["c"] == nil && positions.count == 0)
        positions.keep(5, for: "d", duration: 0)
        #expect(positions["d"] == 5)
        positions.removeAll()
        #expect(positions.count == 0)
    }
}
