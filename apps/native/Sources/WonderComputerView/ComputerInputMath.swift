import CoreGraphics
import Foundation
import WonderPairing

/// Trackpad pointer acceleration: slow finger movement stays 1:1 for precise
/// targeting; a quick flick crosses the screen without lifting the finger.
public enum ComputerPointerAcceleration {
    /// Below this finger speed (points per second) the pointer follows 1:1.
    public static let preciseSpeed: CGFloat = 220
    /// At and above this speed the gain is `maximumGain`.
    public static let fastSpeed: CGFloat = 1_600
    public static let maximumGain: CGFloat = 2.4

    public static func gain(speed: CGFloat) -> CGFloat {
        guard speed.isFinite, speed > preciseSpeed else { return 1 }
        let progress = min((speed - preciseSpeed) / (fastSpeed - preciseSpeed), 1)
        // Ease in so medium speeds stay controllable.
        return 1 + (maximumGain - 1) * progress * progress
    }

    /// Scales a finger delta measured over `interval` seconds.
    public static func accelerated(_ delta: CGSize, interval: TimeInterval) -> CGSize {
        guard delta.width.isFinite, delta.height.isFinite else { return .zero }
        guard interval.isFinite, interval > 0 else { return delta }
        let distance = (delta.width * delta.width + delta.height * delta.height).squareRoot()
        let factor = gain(speed: distance / CGFloat(max(interval, 1.0 / 240)))
        return CGSize(width: delta.width * factor, height: delta.height * factor)
    }
}

/// Inertial scrolling after a two-finger flick, with the same deceleration
/// as a UIKit scroll view (`UIScrollView.DecelerationRate.normal`).
public struct ComputerScrollMomentum: Sendable {
    /// Velocity retained per millisecond.
    public static let decelerationPerMillisecond: Double = 0.998
    /// A flick slower than this (points per second) does not coast.
    public static let minimumStartSpeed: Double = 160
    /// Coasting stops below this speed.
    public static let stopSpeed: Double = 25
    public static let maximumSpeed: Double = 6_000

    public private(set) var velocity: CGVector

    /// Nil when the release was too slow to coast.
    public init?(releaseVelocity: CGVector) {
        guard releaseVelocity.dx.isFinite, releaseVelocity.dy.isFinite else { return nil }
        let speed = Double((releaseVelocity.dx * releaseVelocity.dx + releaseVelocity.dy * releaseVelocity.dy).squareRoot())
        guard speed >= Self.minimumStartSpeed else { return nil }
        let scale = speed > Self.maximumSpeed ? Self.maximumSpeed / speed : 1
        velocity = CGVector(dx: releaseVelocity.dx * scale, dy: releaseVelocity.dy * scale)
    }

    public var isFinished: Bool {
        Double((velocity.dx * velocity.dx + velocity.dy * velocity.dy).squareRoot()) < Self.stopSpeed
    }

    /// The scroll distance covered in the next `interval` seconds, or nil once
    /// coasting has finished. Integrates the exponential decay exactly, so the
    /// total distance does not depend on the display's frame rate.
    public mutating func step(interval: TimeInterval) -> CGSize? {
        guard !isFinished, interval.isFinite, interval > 0 else { return nil }
        let milliseconds = min(interval, 0.1) * 1_000
        let retained = pow(Self.decelerationPerMillisecond, milliseconds)
        // ∫ v·r^t dt over the interval, with t in milliseconds.
        let distanceFactor = (retained - 1) / log(Self.decelerationPerMillisecond) / 1_000
        let delta = CGSize(width: velocity.dx * distanceFactor, height: velocity.dy * distanceFactor)
        velocity = CGVector(dx: velocity.dx * retained, dy: velocity.dy * retained)
        return delta
    }
}

/// How smooth the live view is, in words a person can act on.
public enum ComputerStreamQuality: Equatable, Sendable {
    case good
    case choppy
    case slow

    /// Shown only when the view is struggling; a smooth view says nothing.
    public var label: String? {
        switch self {
        case .good: nil
        case .choppy: "Choppy"
        case .slow: "Slow connection"
        }
    }

    public var detail: String? {
        switch self {
        case .good: nil
        case .choppy: "The live view is dropping frames. Moving closer to your Wi-Fi router can help."
        case .slow: "Your computer is slow to respond. Check that both devices are on a strong Wi-Fi network."
        }
    }

    /// Classifies a few seconds of receiver measurements. `framesPerSecond` is
    /// decoded frames per second, `roundTripMilliseconds` the network round
    /// trip to the Mac, `lossFraction` the share of video packets lost.
    /// A still screen sends few frames, so a low frame rate alone only counts
    /// when frames were also dropped or lost.
    /// Choppy means under `choppyShare` of the frame rate the phone asked the
    /// computer for, with frames dropped; `frameRateCeiling` nil means the 15
    /// fps a computer without the field publishes.
    public static let choppyShare = 0.6

    public static func classify(framesPerSecond: Double?, framesDropped: Double, lossFraction: Double,
                                roundTripMilliseconds: Double?, frameRateCeiling: Int? = nil) -> ComputerStreamQuality {
        if let roundTrip = roundTripMilliseconds, roundTrip.isFinite, roundTrip > 250 { return .slow }
        if lossFraction.isFinite, lossFraction > 0.08 { return .slow }
        if lossFraction.isFinite, lossFraction > 0.02 { return .choppy }
        let floor = choppyShare * Double(frameRateCeiling ?? ComputerFrameRate.legacy)
        if framesDropped.isFinite, framesDropped > 3,
           let fps = framesPerSecond, fps.isFinite, fps < floor { return .choppy }
        return .good
    }
}

/// The frame rates a phone can ask a computer to publish at, and which one it asks for.
public enum ComputerFrameRate {
    /// Steps the ceiling moves between, lowest first.
    public static let steps = [15, 30, 60, 120]
    /// What a computer that predates the field publishes.
    public static let legacy = 15

    /// The highest step the display can show: 60 on a 60 Hz panel, 120 on ProMotion.
    public static func deviceCeiling(displayMax: Int) -> Int {
        steps.last { $0 <= max(displayMax, steps[0]) } ?? steps[0]
    }

    /// Where Auto starts: ProMotion 60, a 60 Hz iPad 60, a 60 Hz iPhone 30.
    public static func defaultRate(displayMax: Int, isPhone: Bool) -> Int {
        deviceCeiling(displayMax: displayMax) >= 120 ? 60 : (isPhone ? 30 : 60)
    }

    /// The step below `rate`, or 15.
    static func lower(than rate: Int) -> Int { steps.last { $0 < rate } ?? steps[0] }
    /// The step above `rate`, or the top step.
    static func higher(than rate: Int) -> Int { steps.first { $0 > rate } ?? steps[steps.count - 1] }
    static func step(atMost rate: Int) -> Int { steps.last { $0 <= rate } ?? steps[0] }
}

/// What holds the frame rate below what was asked for.
public enum ComputerFrameRateLimit: Equatable, Sendable {
    case lowPower
    case thermal
    case computer
}

/// A person's frame-rate choice, saved like the resolution choice.
public enum ComputerFrameRateChoice: Hashable, Sendable {
    case auto
    case fixed(Int)

    public var title: String {
        switch self {
        case .auto: "Auto"
        case .fixed(let rate): "\(rate) fps"
        }
    }

    public var storageValue: String {
        switch self {
        case .auto: "auto"
        case .fixed(let rate): String(rate)
        }
    }

    public init?(storageValue: String?) {
        guard let storageValue else { return nil }
        if storageValue == "auto" { self = .auto; return }
        guard let rate = Int(storageValue), ComputerFrameRate.steps.contains(rate) else { return nil }
        self = .fixed(rate)
    }
}

/// The frame rate to ask for: the display, the computer, Low Power Mode and
/// heat set a ceiling; Auto starts at the class default and moves under it.
public struct ComputerFrameRatePolicy: Equatable, Sendable {
    public var displayMax: Int
    public var isPhone: Bool
    public var lowPower = false
    public var thermalState = ProcessInfo.ThermalState.nominal
    /// What the computer can publish at the chosen resolution; see `computerCap`.
    public var computerCap = 120

    public init(displayMax: Int, isPhone: Bool, lowPower: Bool = false,
                thermalState: ProcessInfo.ThermalState = .nominal, computerCap: Int = 120) {
        self.displayMax = displayMax; self.isPhone = isPhone; self.lowPower = lowPower
        self.thermalState = thermalState; self.computerCap = computerCap
    }

    /// Linux and Windows computers encode in software (VP8): 30. A Mac encodes
    /// 120 up to 1080p and 60 at 1440p. `videoQuality` is the chosen resolution.
    public static func computerCap(platform: String?, quality: ComputerVideoQuality) -> Int {
        switch platform {
        case "linux", "windows": 30
        default: quality == .high ? 60 : 120
        }
    }

    /// Frame rates the person may choose; none above what the display shows.
    public var choices: [ComputerFrameRateChoice] {
        [.auto] + ComputerFrameRate.steps.filter { $0 <= ComputerFrameRate.deviceCeiling(displayMax: displayMax) }.map { .fixed($0) }
    }

    public var defaultRate: Int { ComputerFrameRate.defaultRate(displayMax: displayMax, isPhone: isPhone) }

    /// Low Power Mode and heat, with the one that bites harder; nil when neither limits.
    public var powerCap: (rate: Int, limit: ComputerFrameRateLimit)? {
        var cap: (rate: Int, limit: ComputerFrameRateLimit)?
        if lowPower { cap = (ComputerFrameRate.deviceCeiling(displayMax: displayMax) >= 120 ? 60 : 30, .lowPower) }
        let heat: Int? = switch thermalState {
        case .serious: 30
        case .critical: 15
        default: nil
        }
        if let heat, heat <= (cap?.rate ?? Int.max) { cap = (heat, .thermal) }
        return cap
    }

    /// The most that can be asked for right now: the display, the computer and the power state.
    public var ceiling: Int {
        min(ComputerFrameRate.deviceCeiling(displayMax: displayMax), ComputerFrameRate.step(atMost: computerCap),
            powerCap?.rate ?? .max)
    }

    /// Where Auto starts, under the ceiling.
    public var startingRate: Int { min(defaultRate, ceiling) }

    /// The ceiling to send. `automaticRate` is Auto's current step; a fixed
    /// choice holds, under the limits, and names what held it down.
    public func request(for choice: ComputerFrameRateChoice, automaticRate: Int?) -> (rate: Int, limit: ComputerFrameRateLimit?) {
        switch choice {
        case .auto:
            return (min(automaticRate ?? startingRate, ceiling), nil)
        case .fixed(let wanted):
            let allowed = min(wanted, ComputerFrameRate.deviceCeiling(displayMax: displayMax))
            let rate = min(allowed, ceiling)
            guard rate < allowed else { return (rate, nil) }
            if let power = powerCap, power.rate <= rate { return (rate, power.limit) }
            return (rate, .computer)
        }
    }

    /// A short plain sentence for a limited choice; `device` is "iPhone" or "iPad", `computer` the computer's name.
    public static func note(for limit: ComputerFrameRateLimit, rate: Int, device: String, computer: String) -> String {
        switch limit {
        case .lowPower: "Low Power Mode limits the view to \(rate) fps."
        case .thermal: "This \(device) is warm, so the view is limited to \(rate) fps."
        case .computer: "\(computer) can send up to \(rate) fps."
        }
    }
}

/// What the network did over one statistics interval.
public struct ComputerNetworkSample: Equatable, Sendable {
    public var seconds: Double
    public var lossFraction: Double
    public var roundTripMilliseconds: Double?
    public var framesDropped: Double

    public init(seconds: Double, lossFraction: Double, roundTripMilliseconds: Double?, framesDropped: Double) {
        self.seconds = seconds; self.lossFraction = lossFraction
        self.roundTripMilliseconds = roundTripMilliseconds; self.framesDropped = framesDropped
    }

    /// Loss under 1%, round trip under 60 ms and at most one dropped frame per 2 s.
    var isGood: Bool {
        guard lossFraction < 0.01, droppedPerTwoSeconds <= 1, let roundTrip = roundTripMilliseconds else { return false }
        return roundTrip < 60
    }

    /// Loss over 2%, more than three dropped frames per 2 s, or a round trip over 250 ms.
    var isBad: Bool {
        lossFraction > 0.02 || droppedPerTwoSeconds > 3 || (roundTripMilliseconds ?? 0) > 250
    }

    private var droppedPerTwoSeconds: Double { framesDropped * 2 / max(seconds, 0.001) }
}

/// Moves Auto's frame rate one step at a time: down after two bad intervals in
/// a row, up after ten seconds of good ones, never above the ceiling.
public struct ComputerFrameRateGovernor: Equatable, Sendable {
    public static let goodSecondsToStepUp = 10.0
    public static let badSamplesToStepDown = 2

    public private(set) var rate: Int
    private var goodSeconds = 0.0
    private var badSamples = 0

    public init(rate: Int) { self.rate = rate }

    public mutating func restart(at rate: Int) {
        self.rate = rate; goodSeconds = 0; badSamples = 0
    }

    /// Returns the rate to ask for after this interval.
    @discardableResult
    public mutating func observe(_ sample: ComputerNetworkSample, ceiling: Int) -> Int {
        if rate > ceiling { restart(at: ComputerFrameRate.step(atMost: ceiling)); return rate }
        if sample.isBad {
            goodSeconds = 0
            badSamples += 1
            if badSamples >= Self.badSamplesToStepDown {
                badSamples = 0
                rate = ComputerFrameRate.lower(than: rate)
            }
        } else {
            badSamples = 0
            goodSeconds = sample.isGood ? goodSeconds + sample.seconds : 0
            if goodSeconds >= Self.goodSecondsToStepUp {
                goodSeconds = 0
                rate = min(ComputerFrameRate.higher(than: rate), ComputerFrameRate.step(atMost: ceiling))
            }
        }
        return rate
    }
}
