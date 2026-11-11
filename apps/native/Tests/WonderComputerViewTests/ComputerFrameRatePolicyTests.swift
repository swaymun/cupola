import XCTest
@testable import WonderComputerView
import WonderPairing

/// The phone's frame-rate policy: ceiling, defaults, limits, the choices offered and the network steps.
final class ComputerFrameRatePolicyTests: XCTestCase {
    private func policy(_ displayMax: Int, phone: Bool, lowPower: Bool = false, thermal: ProcessInfo.ThermalState = .nominal,
                        platform: String? = nil, quality: ComputerVideoQuality = .auto) -> ComputerFrameRatePolicy {
        ComputerFrameRatePolicy(displayMax: displayMax, isPhone: phone, lowPower: lowPower, thermalState: thermal,
                                computerCap: ComputerFrameRatePolicy.computerCap(platform: platform, quality: quality))
    }

    func testCeilingStartAndChoicesTable() {
        // display, phone, low power, thermal, platform, quality -> ceiling, starting rate, fixed choices offered
        let rows: [(Int, Bool, Bool, ProcessInfo.ThermalState, String?, ComputerVideoQuality, Int, Int, [Int])] = [
            (60, true, false, .nominal, "macos", .auto, 60, 30, [15, 30, 60]),          // iPhone 11
            (120, true, false, .nominal, "macos", .auto, 120, 60, [15, 30, 60, 120]),   // ProMotion iPhone
            (60, false, false, .nominal, "macos", .auto, 60, 60, [15, 30, 60]),         // 60 Hz iPad
            (120, false, false, .nominal, "macos", .standard, 120, 60, [15, 30, 60, 120]),
            (120, true, false, .nominal, "macos", .high, 60, 60, [15, 30, 60, 120]),     // 1440p tops out at 60
            (120, true, false, .nominal, "linux", .auto, 30, 30, [15, 30, 60, 120]),
            (60, true, false, .nominal, "windows", .auto, 30, 30, [15, 30, 60]),
            (60, true, true, .nominal, "macos", .auto, 30, 30, [15, 30, 60]),           // Low Power on 60 Hz: 30
            (120, true, true, .nominal, "macos", .auto, 60, 60, [15, 30, 60, 120]),     // Low Power on ProMotion: 60
            (120, true, false, .fair, "macos", .auto, 120, 60, [15, 30, 60, 120]),
            (120, true, false, .serious, "macos", .auto, 30, 30, [15, 30, 60, 120]),
            (60, true, false, .critical, "macos", .auto, 15, 15, [15, 30, 60]),
            (120, true, true, .critical, "macos", .auto, 15, 15, [15, 30, 60, 120]),
            (90, true, false, .nominal, "macos", .auto, 60, 30, [15, 30, 60]),          // an odd panel rounds down
        ]
        for (index, row) in rows.enumerated() {
            let value = policy(row.0, phone: row.1, lowPower: row.2, thermal: row.3, platform: row.4, quality: row.5)
            XCTAssertEqual(value.ceiling, row.6, "ceiling, row \(index)")
            XCTAssertEqual(value.startingRate, row.7, "start, row \(index)")
            XCTAssertEqual(value.choices, [.auto] + row.8.map { .fixed($0) }, "choices, row \(index)")
        }
        XCTAssertFalse(policy(60, phone: true).choices.contains(.fixed(120)), "no 120 on a 60 Hz display")
    }

    func testChoiceIsCappedByPowerAndComputerAndSaysWhich() {
        let phone = policy(120, phone: true)
        XCTAssertEqual(phone.request(for: .fixed(120), automaticRate: 30).rate, 120)
        XCTAssertNil(phone.request(for: .fixed(120), automaticRate: 30).limit)
        let low = policy(60, phone: true, lowPower: true)
        XCTAssertEqual(low.request(for: .fixed(60), automaticRate: nil).rate, 30)
        XCTAssertEqual(low.request(for: .fixed(60), automaticRate: nil).limit, .lowPower)
        XCTAssertNil(low.request(for: .fixed(30), automaticRate: nil).limit, "not cut short, so nothing to say")
        let hot = policy(120, phone: true, lowPower: true, thermal: .serious)
        XCTAssertEqual(hot.request(for: .fixed(60), automaticRate: nil).rate, 30)
        XCTAssertEqual(hot.request(for: .fixed(60), automaticRate: nil).limit, .thermal)
        let linux = policy(120, phone: true, platform: "linux")
        XCTAssertEqual(linux.request(for: .fixed(60), automaticRate: nil).rate, 30)
        XCTAssertEqual(linux.request(for: .fixed(60), automaticRate: nil).limit, .computer)
        // A fixed 120 asked on a 60 Hz display is held at 60 without a note: it was never offered.
        XCTAssertEqual(policy(60, phone: true).request(for: .fixed(120), automaticRate: nil).rate, 60)
        XCTAssertNil(policy(60, phone: true).request(for: .fixed(120), automaticRate: nil).limit)
        // Auto follows its step, under the ceiling, and never explains itself.
        XCTAssertEqual(low.request(for: .auto, automaticRate: 60).rate, 30)
        XCTAssertNil(low.request(for: .auto, automaticRate: 60).limit)
        XCTAssertEqual(policy(60, phone: true).request(for: .auto, automaticRate: nil).rate, 30)
    }

    func testChoiceSavesAndRestores() {
        for choice in [ComputerFrameRateChoice.auto, .fixed(15), .fixed(30), .fixed(60), .fixed(120)] {
            XCTAssertEqual(ComputerFrameRateChoice(storageValue: choice.storageValue), choice)
        }
        XCTAssertNil(ComputerFrameRateChoice(storageValue: "45"))
        XCTAssertNil(ComputerFrameRateChoice(storageValue: nil))
    }

    func testChoppyIsMeasuredAgainstTheCeiling() {
        func quality(_ fps: Double, ceiling: Int?) -> ComputerStreamQuality {
            ComputerStreamQuality.classify(framesPerSecond: fps, framesDropped: 5, lossFraction: 0, roundTripMilliseconds: 20,
                                           frameRateCeiling: ceiling)
        }
        XCTAssertEqual(quality(36, ceiling: 60), .good)     // the floor is 0.6 × 60 = 36
        XCTAssertEqual(quality(35.9, ceiling: 60), .choppy)
        XCTAssertEqual(quality(25, ceiling: 30), .good)     // 18 is the floor at 30
        XCTAssertEqual(quality(17, ceiling: 30), .choppy)
        XCTAssertEqual(quality(8, ceiling: nil), .choppy)   // a computer without the field publishes 15 → 9
        XCTAssertEqual(quality(10, ceiling: nil), .good)
    }

    // MARK: Network steps

    private let good = ComputerNetworkSample(seconds: 2, lossFraction: 0, roundTripMilliseconds: 20, framesDropped: 0)
    private let bad = ComputerNetworkSample(seconds: 2, lossFraction: 0.03, roundTripMilliseconds: 20, framesDropped: 0)

    func testStepsDownAfterTwoBadSamplesAndUpAfterTenGoodSeconds() {
        var governor = ComputerFrameRateGovernor(rate: 60)
        XCTAssertEqual(governor.observe(bad, ceiling: 120), 60, "one bad sample is not enough")
        XCTAssertEqual(governor.observe(good, ceiling: 120), 60, "a good one clears it")
        XCTAssertEqual(governor.observe(bad, ceiling: 120), 60)
        XCTAssertEqual(governor.observe(bad, ceiling: 120), 30)
        XCTAssertEqual(governor.observe(bad, ceiling: 120), 30)
        XCTAssertEqual(governor.observe(bad, ceiling: 120), 15)
        XCTAssertEqual(governor.observe(bad, ceiling: 120), 15)
        XCTAssertEqual(governor.observe(bad, ceiling: 120), 15, "15 is the floor")
        for _ in 0..<4 { XCTAssertEqual(governor.observe(good, ceiling: 120), 15) }
        XCTAssertEqual(governor.observe(good, ceiling: 120), 30, "five good samples is ten seconds")
        for _ in 0..<4 { XCTAssertEqual(governor.observe(good, ceiling: 120), 30) }
        XCTAssertEqual(governor.observe(good, ceiling: 120), 60)
    }

    func testStepUpStopsAtTheCeilingAndNeedsEveryGoodMeasure() {
        var governor = ComputerFrameRateGovernor(rate: 30)
        for _ in 0..<20 { governor.observe(good, ceiling: 60) }
        XCTAssertEqual(governor.rate, 60)
        // Slow round trip, a lossy interval of 1-2%, or two dropped frames are neither good nor bad.
        let neutral = [ComputerNetworkSample(seconds: 2, lossFraction: 0, roundTripMilliseconds: 80, framesDropped: 0),
                       ComputerNetworkSample(seconds: 2, lossFraction: 0.015, roundTripMilliseconds: 20, framesDropped: 0),
                       ComputerNetworkSample(seconds: 2, lossFraction: 0, roundTripMilliseconds: 20, framesDropped: 2),
                       ComputerNetworkSample(seconds: 2, lossFraction: 0, roundTripMilliseconds: nil, framesDropped: 0)]
        for sample in neutral {
            var held = ComputerFrameRateGovernor(rate: 30)
            for _ in 0..<20 { held.observe(sample, ceiling: 120) }
            XCTAssertEqual(held.rate, 30, "\(sample)")
        }
        // Four good then one neutral starts the ten seconds over.
        var restarted = ComputerFrameRateGovernor(rate: 30)
        for _ in 0..<4 { restarted.observe(good, ceiling: 120) }
        restarted.observe(neutral[0], ceiling: 120)
        for _ in 0..<4 { restarted.observe(good, ceiling: 120) }
        XCTAssertEqual(restarted.rate, 30)
        restarted.observe(good, ceiling: 120)
        XCTAssertEqual(restarted.rate, 60)
    }

    func testDroppedFramesCountAsBadAndTheCeilingPullsTheRateDown() {
        var governor = ComputerFrameRateGovernor(rate: 60)
        let dropping = ComputerNetworkSample(seconds: 2, lossFraction: 0, roundTripMilliseconds: 20, framesDropped: 4)
        governor.observe(dropping, ceiling: 120)
        XCTAssertEqual(governor.observe(dropping, ceiling: 120), 30)
        // A thermal or Low Power ceiling below the rate takes effect on the next interval.
        var hot = ComputerFrameRateGovernor(rate: 120)
        XCTAssertEqual(hot.observe(good, ceiling: 30), 30)
        XCTAssertEqual(hot.observe(good, ceiling: 15), 15)
        // The samples are per interval: a 4-second interval with 4 drops is 2 per 2 s, not bad.
        var long = ComputerFrameRateGovernor(rate: 60)
        let spread = ComputerNetworkSample(seconds: 4, lossFraction: 0, roundTripMilliseconds: 20, framesDropped: 4)
        long.observe(spread, ceiling: 120); long.observe(spread, ceiling: 120)
        XCTAssertEqual(long.rate, 60)
    }
}
