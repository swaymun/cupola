import Foundation
import Testing
@testable import WonderPairing

struct EnrollmentPollTests {
    @Test func pollDecisions() {
        let live: UInt64 = 1_000, end: UInt64 = 2_000
        let cases: [(Error, UInt64, EnrollmentPoll.Decision)] = [
            (PairingFailure.response(409), live, .keepPolling),
            (URLError(.networkConnectionLost), live, .keepPolling),
            (URLError(.timedOut), live, .keepPolling),
            (PairingFailure.response(503), live, .keepPolling),
            (PairingFailure.response(403), live, .fail),
            (PairingFailure.response(404), live, .fail),
            (PairingFailure.wrongHost, live, .fail),
            (URLError(.networkConnectionLost), end, .fail),
            (PairingFailure.response(409), end + 1, .fail),
        ]
        for (error, now, expected) in cases {
            #expect(EnrollmentPoll.decision(after: error, nowMs: now, expiresAtMs: end) == expected, "\(error) at \(now)")
        }
    }

    @Test func forgottenDeviceRenewalIsAccessEnded() {
        for status in [404, 410, 401] {
            guard case PairingFailure.response(401) = EnrollmentPoll.renewalFailure(PairingFailure.response(status)) else {
                Issue.record("\(status) not mapped to 401"); continue
            }
        }
        guard case PairingFailure.response(500) = EnrollmentPoll.renewalFailure(PairingFailure.response(500)) else {
            Issue.record("500 changed"); return
        }
        #expect(EnrollmentPoll.renewalFailure(URLError(.timedOut)) is URLError)
    }
}
