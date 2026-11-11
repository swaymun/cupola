import Foundation
import XCTest
import CryptoKit
import WonderPairing
import WonderComputerView
@preconcurrency import WebRTC
#if os(iOS)
@testable import Wonder
#else
@testable import WonderRouteSelector
#endif

/// Fast builds remap #filePath for a shared compiler cache. The test runner
/// supplies this checkout at execution time; fixture bytes remain independent.
enum DiagnosticTestResources {
    static func url(_ relative: String) throws -> URL {
        let root = ProcessInfo.processInfo.environment["WONDER_TEST_SOURCE_ROOT"].map {
            URL(fileURLWithPath: $0, isDirectory: true)
        } ?? URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
        let result = root.appendingPathComponent(relative)
        guard FileManager.default.fileExists(atPath: result.path) else {
            throw NSError(domain: "DiagnosticTestResources", code: 1, userInfo: [NSLocalizedDescriptionKey:
                "Missing independent fixture \(relative). Run with wonderctl test, or set WONDER_TEST_SOURCE_ROOT in the test environment."])
        }
        return result
    }
}

/// Observable contract: connect to a home address that answers, otherwise the
/// first authenticated responder, bound unreachable addresses, and keep one
/// contiguous replay cursor during a move.
/// The fake clock makes deadlines and ordering deterministic without a simulator.
@MainActor final class ConnectionRouteSelectorTests: XCTestCase {
    private let home = ConnectionRoute(kind: .home, origin: "https://192.168.1.20:3778")
    private let direct = ConnectionRoute(kind: .tailscale, origin: "https://studio.tail.ts.net")
    /// A second remote route raced alongside the others; the real relay
    /// (`cupolaRelay`) waits until every other route has failed.
    private let relay = ConnectionRoute(kind: .tailscale, origin: "https://opaque.relay.invalid")
    private let cupolaRelay = ConnectionRoute(kind: .relay, origin: "https://r-abcdefghijklmnopqrstuvwxyz.getcupola.app")

    private func waitUntil(_ predicate: () async -> Bool) async throws {
        for _ in 0..<10_000 {
            if await predicate() { return }
            await Task.yield()
        }
        XCTFail("Concurrent probe did not reach its expected state")
        throw URLError(.timedOut)
    }

    private func routeRequest(earlierOnly: Bool = false, excluding: String? = nil, origin: String? = nil) -> ConnectionRouteChecks.Request {
        ConnectionRouteChecks.Request(generation: UUID(uuidString: "00000000-0000-0000-0000-000000000001")!,
            routes: [home, relay], origin: origin ?? relay.origin, hostKeyPin: "pin", sessionToken: "session",
            localNetworkAvailable: true, earlierOnly: earlierOnly, excluding: excluding)
    }

    /// A timer's normal "no earlier route" result must not fail Check connection.
    func testForegroundCheckDoesNotInheritAnInFlightSwitchBackMiss() async throws {
        let clock = RouteTestClock()
        let probe = RouteTestProber(clock: clock, delays: [home.origin: .seconds(100), relay.origin: .milliseconds(40)])
        let selector = ConnectionRouteSelector(clock: clock, probe: probe.run)
        let checks = ConnectionRouteChecks()
        let quietRequest = routeRequest(earlierOnly: true), connectRequest = routeRequest()
        let quiet = Task {
            await checks.run(request: { quietRequest }) { request in
                (try? await selector.switchBack(request.routes, currentOrigin: request.origin,
                    localNetworkAvailable: request.localNetworkAvailable)) != nil
            }
        }
        try await waitUntil { clock.sleepers == 2 }
        var requested = false, finished = false
        let connect = Task {
            let result = await checks.run(request: { requested = true; return connectRequest }) { request in
                (try? await selector.select(request.routes, localNetworkAvailable: request.localNetworkAvailable,
                    excluding: request.excluding)) != nil
            }
            finished = true
            return result
        }
        try await waitUntil { requested }
        clock.advance(by: .milliseconds(1500))
        let quietResult = await quiet.value
        XCTAssertFalse(quietResult)
        try await waitUntil {
            let relayCalls = await probe.callCount(self.relay.origin)
            return finished || (relayCalls == 1 && clock.sleepers == 4)
        }
        clock.advance(by: .milliseconds(40))
        try await waitUntil { clock.sleepers == 2 }
        clock.advance(by: .milliseconds(1460)) // The silent home address reaches its deadline.
        let connectResult = await connect.value
        XCTAssertTrue(connectResult, "The working route must answer this caller's full connection check")
        let relayCalls = await probe.callCount(relay.origin)
        XCTAssertEqual(relayCalls, 1, "A quiet miss cannot substitute for probing the current route")
        XCTAssertEqual(clock.sleepers, 0)
    }

    func testEquivalentRouteChecksShareOneProbeAndResult() async throws {
        let clock = RouteTestClock()
        let probe = RouteTestProber(clock: clock, delays: [relay.origin: .milliseconds(40)])
        let selector = ConnectionRouteSelector(clock: clock, probe: probe.run)
        let checks = ConnectionRouteChecks(), request = routeRequest(excluding: home.origin)
        let first = Task {
            await checks.run(request: { request }) { request in
                (try? await selector.select(request.routes, localNetworkAvailable: true, excluding: request.excluding)) != nil
            }
        }
        try await waitUntil { clock.sleepers == 2 }
        var requested = false
        let second = Task {
            await checks.run(request: { requested = true; return request }) { _ in
                XCTFail("Equivalent callers must reuse the authenticated probe")
                return false
            }
        }
        try await waitUntil { requested }
        clock.advance(by: .milliseconds(40))
        let results = await (first.value, second.value)
        XCTAssertTrue(results.0); XCTAssertTrue(results.1)
        let calls = await probe.calls
        XCTAssertEqual(calls, [relay.origin])
        XCTAssertEqual(clock.sleepers, 0)
    }

    func testWaitingCheckCapturesTheLatestRouteAfterAnotherCheckMoves() async throws {
        let clock = RouteTestClock()
        let probe = RouteTestProber(clock: clock, delays: [home.origin: .milliseconds(10), relay.origin: .milliseconds(40)])
        let selector = ConnectionRouteSelector(clock: clock, probe: probe.run)
        let checks = ConnectionRouteChecks()
        var current = routeRequest()
        var captured: [String] = []
        let first = Task {
            await checks.run(request: { current }) { request in
                guard let result = try? await selector.select(request.routes, localNetworkAvailable: true) else { return false }
                current = self.routeRequest(origin: result.route.origin)
                return true
            }
        }
        try await waitUntil { clock.sleepers == 4 }
        var requested = false
        let second = Task {
            await checks.run(request: { requested = true; return current }) { request in
                captured.append(request.origin)
                return (try? await selector.select(request.routes, localNetworkAvailable: true)) != nil
            }
        }
        try await waitUntil { requested }
        clock.advance(by: .milliseconds(10))
        try await waitUntil { captured == [self.home.origin] && clock.sleepers == 4 }
        clock.advance(by: .milliseconds(10))
        let results = await (first.value, second.value)
        XCTAssertTrue(results.0); XCTAssertTrue(results.1)
        XCTAssertEqual(captured, [home.origin], "A completed check for the old origin cannot answer the new origin")
        XCTAssertEqual(clock.sleepers, 0)
    }

    func testStoppingRouteChecksCancelsActiveAndWaitingCallers() async throws {
        let clock = RouteTestClock()
        let probe = RouteTestProber(clock: clock, delays: [home.origin: .seconds(100), relay.origin: .seconds(100)])
        let selector = ConnectionRouteSelector(clock: clock, probe: probe.run)
        let checks = ConnectionRouteChecks()
        let firstRequest = routeRequest(), secondRequest = routeRequest(excluding: home.origin)
        let first = Task {
            await checks.run(request: { firstRequest }) { request in
                (try? await selector.select(request.routes, localNetworkAvailable: true)) != nil
            }
        }
        try await waitUntil { clock.sleepers == 4 }
        var requested = false, staleCheckRan = false
        let second = Task {
            await checks.run(request: { requested = true; return secondRequest }) { _ in
                staleCheckRan = true
                return true
            }
        }
        try await waitUntil { requested }
        checks.cancel()
        let results = await (first.value, second.value)
        XCTAssertFalse(results.0); XCTAssertFalse(results.1)
        XCTAssertFalse(staleCheckRan)
        XCTAssertEqual(clock.sleepers, 0)
        let fresh = await checks.run(request: { firstRequest }) { _ in true }
        XCTAssertTrue(fresh, "A new foreground lifetime can check again")
    }

    /// On the same Wi-Fi, Tailscale often answers first; the home address
    /// still wins when it answers within its deadline.
    func testHomeThatAnswersWinsOverAFasterTailscaleOrRelay() async throws {
        let clock = RouteTestClock()
        let probe = RouteTestProber(clock: clock, delays: [home.origin: .seconds(1), direct.origin: .milliseconds(20),
                                                           relay.origin: .milliseconds(30)])
        let selector = ConnectionRouteSelector(clock: clock, probe: probe.run)
        let selection = Task { try await selector.select([direct, relay, home], localNetworkAvailable: true) }
        try await waitUntil { await probe.calls.count == 3 && clock.sleepers == 6 }
        clock.advance(by: .milliseconds(30))
        try await waitUntil { clock.sleepers == 2 }
        clock.advance(by: .milliseconds(970))
        let winner = try await selection.value
        XCTAssertEqual(winner?.route, home)
        XCTAssertEqual(winner?.elapsed, .seconds(1))
        XCTAssertEqual(clock.sleepers, 0, "Losing work and deadline tasks must be cancelled")
    }

    /// Every combination of network, home, Tailscale and relay outcome: the
    /// relay is chosen, and even probed, only when the home network and
    /// Tailscale both fail.
    func testTheRelayIsTriedOnlyWhenHomeAndTailscaleFail() async throws {
        for local in [true, false] {
            for homeWorks in [true, false] {
                for tailscaleWorks in [true, false] {
                    for relayWorks in [true, false] {
                        let clock = RouteTestClock()
                        let failures = Set([homeWorks ? nil : home.origin, tailscaleWorks ? nil : direct.origin,
                                            relayWorks ? nil : cupolaRelay.origin].compactMap { $0 })
                        let probe = RouteTestProber(clock: clock, delays: [home.origin: .milliseconds(30), direct.origin: .milliseconds(20),
                            cupolaRelay.origin: .milliseconds(10)], failures: failures)
                        let selector = ConnectionRouteSelector(clock: clock, probe: probe.run)
                        let selection = Task { try await selector.select([home, direct, cupolaRelay], localNetworkAvailable: local) }
                        let done = RouteSelectionDone()
                        Task { _ = try? await selection.value; done.finish() }
                        for _ in 0..<2_000 where !done.finished {
                            await Task.yield()
                            clock.advance(by: .milliseconds(5))
                        }
                        let winner = try await selection.value?.route
                        let expected: ConnectionRoute? = (local && homeWorks) ? home : tailscaleWorks ? direct
                            : (!local && homeWorks) ? home : relayWorks ? cupolaRelay : nil
                        let label = "local \(local) home \(homeWorks) tailscale \(tailscaleWorks) relay \(relayWorks)"
                        XCTAssertEqual(winner, expected, label)
                        let relayCalls = await probe.callCount(cupolaRelay.origin)
                        XCTAssertEqual(relayCalls, expected == cupolaRelay || expected == nil ? 1 : 0, label)
                        let calls = await probe.calls
                        if relayCalls == 1 { XCTAssertEqual(calls.last, cupolaRelay.origin, label) }
                    }
                }
            }
        }
    }

    /// The relay serves only its sealed channel: without the pairing pin, or
    /// without the device key to sign in, it is never contacted.
    func testTheRelayNeedsThePinAndTheDeviceKeyBeforeAnyRequest() async throws {
        _ = RouteProofURLProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let api = PairingAPI(configuration: configuration)
        for (pin, signs) in [(nil, true), ("pin", false)] as [(String?, Bool)] {
            let sign: SealedRPC.Sign? = signs ? { @Sendable _ in "signature" } : nil
            do {
                try await ConnectionRouteProber.check(cupolaRelay, saved: savedForRouteProbe(pin: pin), api: api, sign: sign)
                XCTFail("The relay was probed without the pin or a signer")
            } catch PairingFailure.wrongHost { }
        }
        XCTAssertTrue(RouteProofURLProtocol.requests.isEmpty)
        // Nor does an ordinary request to a relay address leave the phone.
        struct Status: Decodable { let hostName: String }
        do {
            let _: Status = try await api.request("/api/v1/host/status", origin: cupolaRelay.origin,
                                                  credential: savedForRouteProbe(pin: "pin").credential)
            XCTFail("A relay request was sent in plaintext")
        } catch { XCTAssertEqual((error as? URLError)?.code, .networkConnectionLost) }
        XCTAssertTrue(RouteProofURLProtocol.requests.isEmpty)
    }

    /// A computer paired with a code has no saved key: turning on Reach from
    /// anywhere stops before asking the computer for proof, and says to pair
    /// again with the QR code or link rather than blaming the computer's key.
    func testACodePairedComputerIsAskedToPairAgainBeforeTheRelayIsTurnedOn() async throws {
        _ = RouteProofURLProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let model = ConnectionModel(saved: try savedForRouteProbe(pin: nil), persistConnection: { _ in },
                                    api: PairingAPI(configuration: configuration))
        defer { model.cancel() }
        do {
            _ = try await model.enableRelay(account: WonderAccount.shared)
            XCTFail("A code-paired computer started turning on the relay")
        } catch RelayEnableFailure.pairedWithCode {
            let shown = model.relayFailure(RelayEnableFailure.pairedWithCode)
            XCTAssertEqual(shown, RemoteAccessPresentation.pairedWithCode(model.macName))
            XCTAssertFalse(shown.contains("couldn’t prove"), shown)
        }
        XCTAssertFalse(RouteProofURLProtocol.requests.contains { $0.url?.path == "/api/v1/host/proof" })
    }

    #if os(iOS)
    /// The phone's own connection check with only the relay left: it opens the
    /// sealed connection, renews its session and lists its devices inside it,
    /// against a stand-in computer that serves exactly the paths the host
    /// serves there (`sealed-http-paths.json`, which wonderd checks against its
    /// own rule) and renews inside the channel as `wonderd`'s `open_sealed`
    /// does. B-038: a session that ended while only the relay is reachable
    /// signs in again with the device key, with or without a known expiry;
    /// another key or a revoked device ends access; a computer from before
    /// that renewal renews a current session as before and, for an ended one,
    /// says to connect at home or over Tailscale rather than to pair again.
    func testTheConnectionCheckConnectsThroughTheRelay() async throws {
        struct Table: Decodable { let served: [String]; let refused: [String] }
        let file = try DiagnosticTestResources.url("tests/contracts/native/sealed-http-paths.json")
        let table = try JSONDecoder().decode(Table.self, from: Data(contentsOf: file))
        let hostRule: @Sendable (String) -> Bool = { Set(table.served).contains($0) }
        let refusingRenewal: @Sendable (String) -> Bool = { hostRule($0) && !$0.hasPrefix("/api/v1/pairing") }
        enum Outcome: Equatable { case connected, accessEnded, connectAtHome, unreachable }
        struct Case {
            let label: String
            var inChannel = true
            let serves: @Sendable (String) -> Bool
            var ended = false
            var expiryKnown = true
            var otherKey = false
            var revoked = false
            let outcome: Outcome
        }
        let cases = [
            Case(label: "current computer, ended session", serves: hostRule, ended: true, outcome: .connected),
            Case(label: "current computer, ended session of unknown expiry", serves: hostRule, ended: true, expiryKnown: false, outcome: .connected),
            Case(label: "current computer, current session", serves: hostRule, outcome: .connected),
            Case(label: "current computer, another key", serves: hostRule, ended: true, otherKey: true, outcome: .accessEnded),
            Case(label: "current computer, revoked device", serves: hostRule, ended: true, revoked: true, outcome: .accessEnded),
            Case(label: "older computer, current session", inChannel: false, serves: hostRule, outcome: .connected),
            Case(label: "older computer, ended session", inChannel: false, serves: hostRule, ended: true, outcome: .connectAtHome),
            Case(label: "older computer refusing renewal", inChannel: false, serves: refusingRenewal, outcome: .unreachable),
        ]
        for test in cases {
            let label = test.label
            let key = P256.Signing.PrivateKey()
            let computer = RelayStandIn(origin: cupolaRelay.origin, serves: test.serves, refused: Set(table.refused),
                                        deviceKey: test.otherKey ? P256.Signing.PrivateKey().publicKey : key.publicKey)
            computer.renewsInChannel = test.inChannel
            computer.revoked = test.revoked
            if test.ended { computer.endedSessions = ["first"] }
            let signer = SigningIdentity(read: { key.rawRepresentation }, save: { _ in }, restore: { _ in
                EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation, sign: { try key.signature(for: $0) })
            }, create: { throw SigningIdentityFailure.missing })
            let now = UInt64(Date().timeIntervalSince1970 * 1000)
            // An ended session is an hour old; a current one has most of its hour left.
            let expiry = test.expiryKnown ? #","expiresAtMs":\#(test.ended ? now - 60_000 : now + 3_000_000)"# : ""
            let credential = try JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"first","deviceId":"phone","csrfToken":"csrf","hostInstallationId":"relay-check-studio"\#(expiry)}"#.utf8))
            let saved = try SavedConnection(routes: [cupolaRelay], credential: credential, hostKeyPin: computer.pin)
            let store = ReadStore(root: FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Wonder/Hosts"),
                                  host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
            defer { try? store.remove() }
            let model = ConnectionModel(saved: saved, persistConnection: { _ in },
                                        api: PairingAPI(configuration: .ephemeral, sealedSocket: { _ in computer.connect() }), signingIdentity: signer)
            // An ended session renews on the check a launch runs; a current one when asked.
            await model.check(renew: !test.ended)
            XCTAssertTrue(computer.requested.allSatisfy { !table.refused.contains($0) }, "asked the relay for a refused path: \(computer.requested)")
            // Renewal inside the channel never also asks over a signed-in connection.
            XCTAssertEqual(computer.requested.contains("/api/v1/pairing/session/refresh-challenge"), !test.inChannel && !test.ended, label)
            switch test.outcome {
            case .connected:
                XCTAssertEqual(model.status, "Connected through Cupola’s relay.", label)
                XCTAssertEqual(model.macConnected, true, label)
                XCTAssertFalse(model.accessEnded, label)
                XCTAssertEqual(model.connection?.credential.sessionToken, "renewed", label)
                XCTAssertEqual(Array(computer.requested.suffix(2)), ["/api/v1/devices", "/api/v1/host/status"], label)
                // The calls after renewal ride a connection signed in with the new session.
                XCTAssertEqual(computer.sessions, test.ended ? ["renewed"] : ["first", "renewed"], label)
                XCTAssertEqual(computer.renewals, test.inChannel ? 1 : 0, label)
            case .accessEnded:
                XCTAssertTrue(model.accessEnded, label)
                XCTAssertNotEqual(model.macConnected, true, label)
                XCTAssertEqual(model.connection?.credential.sessionToken, "first", label)
                XCTAssertEqual(computer.renewals, 0, label)
            case .connectAtHome:
                XCTAssertFalse(model.accessEnded, label)
                XCTAssertNotEqual(model.macConnected, true, label)
                XCTAssertEqual(model.status, RelayUnsupported.signIn.errorDescription, label)
                XCTAssertFalse(model.status.localizedCaseInsensitiveContains("pair"), label)
            case .unreachable:
                XCTAssertTrue(computer.requested.contains("/api/v1/pairing/session/refresh-challenge"), label)
                XCTAssertNotEqual(model.status, "Connected through Cupola’s relay.", label)
                XCTAssertNotEqual(model.macConnected, true, label)
                XCTAssertFalse(model.accessEnded, label)
            }
            await model.api.closeSealed()
        }
    }
    #endif

    func testCancellationDoesNotSelectAStaleRoute() async throws {
        let clock = RouteTestClock()
        let probe = RouteTestProber(clock: clock, delays: [home.origin: .seconds(100)])
        let selector = ConnectionRouteSelector(clock: clock, probe: probe.run)
        let selection = Task { try await selector.select([home], localNetworkAvailable: true) }
        try await waitUntil { clock.sleepers == 2 }
        selection.cancel()
        do { _ = try await selection.value; XCTFail("Cancelled selection must not commit") }
        catch is CancellationError { }
        XCTAssertEqual(clock.sleepers, 0)
    }

    private func milliseconds(_ duration: Duration) -> Int { Int((duration / .milliseconds(1)).rounded()) }

    /// One owner waits out each miss; the person trying again ends the wait at once.
    func testRecoveryWaitsOutAMissAndWakesWhenAskedToTryAgain() async throws {
        let clock = RouteTestClock()
        let recovery = ConnectionRecovery(clock: clock, jitter: { 1 })
        XCTAssertFalse(recovery.isWaiting)
        recovery.failed()
        XCTAssertTrue(recovery.isWaiting)
        var finished = false
        let first = Task {
            let due = await recovery.waitUntilDue()
            finished = true
            return due
        }
        try await waitUntil { clock.sleepers == 1 }
        clock.advance(by: .milliseconds(2_999))
        for _ in 0..<1_000 { await Task.yield() }
        XCTAssertFalse(finished, "The first retry waits 3 s")
        clock.advance(by: .milliseconds(1))
        let firstDue = await first.value
        XCTAssertTrue(firstDue)
        XCTAssertFalse(recovery.isWaiting)
        recovery.failed()
        XCTAssertEqual(recovery.failures, 2)
        let second = Task { await recovery.waitUntilDue() }
        try await waitUntil { clock.sleepers == 1 }
        recovery.wake()
        let secondDue = await second.value
        XCTAssertTrue(secondDue)
        XCTAssertEqual(clock.sleepers, 0)
        recovery.failed()
        let abandoned = Task { await recovery.waitUntilDue() }
        try await waitUntil { clock.sleepers == 1 }
        abandoned.cancel()
        let abandonedDue = await abandoned.value
        XCTAssertFalse(abandonedDue, "A cancelled owner does not attempt")
        recovery.succeeded()
        XCTAssertFalse(recovery.isWaiting)
        XCTAssertEqual(clock.sleepers, 0)
    }

    /// A quiet live stream is checked after 25 s; an unanswered check closes
    /// it after 10 s so recovery takes over instead of showing Connected.
    func testAQuietStreamWhoseComputerStopsAnsweringIsClosed() async throws {
        let clock = RouteTestClock()
        let stream = QuietLiveStream()
        let receiving = Task {
            try await ConnectionLiveness.receive(clock: clock, receive: { try await stream.receive() }, confirmAlive: {
                stream.noteCheck()
                try await ConnectionRoutePong.wait { _ in } // The computer is asleep.
            }, close: { stream.close() })
        }
        try await waitUntil { clock.sleepers == 1 && stream.isReceiving }
        clock.advance(by: .seconds(24))
        for _ in 0..<1_000 { await Task.yield() }
        XCTAssertEqual(stream.checks, 0)
        clock.advance(by: .seconds(1))
        try await waitUntil { stream.checks == 1 && clock.sleepers == 1 }
        clock.advance(by: .seconds(9))
        for _ in 0..<1_000 { await Task.yield() }
        XCTAssertFalse(stream.isClosed)
        clock.advance(by: .seconds(1))
        do { _ = try await receiving.value; XCTFail("A stream nobody answers stayed open") }
        catch let error as URLError { XCTAssertEqual(error.code, .timedOut) }
        XCTAssertTrue(stream.isClosed)
        XCTAssertEqual(ConnectionFailure.classify(URLError(.timedOut), cancelled: false), .route)
        XCTAssertEqual(clock.sleepers, 0)
    }

    #if os(iOS)
    /// On the relay, a missed liveness check ends the sealed connection itself:
    /// unsubscribing alone left it open and cached, so recovery reused it
    /// without reaching the computer.
    func testARelayLivenessMissEndsTheSealedConnection() async throws {
        let clock = RouteTestClock()
        let computer = RelayStandIn(origin: cupolaRelay.origin, serves: { _ in true }, refused: [])
        let api = PairingAPI(configuration: .ephemeral, sealedSocket: { _ in computer.connect() })
        let credential = try JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"first","deviceId":"phone","csrfToken":"csrf","hostInstallationId":"relay-check-studio"}"#.utf8))
        let sign: SealedRPC.Sign = { _ in "signature" }
        let client = try await api.sealedConnection(origin: cupolaRelay.origin, credential: credential, pin: computer.pin, sign: sign)
        let (events, subscription) = AsyncThrowingStream.makeStream(of: Data.self, throwing: Error.self)
        defer { subscription.finish() }
        let stream = SealedEventStream(client: client, subscription: "s", events: events)
        let receiving = Task {
            try await ConnectionLiveness.receive(clock: clock, receive: { try await stream.receiveFrame() },
                confirmAlive: { try await ConnectionRoutePong.wait { _ in } }, // The computer is asleep.
                close: { await stream.abandon() })
        }
        try await waitUntil { clock.sleepers == 1 }
        clock.advance(by: ConnectionLiveness.quiet)
        try await waitUntil { clock.sleepers == 1 }
        clock.advance(by: ConnectionLiveness.deadline)
        do { _ = try await receiving.value; XCTFail("A relay stream nobody answers stayed open") }
        catch let error as URLError { XCTAssertEqual(error.code, .timedOut) }
        let open = await client.isOpen
        XCTAssertFalse(open, "The dead relay connection must not stay open for reuse")
        let next = try await api.sealedConnection(origin: cupolaRelay.origin, credential: credential, pin: computer.pin, sign: sign)
        XCTAssertFalse(next === client)
        XCTAssertEqual(computer.sessions, ["first", "first"], "Recovery signs in with the computer again")
        await api.closeSealed()
    }

    #endif

    func testHandoffKeepsOldStreamUntilFirstFrameAndPreservesContiguousCursor() async throws {
        var projection = ProjectionState()
        projection.hostEpoch = "epoch"; projection.lastSequence = 10
        var subscriptionCursor: UInt64 = 0, delivered: [UInt64] = [], closed: [String] = [], active = "old"
        func consume(_ event: ReplayEvent) throws {
            if case .skipped = try projection.consumeLive(event) { return }
            delivered.append(event.sequence)
        }
        func event(_ sequence: UInt64) -> ReplayEvent {
            ReplayEvent(eventId: "e\(sequence)", hostEpoch: "epoch", sequence: sequence,
                        occurredAt: "1", conversationId: "chat", event: EventBody(type: "conversation_updated"))
        }
        try await ConnectionRouteHandoff.move(clock: ContinuousClock(), open: {
            subscriptionCursor = projection.lastSequence
            // The old stream advances after subscribing the candidate.
            try consume(event(11))
            return "new"
        }, firstFrame: { _ in
            XCTAssertEqual(active, "old")
            XCTAssertTrue(closed.isEmpty)
            return try JSONEncoder().encode([event(11), event(12)])
        }, commit: { socket, data in
            for event in try ReplayEvent.frame(XCTUnwrap(data)).events { try consume(event) }
            active = socket; closed.append("old")
        }, close: { closed.append($0) })
        XCTAssertEqual(subscriptionCursor, 10)
        XCTAssertEqual(projection.hostEpoch, "epoch")
        XCTAssertEqual(projection.lastSequence, 12)
        XCTAssertEqual(delivered, [11, 12], "Overlapping old/new frames apply once with no missing sequence")
        XCTAssertEqual(active, "new")
        XCTAssertEqual(closed, ["old"])
    }

    func testFailedFirstFrameLeavesOldSocketAndCursorUntouched() async throws {
        for frame in [Data("bad frame".utf8), Data()] {
            var committed = false, closed: [String] = []
            do {
                try await ConnectionRouteHandoff.move(clock: ContinuousClock(), open: { "candidate" }, firstFrame: { _ in frame },
                    commit: { _, _ in committed = true }, close: { closed.append($0) })
                XCTFail("Invalid first frame must not replace the stream")
            } catch { }
            XCTAssertFalse(committed)
            XCTAssertEqual(closed, ["candidate"])
        }
    }

    func testCancelledReplacementCannotCommitAfterBackgroundOrPairingChange() async throws {
        let clock = RouteTestClock()
        var committed = false, closed: [String] = []
        let move = Task {
            try await ConnectionRouteHandoff.move(clock: clock, open: { "candidate" }, firstFrame: { _ in
                try await ConnectionRoutePong.wait { _ in } // Foundation never calls back.
                return nil
            }, commit: { _, _ in committed = true }, close: { closed.append($0) })
        }
        try await waitUntil { clock.sleepers == 1 }
        move.cancel()
        do { try await move.value; XCTFail("An ownerless replacement must not commit") }
        catch is CancellationError { }
        XCTAssertFalse(committed)
        XCTAssertEqual(closed, ["candidate"])
        XCTAssertEqual(clock.sleepers, 0)
    }

    private func savedForRouteProbe(pin: String?) throws -> SavedConnection {
        let credential = try JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"test-session","deviceId":"phone","csrfToken":"test-csrf","hostInstallationId":"studio"}"#.utf8))
        return try SavedConnection(routes: [home], credential: credential, hostKeyPin: pin)
    }

    func testPinnedProbeUsesFreshProofAndNeverSendsCredentialsBeforeVerification() async throws {
        let pin = RouteProofURLProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let api = PairingAPI(configuration: configuration)
        let saved = try savedForRouteProbe(pin: pin)
        for _ in 0..<2 { try await ConnectionRouteProber.check(home, saved: saved, api: api) }
        let requests = RouteProofURLProtocol.requests
        XCTAssertEqual(requests.count, 4)
        XCTAssertEqual(requests.map { $0.url!.path }, ["/readyz", "/api/v1/host/proof", "/readyz", "/api/v1/host/proof"])
        XCTAssertTrue(requests.allSatisfy { $0.value(forHTTPHeaderField: "Cookie") == nil && $0.value(forHTTPHeaderField: "x-wonder-csrf") == nil })
        XCTAssertTrue(requests.allSatisfy { $0.value(forHTTPHeaderField: "Origin") == home.origin })
        XCTAssertNotEqual(requests[1].url!.query, requests[3].url!.query)
    }

    func testWrongPinnedHostCannotWinAProbe() async throws {
        _ = RouteProofURLProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let api = PairingAPI(configuration: configuration)
        let saved = try savedForRouteProbe(pin: "different-key-pin")
        do { try await ConnectionRouteProber.check(home, saved: saved, api: api); XCTFail("A different host must never be selected") }
        catch PairingFailure.wrongHost { }
        XCTAssertEqual(RouteProofURLProtocol.requests.count, 2)
        XCTAssertTrue(RouteProofURLProtocol.requests.allSatisfy { $0.value(forHTTPHeaderField: "Cookie") == nil })
    }

    func testUnpinnedHomeCannotBeProbedOrReceiveCredentials() async throws {
        _ = RouteProofURLProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let api = PairingAPI(configuration: configuration)
        do { try await ConnectionRouteProber.check(home, saved: savedForRouteProbe(pin: nil), api: api); XCTFail("A home address needs its pairing pin") }
        catch PairingFailure.wrongHost { }
        XCTAssertTrue(RouteProofURLProtocol.requests.isEmpty)
    }

    func testHostProofRejectsWrongPinOriginNonceAndSignature() throws {
        let key = P256.Signing.PrivateKey()
        let origin = home.origin, nonce = "nonce_012345678901234567890123456789"
        let signature = try key.signature(for: Data("wonder-host-proof-v1\n\(origin)\n\(nonce)".utf8))
        let data = try JSONSerialization.data(withJSONObject: ["hostKey": key.publicKey.derRepresentation.base64URL, "signature": signature.rawRepresentation.base64URL])
        let proof = try JSONDecoder().decode(ConnectionRouteProof.self, from: data)
        let pin = Data(SHA256.hash(data: key.publicKey.derRepresentation)).base64URL
        XCTAssertNoThrow(try proof.validate(pin: pin, origin: origin, nonce: nonce))
        XCTAssertThrowsError(try proof.validate(pin: "other", origin: origin, nonce: nonce))
        XCTAssertThrowsError(try proof.validate(pin: pin, origin: relay.origin, nonce: nonce))
        XCTAssertThrowsError(try proof.validate(pin: pin, origin: origin, nonce: nonce + "x"))
        let swapped = P256.Signing.PrivateKey()
        let bad = try JSONSerialization.data(withJSONObject: ["hostKey": key.publicKey.derRepresentation.base64URL,
            "signature": swapped.signature(for: Data("wonder-host-proof-v1\n\(origin)\n\(nonce)".utf8)).rawRepresentation.base64URL])
        let badProof = try JSONDecoder().decode(ConnectionRouteProof.self, from: bad)
        XCTAssertThrowsError(try badProof.validate(pin: pin, origin: origin, nonce: nonce))
    }
}

private final class RouteProofURLProtocol: URLProtocol, @unchecked Sendable {
    private final class State: @unchecked Sendable {
        let lock = NSLock()
        var key = P256.Signing.PrivateKey()
        var requests: [URLRequest] = []
        var enrollment = false
        var challenge: Challenge?
    }
    private static let state = State()
    static func reset(enrollment: Bool = false) -> String {
        state.lock.withLock {
            state.key = P256.Signing.PrivateKey(); state.requests = []
            state.enrollment = enrollment; state.challenge = nil
            return Data(SHA256.hash(data: state.key.publicKey.derRepresentation)).base64URL
        }
    }
    static var requests: [URLRequest] { state.lock.withLock { state.requests } }
    static var enrollmentChallenge: Challenge? { state.lock.withLock { state.challenge } }
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}
    override func startLoading() {
        do {
            let data: Data = try Self.state.lock.withLock {
                var recorded = request
                if let stream = request.httpBodyStream, request.httpBody == nil {
                    stream.open(); defer { stream.close() }
                    var data = Data(), buffer = [UInt8](repeating: 0, count: 4096)
                    while stream.hasBytesAvailable {
                        let count = stream.read(&buffer, maxLength: buffer.count)
                        guard count > 0 else { break }
                        data.append(contentsOf: buffer.prefix(count))
                    }
                    recorded.httpBody = data
                }
                Self.state.requests.append(recorded)
                if Self.state.enrollment && request.url?.path == "/api/v1/pairing/claim" {
                    let now = UInt64(Date().timeIntervalSince1970 * 1000)
                    let fields: [String: Any] = ["challengeId": "challenge", "deviceId": "fixture-device", "nonce": "nonce",
                        "origin": request.value(forHTTPHeaderField: "Origin")!, "hostInstallationId": "host",
                        "offerId": "00000000-0000-0000-0000-000000000001", "issuedAtMs": now, "expiresAtMs": now + 60_000]
                    Self.state.challenge = try JSONDecoder().decode(Challenge.self, from: JSONSerialization.data(withJSONObject: fields))
                    return try JSONSerialization.data(withJSONObject: ["deviceId": "fixture-device", "challenge": fields])
                }
                if Self.state.enrollment && request.url?.path == "/api/v1/pairing/session" {
                    return Data(#"{"sessionToken":"fixture-token","deviceId":"fixture-device","csrfToken":"fixture-csrf","hostInstallationId":"host"}"#.utf8)
                }
                guard request.url?.path == "/api/v1/host/proof" else { return Data("{}".utf8) }
                let nonce = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!.queryItems!.first!.value!
                let origin = request.value(forHTTPHeaderField: "Origin")!
                let transcript = Data("wonder-host-proof-v1\n\(origin)\n\(nonce)".utf8)
                return try JSONSerialization.data(withJSONObject: ["hostKey": Self.state.key.publicKey.derRepresentation.base64URL,
                    "signature": Self.state.key.signature(for: transcript).rawRepresentation.base64URL])
            }
            let response = HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: ["Content-Type": "application/json"])!
            client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: data)
            client?.urlProtocolDidFinishLoading(self)
        } catch { client?.urlProtocol(self, didFailWithError: error) }
    }
}

#if os(iOS)
/// A computer behind the relay: the host half of the sealed handshake, then
/// `http.request` for the paths `serves` allows. Refused paths fail as the
/// host fails them; other paths answer 404 as a missing route does.
private final class RelayStandIn: @unchecked Sendable {
    let origin: String
    let hostKey = P256.Signing.PrivateKey()
    let serves: @Sendable (String) -> Bool
    let refused: Set<String>
    /// The key the phone paired with; nil accepts any renewal signature.
    let deviceKey: P256.Signing.PublicKey?
    /// JSON for other served paths (the share sheet's catalog).
    var bodies: [String: Any] = [:]
    /// Sessions this computer refuses at the opening (they ended).
    var endedSessions: Set<String> = []
    /// False: a computer from before renewal inside the channel.
    var renewsInChannel = true
    var revoked = false
    private let lock = NSLock()
    private var paths: [String] = []
    private var signedIn: [String] = []
    private var renewed = 0
    init(origin: String, serves: @escaping @Sendable (String) -> Bool, refused: Set<String>, deviceKey: P256.Signing.PublicKey? = nil) {
        self.origin = origin; self.serves = serves; self.refused = refused; self.deviceKey = deviceKey
    }
    /// Sessions issued by renewal inside the channel.
    var renewals: Int { lock.withLock { renewed } }
    fileprivate func noteRenewal() { lock.withLock { renewed += 1 } }

    /// Whether `signature` is the paired key's over `challenge`, as the host checks it.
    func signed(_ challenge: Challenge, _ signature: String) -> Bool {
        guard !revoked else { return false }
        guard let deviceKey else { return true }
        let padded = signature.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        guard let raw = Data(base64Encoded: padded + String(repeating: "=", count: (4 - padded.count % 4) % 4)),
              let parsed = try? P256.Signing.ECDSASignature(rawRepresentation: raw) else { return false }
        return deviceKey.isValidSignature(parsed, for: challenge.transcript)
    }
    var pin: String { Data(SHA256.hash(data: hostKey.publicKey.derRepresentation)).base64EncodedString()
        .replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "") }
    var requested: [String] { lock.withLock { paths } }
    var sessions: [String] { lock.withLock { signedIn } }
    func connect() -> any SealedSocket { Connection(computer: self) }

    fileprivate func note(path: String) { lock.withLock { paths.append(path) } }
    fileprivate func note(session: String) { lock.withLock { signedIn.append(session) } }

    func challenge(_ issued: Challenge? = nil) -> [String: Any] {
        let now = UInt64(Date().timeIntervalSince1970 * 1000)
        return ["challengeId": issued?.challengeId ?? UUID().uuidString, "deviceId": "phone", "nonce": issued?.nonce ?? "n", "origin": origin,
                "hostInstallationId": "relay-check-studio", "offerId": "", "issuedAtMs": issued?.issuedAtMs ?? now,
                "expiresAtMs": issued?.expiresAtMs ?? now + 60_000]
    }

    /// `http.request`: the route's status and body, or the host's refusal.
    func answer(_ params: [String: Any]) -> (result: [String: Any]?, refusal: String?) {
        let path = (params["path"] as? String ?? "").components(separatedBy: "?")[0]
        note(path: path)
        if refused.contains(path) || (path.hasPrefix("/api/v1/pairing") && !serves(path)) {
            return (nil, "This isn't available through the relay.")
        }
        func json(_ value: Any) -> [String: Any] {
            ["status": 200, "contentType": "application/json",
             "body": ((try? JSONSerialization.data(withJSONObject: value)) ?? Data()).base64EncodedString()]
        }
        guard serves(path) else { return (["status": 404, "body": ""], nil) }
        switch path {
        case "/api/v1/pairing/session/refresh-challenge": return (json(challenge()), nil)
        case "/api/v1/pairing/session":
            return (json(["sessionToken": "renewed", "deviceId": "phone", "csrfToken": "csrf-2", "hostInstallationId": "relay-check-studio"]), nil)
        case "/api/v1/devices": return (json([["id": "phone"]]), nil)
        case "/api/v1/host/status": return (json(["hostInstallationId": "relay-check-studio", "hostName": "Studio"]), nil)
        default:
            if let body = bodies[path] { return (json(body), nil) }
            return (["status": 404, "body": ""], nil)
        }
    }

    final class Connection: SealedSocket, @unchecked Sendable {
        let computer: RelayStandIn
        private let lock = NSLock()
        private var channel: SealedChannel?
        private var opened = false
        /// The renewal challenge issued on this connection, until answered.
        private var issued: Challenge?
        private let inbox: AsyncStream<SealedSocketMessage>
        private let deliver: AsyncStream<SealedSocketMessage>.Continuation
        private var iterator: AsyncStream<SealedSocketMessage>.Iterator
        init(computer: RelayStandIn) {
            self.computer = computer
            (inbox, deliver) = AsyncStream.makeStream()
            iterator = inbox.makeAsyncIterator()
        }
        func send(text: String) async throws {
            let (welcome, channel) = try SealedResponder.answer(hello: text, hostKey: computer.hostKey, origin: computer.origin)
            lock.withLock { self.channel = channel }
            deliver.yield(.text(welcome))
        }
        func send(data: Data) async throws {
            let (message, first, answering): ([String: Any], Bool, Challenge?) = try lock.withLock {
                guard var open = channel else { throw SealedChannelFailure.frame }
                let plaintext = try open.open(data)
                channel = open
                defer { opened = true; issued = nil }
                return (try JSONSerialization.jsonObject(with: plaintext) as? [String: Any] ?? [:], !opened, issued)
            }
            // The opening and a renewal's answer, as `wonderd`'s `open_sealed`
            // reads them; every refusal is one `id: null` error, then close.
            func refuse() {
                push(["jsonrpc": "2.0", "id": NSNull(), "error": ["code": -32001, "message": "This device is not signed in to this computer. Pair it again."]])
                close()
            }
            if let answering {
                guard message["challengeId"] as? String == answering.challengeId, let signature = message["signature"] as? String,
                      computer.signed(answering, signature) else { return refuse() }
                computer.noteRenewal()
                computer.note(session: "renewed")
                push(["renewed": ["sessionToken": "renewed", "deviceId": "phone", "csrfToken": "csrf-2", "hostInstallationId": "relay-check-studio"]])
                return
            }
            if first, let session = message["session"] as? String {
                if computer.endedSessions.contains(session) { return refuse() }
                computer.note(session: session); return
            }
            if first, let renew = message["renew"] as? [String: Any] {
                // A computer from before renewal reads no such opening and closes.
                guard computer.renewsInChannel else { return close() }
                guard renew["deviceId"] as? String == "phone", !computer.revoked,
                      let challenge = try? JSONDecoder().decode(Challenge.self, from: JSONSerialization.data(withJSONObject: computer.challenge())) else { return refuse() }
                lock.withLock { issued = challenge }
                push(["challenge": computer.challenge(challenge)])
                return
            }
            guard let id = message["id"] as? Int, let method = message["method"] as? String else { return }
            switch method {
            case "auth.challenge": push(["jsonrpc": "2.0", "id": id, "result": computer.challenge()])
            case "http.request":
                let (result, refusal) = computer.answer(message["params"] as? [String: Any] ?? [:])
                if let refusal { push(["jsonrpc": "2.0", "id": id, "error": ["code": -32004, "message": refusal]]) }
                else { push(["jsonrpc": "2.0", "id": id, "result": result ?? [:]]) }
            default: push(["jsonrpc": "2.0", "id": id, "result": [String: Any]()])
            }
        }
        private func push(_ message: [String: Any]) {
            let sealed: Data? = lock.withLock {
                guard var open = channel, let text = try? JSONSerialization.data(withJSONObject: message),
                      let frame = try? open.seal(text) else { return nil }
                channel = open
                return frame
            }
            if let sealed { deliver.yield(.data(sealed)) }
        }
        func receive() async throws -> SealedSocketMessage {
            var next = lock.withLock { iterator }
            defer { lock.withLock { iterator = next } }
            guard let message = await next.next() else { throw URLError(.networkConnectionLost) }
            return message
        }
        func close() { deliver.finish() }
    }
}
#endif

/// A live stream that stays quiet until a test delivers a frame or it is closed.
private final class QuietLiveStream: @unchecked Sendable {
    private let lock = NSLock()
    private var waiting: CheckedContinuation<Data, Error>?
    private var closed = false
    private var count = 0
    var isReceiving: Bool { lock.withLock { waiting != nil } }
    var isClosed: Bool { lock.withLock { closed } }
    var checks: Int { lock.withLock { count } }
    func noteCheck() { lock.withLock { count += 1 } }
    func receive() async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            let refused = lock.withLock { () -> Bool in
                if closed { return true }
                waiting = continuation
                return false
            }
            if refused { continuation.resume(throwing: URLError(.networkConnectionLost)) }
        }
    }
    func deliver(_ frame: Data) {
        let waiter = lock.withLock { () -> CheckedContinuation<Data, Error>? in
            defer { waiting = nil }
            return waiting
        }
        waiter?.resume(returning: frame)
    }
    func close() {
        let waiter = lock.withLock { () -> CheckedContinuation<Data, Error>? in
            closed = true
            defer { waiting = nil }
            return waiting
        }
        waiter?.resume(throwing: URLError(.networkConnectionLost))
    }
}

private final class RouteSelectionDone: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    var finished: Bool { lock.withLock { value } }
    func finish() { lock.withLock { value = true } }
}

private actor RouteTestProber {
    let clock: RouteTestClock
    var delays: [String: Duration]
    let failures: Set<String>
    var calls: [String] = []
    init(clock: RouteTestClock, delays: [String: Duration], failures: Set<String> = []) {
        self.clock = clock; self.delays = delays; self.failures = failures
    }
    func run(_ route: ConnectionRoute) async throws {
        calls.append(route.origin)
        try await clock.sleep(for: delays[route.origin] ?? .seconds(100))
        if failures.contains(route.origin) { throw URLError(.cannotConnectToHost) }
    }
    func callCount(_ origin: String) -> Int { calls.filter { $0 == origin }.count }
    func set(_ origin: String, delay: Duration) { delays[origin] = delay }
}

private final class RouteTestClock: Clock, @unchecked Sendable {
    typealias Instant = ContinuousClock.Instant
    private let lock = NSLock()
    private var instant = ContinuousClock().now
    private var waiting: [UUID: (Instant, CheckedContinuation<Void, Error>)] = [:]
    private var cancelled: Set<UUID> = []
    var now: Instant { lock.withLock { instant } }
    var minimumResolution: Duration { .nanoseconds(1) }
    var sleepers: Int { lock.withLock { waiting.count } }
    func sleep(until deadline: Instant, tolerance: Duration?) async throws {
        let id = UUID()
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
                lock.withLock {
                    if cancelled.remove(id) != nil || Task.isCancelled { continuation.resume(throwing: CancellationError()) }
                    else if deadline <= instant { continuation.resume() }
                    else { waiting[id] = (deadline, continuation) }
                }
            }
        } onCancel: {
            self.lock.withLock {
                if let item = self.waiting.removeValue(forKey: id) { item.1.resume(throwing: CancellationError()) }
                else { self.cancelled.insert(id) }
            }
        }
    }
    func advance(by amount: Duration) {
        lock.withLock {
            instant = instant.advanced(by: amount)
            for (id, item) in waiting where item.0 <= instant {
                waiting.removeValue(forKey: id); item.1.resume()
            }
        }
    }
}

// MARK: Home network contracts
#if os(iOS)
import Security
import Network

/// A DHCP-reused address must never receive a paired session. OpenSSL supplied
/// the independent SPKI pin and certificates, including a renewal of the same key.
final class HomeNetworkTrustTests: XCTestCase {
    /// Fails every load the way URLSession reports a refused certificate;
    /// `present` stands in for the TLS handshake reaching the trust first.
    /// Without it, the computer is out of reach and the load times out.
    private final class ImpostorTLSURLProtocol: URLProtocol, @unchecked Sendable {
        nonisolated(unsafe) static var present: (() -> Void)?
        override class func canInit(with request: URLRequest) -> Bool { true }
        override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
        override func startLoading() {
            guard let present = Self.present else { client?.urlProtocol(self, didFailWithError: URLError(.timedOut)); return }
            present()
            client?.urlProtocol(self, didFailWithError: URLError(.cancelled))
        }
        override func stopLoading() {}
    }
    private let origin = "https://192.168.1.20:3778"
    private let pin = "i66hy2aNjaThHWj9h7QrmA9spXxJfn2LxtG4lQLDEsE"
    private let paired = "MIIBVTCB/KADAgECAhQg4rlVH9jVlySVjpmM4sU2zdLFTTAKBggqhkjOPQQDAjARMQ8wDQYDVQQDDAZwYWlyZWQwHhcNMjYxMDE5MDAwOTQ1WhcNMjYxMTE4MDAwOTQ1WjARMQ8wDQYDVQQDDAZwYWlyZWQwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASVC5EPcqMR+f2qbdCwkiEJ1NibczYZrUV/QEp2zhaoP1uhHsAP0kP8NH1RFTEhauv8unLBFHFmCKqqoOm9o2RaozIwMDAdBgNVHQ4EFgQUenqRoZVoObmQkfUlmHsFrnQArbwwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNIADBFAiBL2TFPgJv6Jy8kGl8MXFynI65TypIuH6Xh/x6FYBGhbgIhAIwN+GlaBub+YBbXYLofhabw4bVdQKtxAaBK8XLMVCnc"
    private let renewed = "MIIBVjCB/qADAgECAhRB9y8I6MxcQdSQyQPQZXMusNZZUTAKBggqhkjOPQQDAjASMRAwDgYDVQQDDAdyZW5ld2VkMB4XDTI2MTAxOTAwMDk0NVoXDTI2MTExODAwMDk0NVowEjEQMA4GA1UEAwwHcmVuZXdlZDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABJULkQ9yoxH5/apt0LCSIQnU2JtzNhmtRX9ASnbOFqg/W6EewA/SQ/w0fVEVMSFq6/y6csEUcWYIqqqg6b2jZFqjMjAwMB0GA1UdDgQWBBR6epGhlWg5uZCR9SWYewWudACtvDAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIGgv6giCmQljPA5xj9lHBKrSeKmboKgkft7q64mYpseAAiBJwuhSPMlvw0UTUaPzD/o4q4dV6GDnPR9zw47PVnNtYw=="
    private let impostor = "MIIBWzCCAQCgAwIBAgIUMMjIDM5+7Vz9b7ybycwP7Wh+j8YwCgYIKoZIzj0EAwIwEzERMA8GA1UEAwwIaW1wb3N0b3IwHhcNMjYxMDE5MDAwOTQ1WhcNMjYxMTE4MDAwOTQ1WjATMREwDwYDVQQDDAhpbXBvc3RvcjBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABJQDxI/sHwSyS7KmLDcdrbefD1zT82Zwa0qMeUw4PqmdOteKh78elc4M65OINnwvf/F+Ay5ie1Kzv+G3TDnJzGSjMjAwMB0GA1UdDgQWBBQMl6DTc8rcWtwvpGKp6CvaoSU3BzAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0kAMEYCIQDYARHYl5MZ0ddjPidq1GFZXBgPzfZOPY46h9VbbognSAIhAOIV2jMlZ6GNXze8oaMF6HjO06vljO9b1z8yVEe00vwH"
    private func certificate(_ value: String) -> SecCertificate {
        SecCertificateCreateWithData(nil, Data(base64Encoded: value)! as CFData)!
    }
    private func saved() throws -> SavedConnection {
        let credential = try JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"test","csrfToken":"test","deviceId":"test-device","hostInstallationId":"test-host"}"#.utf8))
        return try SavedConnection(routes: [ConnectionRoute(kind: .home, origin: origin)], credential: credential, hostKeyPin: pin)
    }
    func testPinnedKeyAcceptsItsRenewedCertificateAndRefusesAnImpostor() throws {
        let trust = HomeNetworkTrust(); trust.update(try saved())
        XCTAssertEqual(HomeNetworkTrust.digest(certificate(paired)), pin)
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(paired)), .accept)
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(renewed)), .accept)
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(impostor)), .reject)
    }
    // A3/A1: a home certificate with another key is a key change. URLSession
    // fails it before `/host/proof`, so the trust remembers why, for the
    // attempt that saw it only.
    func testAnotherKeyAtTheHomeAddressReadsAsAKeyChange() async throws {
        let trust = HomeNetworkTrust(); trust.update(try saved())
        let attempt = trust.mark()
        XCTAssertFalse(trust.provedAnotherKey(origin, since: attempt))
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(impostor)), .reject)
        XCTAssertTrue(trust.provedAnotherKey(origin, since: attempt))
        XCTAssertFalse(trust.provedAnotherKey("https://192.168.1.21:3778", since: attempt))

        // URLSession reports the refused certificate as a cancelled load.
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ImpostorTLSURLProtocol.self]
        ImpostorTLSURLProtocol.present = { [self] in _ = trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(impostor)) }
        defer { ImpostorTLSURLProtocol.present = nil }
        let fixture = try saved()
        do {
            try await ConnectionRouteProber.check(fixture.routes[0], saved: fixture, api: PairingAPI(configuration: configuration), keyMismatches: trust)
            XCTFail("a rejected certificate must not pass")
        } catch PairingFailure.wrongHost { }
        do {
            try await ConnectionRouteProber.check(fixture.routes[0], saved: fixture, api: PairingAPI(configuration: configuration))
            XCTFail("a rejected certificate must not pass")
        } catch let error as URLError { XCTAssertEqual(error.code, .cancelled, "without the mismatch it stays a network failure") }

        trust.update(try saved())
        _ = trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(paired))
        XCTAssertFalse(trust.provedAnotherKey(origin, since: 0), "the paired key answering again clears it")
        _ = trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(impostor))
        let repaired = try SavedConnection(routes: [ConnectionRoute(kind: .home, origin: origin)],
                                           credential: try saved().credential, hostKeyPin: "a-new-pairing")
        trust.update(repaired)
        XCTAssertFalse(trust.provedAnotherKey(origin, since: 0), "pairing again with a new key forgets the old mismatch")
    }

    func testPinIsBoundToTheExactAddressAndPortAndRequiresALeaf() throws {
        let trust = HomeNetworkTrust(); trust.update(try saved())
        XCTAssertEqual(trust.decision(host: "192.168.1.21", port: 3778, certificate: certificate(paired)), .reject)
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3779, certificate: certificate(paired)), .reject)
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: nil), .reject)
        XCTAssertEqual(trust.decision(host: "studio.tail.ts.net", port: 443, certificate: certificate(paired)), .system)
        XCTAssertEqual(trust.decision(host: "example.com", port: 443, certificate: certificate(impostor)), .system)
    }
    func testUnpairedAndRemovedHomeAddressesAreNeverTrusted() throws {
        let trust = HomeNetworkTrust()
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(paired)), .reject)
        trust.update(try saved()); trust.update(nil)
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(paired)), .reject)
    }
    func testQRPinIsTemporaryUntilPairingPersistsAndFailureRestoresSavedTrust() throws {
        let trust = HomeNetworkTrust()
        XCTAssertThrowsError(try trust.preparePairing(origin: origin, pin: nil))
        try trust.preparePairing(origin: origin, pin: pin)
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(paired)), .accept)
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(impostor)), .reject)
        trust.update(nil)
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(paired)), .reject)
        trust.update(try saved())
        let restored = try JSONDecoder().decode(SavedConnection.self, from: JSONEncoder().encode(saved()))
        XCTAssertEqual(restored.hostKeyPin, pin)
        XCTAssertEqual(restored.routes.first?.kind, .home)
        trust.update(restored)
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(renewed)), .accept)
        try trust.preparePairing(origin: "https://studio.tail.ts.net", pin: nil)
        XCTAssertEqual(trust.decision(host: "studio.tail.ts.net", port: 443, certificate: nil), .system)
    }
    func testSharedClientsForgetRemovedPinsAndRejectConflictingHostsAtOneAddress() throws {
        let original = try saved()
        let other = try SavedConnection(routes: [ConnectionRoute(kind: .home, origin: "https://192.168.1.21:3778")],
            credential: original.credential, hostKeyPin: pin)
        let conflict = try SavedConnection(routes: original.routes, credential: original.credential, hostKeyPin: "another-key")
        let trust = HomeNetworkTrust()
        trust.update([original, other])
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(paired)), .accept)
        XCTAssertEqual(trust.decision(host: "192.168.1.21", port: 3778, certificate: certificate(paired)), .accept)
        trust.update([original, other, conflict])
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(paired)), .reject)
        trust.update([other])
        XCTAssertEqual(trust.decision(host: "192.168.1.20", port: 3778, certificate: certificate(paired)), .reject)
        var repairing = other; repairing.requiresPairing = true
        trust.update([repairing])
        XCTAssertEqual(trust.decision(host: "192.168.1.21", port: 3778, certificate: certificate(paired)), .reject)
    }

    func testPrivateAddressClassifierExcludesPublicLoopbackAndOverlayAddresses() {
        for origin in ["https://10.1.2.3:3778", "https://172.16.0.1:3778", "https://172.31.255.255:3778", "https://192.168.1.2:3778", "https://[fd12::1]:3778", "https://[fc00::1]:3778"] {
            XCTAssertTrue(HomeNetworkTrust.isHomeOrigin(origin), origin)
        }
        for origin in ["https://172.15.0.1:3778", "https://172.32.0.1:3778", "https://100.64.1.2:3778", "https://[fd7a:115c:a1e0::1]:3778", "https://127.0.0.1:3778", "https://169.254.1.2:3778", "https://[fe80::1]:3778", "https://8.8.8.8:3778", "https://[::ffff:192.168.1.2]:3778", "https://host.local:3778", "http://192.168.1.2:3778"] {
            XCTAssertFalse(HomeNetworkTrust.isHomeOrigin(origin), origin)
        }
    }
    #if os(iOS)
    @MainActor private static func pong(_ socket: URLSessionWebSocketTask) async throws {
        try await ConnectionRoutePong.wait { socket.sendPing(pongReceiveHandler: $0) }
    }
    @MainActor private static func boundedPong(_ socket: URLSessionWebSocketTask) async throws {
        try await withThrowingTaskGroup(of: Void.self) { group in
            group.addTask { try await Self.pong(socket) }
            group.addTask { try await Task.sleep(for: .seconds(6)); throw URLError(.timedOut) }
            defer { group.cancelAll() }
            try await group.next()
        }
    }
    /// Opt-in read-only integration against a disposable Mac daemon. The
    /// external fixture pairs its own device with normal signature checks;
    /// no provider turn or user chat is involved. Empty env skips this check.
    @MainActor func testLiveHomeClientPinsBothHTTPAndEventSocket() async throws {
        guard let input = ProcessInfo.processInfo.environment["WONDER_I3_LIVE"] else {
            throw XCTSkip("Needs an explicitly owned LAN daemon fixture")
        }
        struct Fixture: Decodable { let saved: SavedConnection; let privateKey: String; let impostor: Bool }
        let fixture = try JSONDecoder().decode(Fixture.self, from: Data(input.utf8))
        let raw = fixture.privateKey.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        let key = try P256.Signing.PrivateKey(rawRepresentation: XCTUnwrap(Data(base64Encoded: raw + "=")))
        let signer = SigningIdentity(read: { key.rawRepresentation }, save: { _ in }, restore: { _ in
            EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation, sign: { try key.signature(for: $0) })
        }, create: { throw SigningIdentityFailure.missing })
        // The production initializer restores the pin and installs the delegate;
        // this is the app's actual HTTP/WebSocket client, without an injected API.
        let saved = try JSONDecoder().decode(SavedConnection.self, from: JSONEncoder().encode(fixture.saved))
        let model = ConnectionModel(saved: saved, persistConnection: { _ in }, signingIdentity: signer)
        struct Ready: Decodable, Sendable { let ready: Bool }
        if fixture.impostor {
            do {
                let _: Ready = try await model.api.request("/readyz", origin: saved.origin)
                XCTFail("An impostor must not receive an HTTP request")
            } catch let error as URLError { XCTAssertEqual(error.code, .cancelled) }
            let socket = try model.api.eventSocket(connection: saved)
            defer { socket.cancel(with: .goingAway, reason: nil) }
            socket.resume()
            do {
                try await Self.boundedPong(socket)
                XCTFail("An impostor must not open a WebSocket")
            } catch let error as URLError { XCTAssertEqual(error.code, .cancelled) }
            print("I3 LIVE: impostor refused by HTTP and WebSocket; app test process remains alive")
            return
        }
        try await Self.checkPinnedClient(model, saved: saved, key: key)
        print("I3 LIVE: pinned HTTP readyz and authenticated devices passed; signed event socket answered two Pongs")
    }

    /// The genuine path of both live checks: the app's own client reaches the
    /// pinned home address over HTTP and over a device-signed event socket.
    @MainActor private static func checkPinnedClient(_ model: ConnectionModel, saved: SavedConnection, key: P256.Signing.PrivateKey) async throws {
        struct Ready: Decodable, Sendable { let ready: Bool }
        let ready: Ready = try await model.api.request("/readyz", origin: saved.origin)
        XCTAssertTrue(ready.ready)
        struct Device: Decodable, Sendable { let id: String; let revokedAt: String? }
        let devices: [Device] = try await model.api.request("/api/v1/devices", origin: saved.origin, credential: saved.credential)
        XCTAssertTrue(devices.contains { $0.id == saved.credential.deviceId && $0.revokedAt == nil })
        struct Host: Decodable, Sendable { let hostEpoch: String; let hostInstallationId: String }
        let host: Host = try await model.api.request("/api/v1/host/status", origin: saved.origin, credential: saved.credential)
        XCTAssertEqual(host.hostInstallationId, saved.credential.hostInstallationId)
        XCTAssertFalse(host.hostEpoch.isEmpty)
        // The daemon can have pruned sequence zero. Use the same authenticated
        // checkpoint as the app, rather than mistaking an intentional resync
        // close for a TLS or WebSocket failure.
        struct Checkpoint: Decodable, Sendable { let hostEpoch: String; let lastSequence: UInt64; let hostInstallationId: String }
        let checkpoint: Checkpoint = try await model.api.request("/api/v1/sync/checkpoint", origin: saved.origin, credential: saved.credential)
        XCTAssertEqual(checkpoint.hostInstallationId, saved.credential.hostInstallationId)
        XCTAssertEqual(checkpoint.hostEpoch, host.hostEpoch)
        let challenge: Challenge = try await model.api.request("/api/v1/events/challenge", origin: saved.origin, credential: saved.credential)
        try challenge.validate(origin: saved.origin, hostID: saved.credential.hostInstallationId, deviceID: saved.credential.deviceId)
        let signature = try key.signature(for: challenge.transcript).rawRepresentation.base64URL
        let socket = try model.api.eventSocket(connection: saved)
        defer { socket.cancel(with: .goingAway, reason: nil) }
        socket.resume()
        let payload = try JSONSerialization.data(withJSONObject: ["deviceId": saved.credential.deviceId,
            "hostEpoch": checkpoint.hostEpoch, "lastSequence": checkpoint.lastSequence, "csrfToken": saved.credential.csrfToken,
            "challengeId": challenge.challengeId, "signature": signature, "batch": true])
        try await socket.send(.string(String(decoding: payload, as: UTF8.self)))
        // Keep reading as the app's replay loop does. Foundation needs an
        // outstanding receive to keep processing a quiet stream's control frames.
        let receiver = Task { while !Task.isCancelled { _ = try await socket.receive() } }
        defer { receiver.cancel() }
        for _ in 0..<2 {
            try await Self.boundedPong(socket)
            try await Task.sleep(for: .milliseconds(250))
        }
    }

    /// Opt-in QR enrollment against a disposable daemon. The link's key pin must
    /// verify the computer's proof before the one-use offer is claimed. The test
    /// plays the owner: it reads the phone's verification text and confirms it on
    /// the computer through the daemon's loopback-only owner route.
    @MainActor func testLiveQREnrollmentChecksTheHostProofThenPinsTheConnection() async throws {
        guard let input = ProcessInfo.processInfo.environment["WONDER_I3_ENROLL"] else {
            throw XCTSkip("Needs an explicitly owned LAN daemon fixture")
        }
        struct Fixture: Decodable { let link: String; let loopback: String; let capability: String; let impostor: Bool }
        let fixture = try JSONDecoder().decode(Fixture.self, from: Data(input.utf8))
        let link = try PairingLink(fixture.link)
        let pin = try XCTUnwrap(link.hostKey, "The computer's pairing link must carry its key pin")
        let key = P256.Signing.PrivateKey()
        let signer = SigningIdentity(read: { key.rawRepresentation }, save: { _ in }, restore: { _ in
            EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation, sign: { try key.signature(for: $0) })
        }, create: { throw SigningIdentityFailure.missing })
        var persisted: SavedConnection?
        // The production client and trust delegate, without an injected API.
        let model = ConnectionModel(persistConnection: { persisted = $0 }, signingIdentity: signer)
        defer { model.cancel() }
        func owner(_ path: String, code: String? = nil) async throws -> Data {
            var request = URLRequest(url: try XCTUnwrap(URL(string: fixture.loopback + path)))
            request.setValue(fixture.capability, forHTTPHeaderField: "x-wonder-loopback-capability")
            request.setValue(link.origin, forHTTPHeaderField: "Origin")
            if let code {
                request.httpMethod = "POST"; request.httpBody = try JSONEncoder().encode(["code": code])
                request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            }
            let (data, response) = try await URLSession.shared.data(for: request)
            let status = try XCTUnwrap(response as? HTTPURLResponse).statusCode
            guard (200..<300).contains(status) else { throw PairingFailure.response(status) }
            return data
        }
        struct Pending: Decodable { let deviceId: String; let offerId: String }
        model.pair(link: fixture.link, address: "", code: "")
        if fixture.impostor {
            for _ in 0..<1500 where model.busy { try await Task.sleep(for: .milliseconds(10)) }
            XCTAssertFalse(model.busy)
            XCTAssertEqual(model.error, PairingFailure.wrongHost.errorDescription)
            XCTAssertNil(model.verification)
            XCTAssertNil(model.connection)
            XCTAssertNil(persisted)
            let pending = try JSONDecoder().decode([Pending].self, from: await owner("/api/v1/pairing/pending"))
            XCTAssertTrue(pending.isEmpty, "No claim may reach a computer that fails the QR pin")
            print("I3 LIVE: QR enrollment refused the impostor before claiming; it has no pending phone; app process alive; shown error: \(model.error ?? "none")")
            return
        }
        for _ in 0..<1500 where model.verification == nil && model.busy { try await Task.sleep(for: .milliseconds(10)) }
        let code = try XCTUnwrap(model.verification, model.error ?? "No verification text")
        let pending = try JSONDecoder().decode([Pending].self, from: await owner("/api/v1/pairing/pending"))
        let device = try XCTUnwrap(pending.first { $0.offerId == link.offerID })
        _ = try await owner("/api/v1/pairing/pending/\(device.deviceId)/confirm", code: code)
        for _ in 0..<1500 where model.busy { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertNil(model.error)
        let saved = try XCTUnwrap(persisted)
        XCTAssertEqual(saved.hostKeyPin, pin)
        XCTAssertEqual(saved.routes.map(\.kind), [.home])
        XCTAssertEqual(saved.origin, link.origin)
        XCTAssertEqual(saved.credential.deviceId, device.deviceId)
        XCTAssertEqual(saved.credential.hostInstallationId, link.hostID)
        // A relaunch restores the saved pin through the production initializer.
        let restored = try JSONDecoder().decode(SavedConnection.self, from: JSONEncoder().encode(saved))
        let relaunched = ConnectionModel(saved: restored, persistConnection: { _ in }, signingIdentity: signer)
        try await Self.checkPinnedClient(relaunched, saved: restored, key: key)
        print("I3 LIVE: QR proof verified before the claim; pin saved; relaunched client passed pinned HTTP and two event socket Pongs")
    }

    @MainActor func testEnrollmentVerifiesQRProofThenPersistsPinAndSignsTheBoundChallenge() async throws {
        let pin = RouteProofURLProtocol.reset(enrollment: true)
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let key = P256.Signing.PrivateKey()
        let signer = SigningIdentity(read: { key.rawRepresentation }, save: { _ in }, restore: { _ in
            EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation, sign: { try key.signature(for: $0) })
        }, create: { throw SigningIdentityFailure.missing })
        var persisted: SavedConnection?
        let model = ConnectionModel(persistConnection: { persisted = $0 }, api: PairingAPI(configuration: configuration), signingIdentity: signer)
        let link = origin + "/pair#secret=one-use-secret&offerId=00000000-0000-0000-0000-000000000001&hostInstallationId=host&hostKey=" + pin
        model.pair(link: link, address: "", code: "")
        for _ in 0..<200 where model.busy { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(model.busy)
        XCTAssertNil(model.error)
        let saved = try XCTUnwrap(persisted)
        XCTAssertEqual(saved.hostKeyPin, pin)
        XCTAssertEqual(saved.routes.map(\.kind), [.home])
        XCTAssertEqual(saved.origin, origin)
        XCTAssertEqual(model.connection?.credential.hostInstallationId, "host")
        let requests = RouteProofURLProtocol.requests
        XCTAssertEqual(requests.map { $0.url!.path }, ["/api/v1/host/proof", "/api/v1/pairing/claim", "/api/v1/pairing/session"])
        let signed = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(requests.last?.httpBody)) as? [String: String])
        XCTAssertEqual(signed["challengeId"], "challenge")
        let encoded = try XCTUnwrap(signed["signature"]).replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        let signature = try P256.Signing.ECDSASignature(rawRepresentation: XCTUnwrap(Data(base64Encoded: encoded + "==")))
        XCTAssertTrue(key.publicKey.isValidSignature(signature, for: try XCTUnwrap(RouteProofURLProtocol.enrollmentChallenge).transcript))
        model.cancel()
    }

    /// A wrong proof must fail before a one-use secret or device public key
    /// leaves the real enrollment flow. The model owns this ordering.
    @MainActor func testEnrollmentRefusesWrongProofBeforeClaimingTheOffer() async throws {
        _ = RouteProofURLProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let key = P256.Signing.PrivateKey()
        let signer = SigningIdentity(read: { key.rawRepresentation }, save: { _ in }, restore: { _ in
            EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation, sign: { try key.signature(for: $0) })
        }, create: { throw SigningIdentityFailure.missing })
        var persisted = false
        let model = ConnectionModel(persistConnection: { _ in persisted = true },
                                    api: PairingAPI(configuration: configuration), signingIdentity: signer)
        let link = origin + "/pair#secret=one-use-secret&offerId=00000000-0000-0000-0000-000000000001&hostInstallationId=host&hostKey=" + pin
        model.pair(link: link, address: "", code: "")
        for _ in 0..<200 where model.busy { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(model.busy)
        XCTAssertNotNil(model.error)
        XCTAssertNil(model.connection)
        XCTAssertFalse(persisted)
        let requests = RouteProofURLProtocol.requests
        XCTAssertEqual(requests.map { $0.url!.path }, ["/api/v1/host/proof"])
        XCTAssertTrue(requests.allSatisfy { $0.httpBody == nil && $0.httpBodyStream == nil && $0.value(forHTTPHeaderField: "Cookie") == nil })
        model.cancel()
    }

    /// B-023/B-448: a code can't pair with a home address (only the QR code and
    /// link carry its key), so it says which way works before touching the
    /// device key or the network. A bare Tailscale name still reaches the code route.
    @MainActor func testPairingCodeRefusesAHomeAddressAndAcceptsABareTailscaleName() async throws {
        _ = RouteProofURLProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let untouched = SigningIdentity(read: {
            XCTFail("A refused home code must not read the device key")
            throw SigningIdentityFailure.missing
        }, save: { _ in XCTFail("A refused home code must not save a key") }, restore: { _ in
            XCTFail("A refused home code must not restore a key")
            throw SigningIdentityFailure.missing
        }, create: {
            XCTFail("A refused home code must not create a key")
            throw SigningIdentityFailure.missing
        })
        var persisted = false
        let home = ConnectionModel(persistConnection: { _ in persisted = true },
                                   api: PairingAPI(configuration: configuration), signingIdentity: untouched)
        defer { home.cancel() }
        for address in ["192.168.1.20:3778", "https://192.168.1.20:3778", " [fd12::1]:3778 "] {
            home.pair(link: "", address: address, code: "ABCD-1234")
            for _ in 0..<200 where home.busy { try await Task.sleep(for: .milliseconds(10)) }
            XCTAssertFalse(home.busy)
            XCTAssertEqual(home.error, PairingFailure.homeNetworkCode.errorDescription, address)
        }
        XCTAssertTrue(RouteProofURLProtocol.requests.isEmpty)
        XCTAssertNil(home.connection)
        XCTAssertFalse(persisted)

        let key = P256.Signing.PrivateKey()
        let signer = SigningIdentity(read: { key.rawRepresentation }, save: { _ in }, restore: { _ in
            EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation, sign: { try key.signature(for: $0) })
        }, create: { throw SigningIdentityFailure.missing })
        let model = ConnectionModel(persistConnection: { _ in persisted = true },
                                    api: PairingAPI(configuration: configuration), signingIdentity: signer)
        defer { model.cancel() }
        model.pair(link: "", address: "studio.tail1234.ts.net:3778", code: "ABCD-1234")
        for _ in 0..<200 where model.busy { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(model.busy)
        let request = try XCTUnwrap(RouteProofURLProtocol.requests.first)
        XCTAssertEqual(request.url?.absoluteString, "https://studio.tail1234.ts.net:3778/api/v1/pairing/code")
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(request.httpBody)) as? [String: Any])
        XCTAssertEqual(body["humanCode"] as? String, "ABCD-1234")
        XCTAssertNotEqual(model.error, PairingFailure.homeNetworkCode.errorDescription)
        XCTAssertNotEqual(model.error, PairingFailure.invalidLink.errorDescription)
    }
    #endif

}
#endif
// MARK: End home network contracts

#if os(iOS)
import AuthenticationServices
import Combine
import XCTest
import CryptoKit
import UIKit
import UserNotifications
import AVFoundation
import SwiftUI
import ImageIO
import UniformTypeIdentifiers
import Vision
import PDFKit
import WonderPairing
@testable import Wonder

final class WonderDiagnosticsTests: XCTestCase {
    /// Native reply controls share the durable composer owner: they may propose
    /// text, but must preserve an existing intent and never begin delivery.
    @MainActor func testNativeReplyDraftPersistsWithoutSendingOrReplacingAnIntent() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let saved = Self.cameraSavedConnection()
        let chat = try Self.cameraChat(id: "native-reply-draft")
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat)
        XCTAssertEqual(model.fillEmptyDraft("Suggest a shorter route.", chat: chat.id), .saved)
        let reopened = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat)
        XCTAssertEqual(reopened.composers[chat.id]?.draft, "Suggest a shorter route.")
        XCTAssertNil(reopened.composers[chat.id]?.pending)
        XCTAssertEqual(reopened.fillEmptyDraft("Replace my draft", chat: chat.id), .occupied)
        XCTAssertEqual(reopened.fillEmptyDraft("Suggest a shorter route.", chat: chat.id), .occupied,
                       "A saved draft stays occupied even when a proposal has identical text")
        XCTAssertEqual(reopened.composers[chat.id]?.draft, "Suggest a shorter route.")
        var attached = ComposerIntent()
        attached.stagedFiles = [try StagedFile(name: "note.txt", mimeType: "text/plain", data: Data("Keep me".utf8))]
        let other = try Self.cameraChat(id: "native-reply-attachment")
        let attachmentModel = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: other, initialIntent: attached)
        XCTAssertEqual(attachmentModel.fillEmptyDraft("Use this instead", chat: other.id), .occupied)
        XCTAssertEqual(attachmentModel.composers[other.id]?.draft, "")
        XCTAssertEqual(attachmentModel.composers[other.id]?.stagedFiles?.first?.data, Data("Keep me".utf8))
        XCTAssertNil(attachmentModel.composers[other.id]?.pending)
    }

    @MainActor func testNativeReplyDraftReportsFailedSaveAndKeepsRecoverableText() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let saved = Self.cameraSavedConnection()
        let chat = try Self.cameraChat(id: "native-reply-save-failure")
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat)
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        try FileManager.default.setAttributes([.posixPermissions: 0o500], ofItemAtPath: store.directory.path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: store.directory.path) }
        XCTAssertEqual(model.fillEmptyDraft("Recover this draft", chat: chat.id),
                       .saveFailed("This draft could not be saved. Free some storage before sending."))
        XCTAssertEqual(model.fillEmptyDraft("Replace the unsaved draft", chat: chat.id), .occupied)
        XCTAssertNotNil(model.composerErrors[chat.id])
        XCTAssertEqual(model.composers[chat.id]?.draft, "Recover this draft")
        XCTAssertNil(model.composers[chat.id]?.pending)
        XCTAssertEqual(try store.loadComposer(conversation: chat.id).draft, "")
    }
    func testGoalUsagePreservesMissingAndNativeZero() throws {
        for fields in ["", #", "tokensUsed":null,"timeUsedSeconds":null"#] {
            let goal = try JSONDecoder().decode(ConversationGoal.self, from:
                Data((#"{"objective":"Review","status":"active","tokenBudget":1000,"timeBudgetSeconds":600"# + fields + "}").utf8))
            XCTAssertNil(goal.tokensUsed)
            XCTAssertNil(goal.timeUsedSeconds)
            XCTAssertEqual(goal.tokenBudget, 1000)
            XCTAssertEqual(goal.timeBudgetSeconds, 600)
        }
        for used in [0, 12345] {
            let goal = try JSONDecoder().decode(ConversationGoal.self, from:
                Data("{\"objective\":\"Review\",\"status\":\"active\",\"tokensUsed\":\(used),\"timeUsedSeconds\":\(used)}".utf8))
            XCTAssertEqual(goal.tokensUsed, used)
            XCTAssertEqual(goal.timeUsedSeconds, used)
        }
    }

    @MainActor func testLatestHistoryAppearsBeforeSlowOlderToolRefresh() async throws {
        for family in ["codex", "claude"] {
            try await checkLatestHistoryAppearsBeforeSlowOlderToolRefresh(family: family)
        }
    }
    @MainActor private func checkLatestHistoryAppearsBeforeSlowOlderToolRefresh(family: String) async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root, family: family)
        model.macConnected = true
        let path = "/api/v1/conversations/\(chat.id)"
        let original = Data(#"{"conversationId":"recovery-chat","hostEpoch":"epoch","lastSequence":1,"messages":[],"assistantMessages":[],"thread":{"hydrated":true,"turns":[{"id":"turn","status":"inProgress","items":[{"id":"old-tool","type":"commandExecution","state":"started","createdAt":"1000"}]}]}}"#.utf8)
        MessageRecoveryURLProtocol.enqueue(path: path, body: original)
        await model.retryConversation(chat)
        model.editDraft("Keep my draft", chat: chat.id)
        let newest = Data(#"{"conversationId":"recovery-chat","hostEpoch":"epoch","lastSequence":2,"messages":[],"assistantMessages":[],"thread":{"hydrated":true,"nextCursor":"older","turns":[{"id":"turn","status":"inProgress","items":[{"id":"fresh","type":"agentMessage","state":"completed","text":"Latest desktop reply","createdAt":"2000"}]}]}}"#.utf8)
        let older = Data(#"{"conversationId":"recovery-chat","hostEpoch":"epoch","lastSequence":2,"messages":[],"assistantMessages":[],"thread":{"hydrated":true,"turns":[{"id":"turn","status":"inProgress","items":[{"id":"old-tool","type":"commandExecution","state":"completed","createdAt":"1000"}]}]}}"#.utf8)
        MessageRecoveryURLProtocol.enqueue(path: path, body: newest)
        MessageRecoveryURLProtocol.enqueue(path: path + "/history", body: older)
        MessageRecoveryURLProtocol.hold(path: path + "/history")
        let refresh = Task { await model.retryConversation(chat) }
        try await waitForRecoveryRequests(path: path + "/history")
        XCTAssertEqual(model.snapshots[chat.id]?.rows(author: family).last?.text, "Latest desktop reply",
                       "The newest page must publish while an older page is still held")
        XCTAssertEqual(model.snapshots[chat.id]?.thread.nextCursor, "older", "The unread gap remains pageable")
        MessageRecoveryURLProtocol.releaseHeld()
        await refresh.value
        XCTAssertEqual(model.snapshots[chat.id]?.thread.turns?.last?.items.first?.state, "completed")
        XCTAssertEqual(model.snapshots[chat.id]?.thread.turns?.last?.items.map(\.id), ["old-tool", "fresh"])
        XCTAssertNil(model.snapshots[chat.id]?.thread.nextCursor)
        XCTAssertEqual(model.composers[chat.id]?.draft, "Keep my draft")
    }

    // A settle walk too long for one refresh goes on where it stopped at the
    // next, and a newest page that does not reach the saved turns keeps its
    // older-page cursor. Regressions: every refresh walked again from the
    // newest page and never reached an unfinished item past its page bound;
    // merging the saved copy (fully loaded) dropped the cursor to unread turns.
    @MainActor func testSettleRepairContinuesAcrossRefreshesAndKeepsGapsPageable() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        model.macConnected = true
        let path = "/api/v1/conversations/\(chat.id)"
        func page(_ turns: [String], deepTool: String = "completed", cursor: String?, sequence: Int) -> Data {
            let turns = turns.map { id -> [String: Any] in
                ["id": id, "status": id == "t1" ? "unknown" : "completed", "items": [[
                    "id": "tool-" + id, "type": "commandExecution", "createdAt": "1000",
                    "state": id == "t1" ? deepTool : "completed"]]]
            }
            var thread: [String: Any] = ["hydrated": true, "turns": turns]
            thread["nextCursor"] = cursor
            return try! JSONSerialization.data(withJSONObject: ["conversationId": chat.id, "hostEpoch": "epoch", "lastSequence": sequence,
                                                                "messages": [], "assistantMessages": [], "thread": thread])
        }
        func deepToolState() -> String? {
            model.snapshots[chat.id]?.thread.turns?.first { $0.id == "t1" }?.items.first?.state
        }
        MessageRecoveryURLProtocol.enqueue(path: path, body: page(["t1", "t2", "t3", "t4", "t5", "t6"], deepTool: "started", cursor: nil, sequence: 1))
        await model.refreshConversation(chat)
        XCTAssertEqual(deepToolState(), "started")
        XCTAssertEqual(ConnectionModel.settleRepairPages, 4, "The fixture's history is one page deeper than one walk")
        for _ in 0..<2 { MessageRecoveryURLProtocol.enqueue(path: path, body: page(["t6"], cursor: "c5", sequence: 2)) }
        for (cursor, turn) in [("c5", "t5"), ("c4", "t4"), ("c3", "t3"), ("c2", "t2")] {
            MessageRecoveryURLProtocol.enqueue(path: path + "/history?before=" + cursor,
                                               body: page([turn], cursor: "c" + String(Int(turn.dropFirst())! - 1), sequence: 2))
        }
        MessageRecoveryURLProtocol.enqueue(path: path + "/history?before=c1", body: page(["t1"], cursor: nil, sequence: 2))
        await model.refreshConversation(chat)
        XCTAssertEqual(deepToolState(), "started", "One refresh reads at most four older pages")
        await model.refreshConversation(chat)
        XCTAssertEqual(deepToolState(), "completed", "The next refresh goes on from the fifth page")
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path + "/history", includingEmpty: true).count, 5)
        XCTAssertNil(model.snapshots[chat.id]?.thread.nextCursor, "Everything is read")

        MessageRecoveryURLProtocol.enqueue(path: path, body: page(["t8", "t9"], cursor: "c7", sequence: 3))
        await model.refreshConversation(chat)
        XCTAssertEqual(model.snapshots[chat.id]?.thread.turns?.suffix(2).map(\.id), ["t8", "t9"])
        XCTAssertEqual(model.snapshots[chat.id]?.thread.nextCursor, "c7", "t7 was never read: Load older still reaches it")
    }

    @MainActor func testProgressiveDictationOwnsOnlyVolatileSelectionAndRejectsLateResults() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let chat = try Self.cameraChat(id: "dictation-editor")
        var initial = ComposerIntent(); initial.draft = "Hello old friend 👋"
        initial.stagedFiles = [try StagedFile(id: "attachment", name: "note.txt", mimeType: "text/plain", data: Data("keep".utf8))]
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: Self.cameraSavedConnection(), chat: chat, initialIntent: initial)
        let controller = model.dictation
        let editor = BoundedComposerEditor(text: Binding(get: { model.composers[chat.id]?.draft ?? "" },
            set: { model.editDraft($0, chat: chat.id) }), maximumLines: 6, label: "Message", editable: true,
            dictation: controller, conversationID: chat.id)
        let coordinator = editor.makeCoordinator()
        let view = UITextView(); view.text = initial.draft; view.delegate = coordinator; coordinator.view = view
        controller.attachEditor(coordinator, conversationID: chat.id)
        view.selectedRange = NSRange(location: 6, length: 3)
        let first = try XCTUnwrap(controller.beginNativeFixture(chat: chat))
        controller.receiveNativeFixture("blue", pending: first)
        controller.receiveNativeFixture("green card", pending: first)
        XCTAssertEqual(view.text, "Hello green card friend 👋")
        model.reloadCameraFixtureDraft(chat.id)
        XCTAssertEqual(model.composers[chat.id]?.draft, initial.draft, "Provisional words must never be persisted")
        XCTAssertEqual(view.selectedRange.location, 16)
        controller.cancel()
        XCTAssertEqual(view.text, initial.draft)
        XCTAssertEqual(view.selectedRange, NSRange(location: 6, length: 3))
        controller.receiveNativeFixture("late cancellation", pending: first, final: true)
        XCTAssertEqual(view.text, initial.draft)

        view.selectedRange = NSRange(location: 6, length: 3)
        let second = try XCTUnwrap(controller.beginNativeFixture(chat: chat))
        controller.receiveNativeFixture("new", pending: second)
        let range = view.selectedRange
        XCTAssertTrue(coordinator.textView(view, shouldChangeTextIn: range, replacementText: " manual"))
        view.text = (view.text as NSString).replacingCharacters(in: range, with: " manual")
        coordinator.textViewDidChange(view)
        controller.receiveNativeFixture("late revision", pending: second, final: true)
        model.reloadCameraFixtureDraft(chat.id)
        XCTAssertEqual(model.composers[chat.id]?.draft, "Hello new manual friend 👋")
        XCTAssertEqual(model.composers[chat.id]?.attachmentIDs, ["attachment"])
        XCTAssertNil(model.composers[chat.id]?.pending)

        view.selectedRange = NSRange(location: (view.text as NSString).length, length: 0)
        let third = try XCTUnwrap(controller.beginNativeFixture(chat: chat))
        controller.receiveNativeFixture("navigation", pending: third)
        controller.captureControlsHidden(conversationID: chat.id)
        controller.receiveNativeFixture("wrong conversation", pending: third, final: true)
        model.reloadCameraFixtureDraft(chat.id)
        XCTAssertEqual(model.composers[chat.id]?.draft, "Hello new manual friend 👋 navigation")
        XCTAssertNil(controller.nativeConversationID)

        let fourth = try XCTUnwrap(controller.beginNativeFixture(chat: chat))
        controller.receiveNativeFixture("discard on revocation", pending: fourth)
        controller.connectionChanged()
        controller.receiveNativeFixture("late revocation", pending: fourth, final: true)
        XCTAssertEqual(view.text, "Hello new manual friend 👋 navigation")
        XCTAssertEqual(model.composers[chat.id]?.attachmentIDs, ["attachment"])
    }

    @MainActor func testProgressiveDictationStopTimeoutAndBackgroundKeepWordsOnce() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let chat = try Self.cameraChat(id: "dictation-stop")
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: Self.cameraSavedConnection(), chat: chat)
        let controller = model.dictation
        let editor = BoundedComposerEditor(text: Binding(get: { model.composers[chat.id]?.draft ?? "" },
            set: { model.editDraft($0, chat: chat.id) }), maximumLines: 6, label: "Message", editable: true,
            dictation: controller, conversationID: chat.id)
        let coordinator = editor.makeCoordinator(), view = UITextView()
        coordinator.view = view; view.delegate = coordinator; view.text = ""
        controller.attachEditor(coordinator, conversationID: chat.id)
        for terminal in ["timeout", "background", "error", "selection", "interruption", "route"] {
            model.editDraft("", chat: chat.id); view.text = ""; coordinator.lastSynchronizedText = ""
            let pending = try XCTUnwrap(controller.beginNativeFixture(chat: chat))
            controller.receiveNativeFixture("First", pending: pending, final: true)
            XCTAssertTrue(controller.recording, "A finalized phrase must not stop listening")
            XCTAssertFalse(controller.blocksSending(conversationID: chat.id), "Send finishes an active capture")
            XCTAssertTrue(controller.hasUncommittedWords(conversationID: chat.id))
            XCTAssertFalse(controller.hasUncommittedWords(conversationID: "another-chat"))
            controller.receiveNativeFixture(" second", pending: pending, start: 1, end: 2)
            controller.receiveNativeFixture(" words", pending: pending, start: 1, end: 2)
            XCTAssertEqual(view.text, "First words")
            XCTAssertEqual(model.composers[chat.id]?.draft, "")
            switch terminal {
            case "timeout":
                controller.finishCapture()
                XCTAssertTrue(controller.processing)
                XCTAssertFalse(controller.blocksSending(conversationID: chat.id), "Send stays available while the recognizer drains")
                XCTAssertTrue(controller.hasUncommittedWords(conversationID: chat.id), "Draining words still count for Send")
                XCTAssertEqual(model.composers[chat.id]?.draft, "")
                try await Task.sleep(for: .milliseconds(2200))
            case "background": controller.foreground(false)
            case "error": controller.receiveNativeFixture("words", pending: pending, failed: true)
            case "interruption", "route":
                try await Task.sleep(for: .milliseconds(50))
                if terminal == "route" {
                    NotificationCenter.default.post(name: AVAudioSession.routeChangeNotification, object: nil,
                        userInfo: [AVAudioSessionRouteChangeReasonKey: AVAudioSession.RouteChangeReason.oldDeviceUnavailable.rawValue])
                } else { NotificationCenter.default.post(name: AVAudioSession.interruptionNotification, object: nil) }
                try await Task.sleep(for: .milliseconds(50))
            default:
                view.selectedRange = NSRange(location: 0, length: 0)
                coordinator.textViewDidChangeSelection(view)
                XCTAssertEqual(view.selectedRange.location, 0)
            }
            controller.receiveNativeFixture("late words", pending: pending, final: true)
            model.reloadCameraFixtureDraft(chat.id)
            XCTAssertEqual(model.composers[chat.id]?.draft, "First words", terminal)
            XCTAssertNil(controller.nativeConversationID, terminal)
            if terminal == "error" {
                XCTAssertNotNil(controller.visibleNotice(for: chat.id), "A dictation problem shows in its own chat")
                XCTAssertNil(controller.visibleNotice(for: "another-chat"), "Another chat does not inherit this chat's dictation problem")
            }
            XCTAssertFalse(controller.blocksSending(conversationID: chat.id))
            XCTAssertNil(model.composers[chat.id]?.pending)
        }
    }

    /// Move earlier/later sends the whole new order with each message's
    /// revision; a 409 (something changed or started) refreshes and moves
    /// nothing. A queue T3 Code holds any of cannot be reordered here.
    @MainActor func testQueuedMessagesMoveWithTheirRevisionsAndAChangedQueueRefreshes() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        func item(_ id: String, _ revision: Int, inT3Code: Bool = false) throws -> QueuedMessage {
            var fields: [String: Any] = ["id": id, "clientMessageId": "client-" + id, "body": "Body " + id, "revision": revision, "attachmentIds": [String]()]
            if inT3Code { fields["waitsInT3Code"] = true }
            return try JSONDecoder().decode(QueuedMessage.self, from: JSONSerialization.data(withJSONObject: fields))
        }
        let path = "/api/v1/conversations/recovery-chat/queue"
        func list(_ ids: [(String, Int)]) throws -> Data {
            try JSONSerialization.data(withJSONObject: ids.map { ["id": $0.0, "clientMessageId": "client-" + $0.0, "body": "Body " + $0.0, "revision": $0.1, "attachmentIds": [String]()] as [String: Any] })
        }
        model.queues[chat.id] = [try item("a", 1), try item("b", 3), try item("c", 2)]
        XCTAssertTrue(model.canReorderQueue(chat))
        MessageRecoveryURLProtocol.enqueue(path: path, method: "POST", status: 204)
        MessageRecoveryURLProtocol.enqueue(path: path, method: "GET", body: try list([("a", 2), ("c", 3), ("b", 4)]))
        try await model.moveQueued(chat, item: try item("c", 2), by: -1)
        let sent = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(MessageRecoveryURLProtocol.bodies(path: path).last)) as? [String: Any])
        let order = try XCTUnwrap(sent["items"] as? [[Any]])
        XCTAssertEqual(order.map { $0[0] as? String }, ["a", "c", "b"])
        XCTAssertEqual(order.map { $0[1] as? Int }, [1, 2, 3], "each message's revision as read")
        XCTAssertEqual(model.queues[chat.id]?.map(\.id), ["a", "c", "b"])

        MessageRecoveryURLProtocol.enqueue(path: path, method: "POST", status: 409, body: Data("The queue changed. Refresh before reordering.".utf8), contentType: "text/plain")
        MessageRecoveryURLProtocol.enqueue(path: path, method: "GET", body: try list([("c", 3), ("b", 4)]))
        do {
            try await model.moveQueued(chat, item: try item("c", 3), by: 1)
            XCTFail("a changed queue is refused")
        } catch PairingFailure.response(409) {}
        XCTAssertEqual(model.queues[chat.id]?.map(\.id), ["c", "b"], "the refreshed queue replaces the stale one")

        model.queues[chat.id] = [try item("held", 1, inT3Code: true), try item("here", 1)]
        XCTAssertFalse(model.canReorderQueue(chat), "T3 Code holds part of the queue")
        model.queues[chat.id] = [try item("only", 1)]
        XCTAssertFalse(model.canReorderQueue(chat))
    }

    @MainActor func testSendDuringDictationSendsTheFinishedTranscriptOnce() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        let controller = model.dictation
        let editor = BoundedComposerEditor(text: Binding(get: { model.composers[chat.id]?.draft ?? "" },
            set: { model.editDraft($0, chat: chat.id) }), maximumLines: 6, label: "Message", editable: true,
            dictation: controller, conversationID: chat.id)
        let coordinator = editor.makeCoordinator(), view = UITextView()
        coordinator.view = view; view.delegate = coordinator; view.text = ""
        controller.attachEditor(coordinator, conversationID: chat.id)
        let pending = try XCTUnwrap(controller.beginNativeFixture(chat: chat))
        controller.receiveNativeFixture("Ship the", pending: pending)
        XCTAssertEqual(model.composers[chat.id]?.draft ?? "", "", "Words stay uncommitted while recording")
        XCTAssertTrue(model.canSend(chat), "Spoken words alone enable Send")

        let first = Task { @MainActor in await model.send(chat) }
        for _ in 0..<100 where !controller.processing { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(controller.processing, "Send stops capture and drains the recognizer")
        XCTAssertTrue(model.finishingDictation.contains(chat.id), "Send shows it is preparing")
        XCTAssertFalse(model.canSend(chat), "A send already waiting for the words takes no second tap")
        // The recognizer's final revision arrives after the microphone stopped.
        controller.receiveNativeFixture("Ship the release", pending: pending, final: true)
        await model.send(chat)  // A second tap while finishing is ignored.
        await first.value

        let bodies = MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/recovery-chat/messages")
        XCTAssertEqual(bodies.count, 1)
        XCTAssertEqual(try JSONDecoder().decode(SendRequest.self, from: XCTUnwrap(bodies.first)).body, "Ship the release")
        XCTAssertNil(controller.intent)
        XCTAssertNil(controller.nativeConversationID)
        XCTAssertFalse(model.finishingDictation.contains(chat.id))

    }

    // B-040: an open preview sends back the computer's change token; while
    // the file is unchanged nothing is downloaded, hashed or re-shown, and
    // checks come less often. A changed file still arrives and is shown.
    @MainActor func testOpenWorkspaceFileRevalidatesWithItsRevisionToken() async throws {
        actor Requests {
            private var validators: [String?] = []
            func record(_ validator: String?) { validators.append(validator) }
            func all() -> [String?] { validators }
        }
        func waitUntil(_ condition: @MainActor () async -> Bool) async throws {
            for _ in 0..<300 where !(await condition()) { try await Task.sleep(for: .milliseconds(10)) }
        }
        let original = Data("old".utf8)
        let updated = Data("new".utf8)
        let first = "W/\"one\"", second = "W/\"two\""
        let revision = WorkspaceRevisionState(data: original, sha256: ConversationFile.digest(original), validator: first)
        let requests = Requests()
        var changed = false
        var bodies = 0
        var noticed: [String] = []
        let watch = Task {
            await revision.watch(mimeType: "text/plain", refresh: { validator, whole in
                await requests.record(validator)
                XCTAssertFalse(whole)
                let current = changed ? second : first
                if validator == current { return .unchanged }
                bodies += 1
                return .read(WorkspaceFileRead(data: changed ? updated : original, totalBytes: 3, validator: current))
            }, onRevision: { noticed.append($0) }, failureMessage: "Unreachable", interval: 0.02)
        }
        try await waitUntil { await requests.all().count >= 3 }
        let unchangedRequests = await requests.all()
        XCTAssertTrue(unchangedRequests.allSatisfy { $0 == first }, "Every check names the shown revision: \(unchangedRequests)")
        XCTAssertEqual(bodies, 0, "An unchanged file is not downloaded again")
        XCTAssertEqual(revision.currentData, original)
        XCTAssertEqual(noticed.last, ConversationFile.digest(original),
                       "Draft comments are still checked against the whole file's hash")

        changed = true
        try await waitUntil { revision.currentData == updated }
        XCTAssertEqual(revision.currentSha256, ConversationFile.digest(updated))
        XCTAssertEqual(revision.preparedText, "new")
        let seen = await requests.all().count
        try await waitUntil { await requests.all().count >= seen + 2 }
        let latest = await requests.all().last
        XCTAssertEqual(latest, second, "Later checks name the revision just read")
        XCTAssertEqual(bodies, 1)
        watch.cancel()
        await watch.value
        revision.close()

        // A body-free check starts at 3 s whatever the size and backs off
        // while nothing changes.
        XCTAssertEqual(WorkspaceRevisionState.pollInterval(byteCount: 200 * 1024 * 1024, conditional: true), 3)
        XCTAssertEqual(WorkspaceRevisionState.pollInterval(byteCount: 4 * 1024, conditional: true, unchangedChecks: 1), 6)
        XCTAssertEqual(WorkspaceRevisionState.pollInterval(byteCount: 4 * 1024, conditional: true, unchangedChecks: 9), 60)
        XCTAssertEqual(WorkspaceRevisionState.pollInterval(byteCount: 4 * 1024, unchangedChecks: 2), 12)
    }

    // B-277: opening or replacing a PDF never parses it on the main thread.
    // The view mounts at once with a spinner; the document arrives from the
    // preparer, and a revision that arrives mid-parse wins over the older one.
    // Logs the mount and parse times for a large PDF (the measurement).
    @MainActor func testPDFPreviewParsesOffTheMainThreadAndShowsTheNewestRevision() throws {
        func document(_ label: String, pages: Int) -> Data {
            UIGraphicsPDFRenderer(bounds: CGRect(x: 0, y: 0, width: 612, height: 792)).pdfData { context in
                for page in 1...pages {
                    context.beginPage()
                    ("\(label) page \(page)" as NSString).draw(at: CGPoint(x: 24, y: 24),
                        withAttributes: [.font: UIFont.systemFont(ofSize: 22)])
                    for line in 0..<40 {
                        ("Line \(line) of a long report with enough text to be realistic." as NSString)
                            .draw(at: CGPoint(x: 24, y: 64 + line * 17), withAttributes: [.font: UIFont.systemFont(ofSize: 12)])
                    }
                }
            }
        }
        func descendant<T: UIView>(_ type: T.Type, in view: UIView, where match: (T) -> Bool = { _ in true }) -> T? {
            if let view = view as? T, match(view) { return view }
            for subview in view.subviews { if let found = descendant(type, in: subview, where: match) { return found } }
            return nil
        }
        func firstPageText(_ view: PDFView) -> String { view.document?.page(at: 0)?.string ?? "" }
        let large = document("Large", pages: 400)
        let replacement = document("Replacement", pages: 400)
        let newest = document("Newest", pages: 2)
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        let previousWindow = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        let session = PDFPreviewSession()
        let mountStarted = ContinuousClock.now
        let host = UIHostingController(rootView: AnyView(PDFPreview(data: large, revision: "large", session: session)))
        window.rootViewController = host
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previousWindow?.makeKey()
        }
        host.view.layoutIfNeeded()
        let mounted = mountStarted.duration(to: .now)
        let viewer = try XCTUnwrap(descendant(PDFView.self, in: host.view))
        XCTAssertNil(viewer.document, "The PDF must not be parsed while the view is made")
        XCTAssertEqual(session.preparingRevision, "large")
        let spinner = descendant(UIActivityIndicatorView.self, in: viewer) { $0.accessibilityIdentifier == "workspace-pdf-loading" }
        XCTAssertEqual(spinner?.isAnimating, true, "Opening shows that the PDF is being prepared")
        let opened = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in viewer.document != nil }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [opened], timeout: 20), .completed)
        let ready = mountStarted.duration(to: .now)
        XCTAssertTrue(firstPageText(viewer).contains("Large page 1"))
        XCTAssertEqual(spinner?.isAnimating, false)
        XCTAssertTrue(session.showsRequestedRevision, "Comments are offered on the page that is shown")
        let shown = try XCTUnwrap(viewer.document)

        // A replacement keeps the current page on screen until it is ready;
        // a newer revision during that parse supersedes it.
        let replaceStarted = ContinuousClock.now
        host.rootView = AnyView(PDFPreview(data: replacement, revision: "replacement", session: session))
        host.view.layoutIfNeeded()
        let replaceMounted = replaceStarted.duration(to: .now)
        XCTAssertTrue(viewer.document === shown, "The open revision stays visible while the next one is parsed")
        XCTAssertFalse(session.showsRequestedRevision,
                       "No comment while the old page is shown for new bytes: it would anchor to a page the reader can't see")
        host.rootView = AnyView(PDFPreview(data: newest, revision: "newest", session: session))
        host.view.layoutIfNeeded()
        let replaced = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            session.preparingRevision == nil && session.revision == "newest"
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [replaced], timeout: 20), .completed)
        XCTAssertTrue(firstPageText(viewer).contains("Newest page 1"), firstPageText(viewer))
        XCTAssertTrue(viewer.document === session.document, "The session is the document's one owner")
        XCTAssertTrue(session.showsRequestedRevision, "Comments come back once the new revision is on screen")
        // A superseded parse finishing late must not bring the old revision back.
        RunLoop.main.run(until: Date().addingTimeInterval(0.5))
        XCTAssertTrue(firstPageText(viewer).contains("Newest page 1"), firstPageText(viewer))
        let measurement = "PDF preview, 400 pages (\(large.count) bytes): mount \(mounted), ready \(ready); "
            + "replacement mount \(replaceMounted)"
        print(measurement)
        let attachment = XCTAttachment(string: measurement)
        attachment.name = "PDF preview timings"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    // B-253: Files and a file opened from a reply show the same bounded text:
    // at most its first 128 KB, cut on a character boundary, prepared off the
    // main thread; a large HTML page is refused rather than rendered whole.
    func testPreviewTextIsTheFirst128KBOfTheVerifiedFile() async {
        let small = Data("Saturday: breakfast and a walk.".utf8)
        let shortResult = await WorkspaceRevisionState.prepareText(small, mimeType: "text/plain")
        XCTAssertEqual(shortResult.0, "Saturday: breakfast and a walk.")
        XCTAssertFalse(shortResult.1)
        // "é" is two bytes; one falls across the limit.
        let long = Data((String(repeating: "a", count: 128 * 1024 - 1) + String(repeating: "é", count: 4 * 1024 * 1024)).utf8)
        let longResult = await WorkspaceRevisionState.prepareText(long, mimeType: "text/plain")
        let shownText = longResult.0
        XCTAssertTrue(longResult.1)
        XCTAssertLessThanOrEqual(shownText?.utf8.count ?? .max, 128 * 1024)
        XCTAssertEqual(shownText?.utf8.count, 128 * 1024 - 1, "The cut never splits a character")
        XCTAssertTrue(long.starts(with: Data((shownText ?? "").utf8)), "The preview is an exact prefix of the file")
        let page = Data(("<p>" + String(repeating: "x", count: 200 * 1024) + "</p>").utf8)
        let pageResult = await WorkspaceRevisionState.prepareText(page, mimeType: "text/html")
        XCTAssertNil(pageResult.0)
        XCTAssertTrue(pageResult.1)
        let binary = await WorkspaceRevisionState.prepareText(Data([0xff, 0xfe, 0x00]), mimeType: "text/plain")
        XCTAssertNil(binary.0)
        XCTAssertFalse(binary.1)
        // A reply's file without a recorded SHA-256 still has a stable revision.
        let unhashed = try? JSONDecoder().decode(ConversationFile.self, from: Data(
            #"{"id":"f","name":"f.pdf","mimeType":"application/pdf","state":"available","updatedAt":"now"}"#.utf8))
        XCTAssertEqual(unhashed.map(PreviewDeck.revision(of:)), "f@now")
    }

    /// SwiftUI's nodes answer `accessibilityIdentifier` without declaring the protocol.
    private func accessibilityIdentifier(_ object: NSObject) -> String? {
        object.responds(to: #selector(getter: UIAccessibilityIdentification.accessibilityIdentifier))
            ? object.value(forKey: "accessibilityIdentifier") as? String : nil
    }

    // The host's setup link becomes Connect only for an app that isn't usable
    // yet and only toward the vendor pages the host itself allows.
    @MainActor func testConnectedAppDecodesSetupLinkForConnect() throws {
        func decode(_ fields: [String: Any]) throws -> ConnectedApp {
            try JSONDecoder().decode(ConnectedApp.self, from: JSONSerialization.data(withJSONObject: (["id": "codex:linear", "name": "Linear"] as [String: Any]).merging(fields) { $1 }))
        }
        let older = try decode(["status": "not_connected"])
        XCTAssertNil(older.setupUrl, "Older hosts send no setup link")
        XCTAssertNil(older.connectURL)
        XCTAssertEqual(older.label, "Not connected")
        let chatGPT = "https://chatgpt.com/apps/linear"
        XCTAssertEqual(try decode(["status": "not_connected", "setupUrl": chatGPT]).connectURL?.absoluteString, chatGPT)
        XCTAssertEqual(try decode(["status": "disabled", "setupUrl": chatGPT]).connectURL?.absoluteString, chatGPT)
        XCTAssertEqual(try decode(["status": "not_connected", "setupUrl": "https://claude.ai/settings/connectors"]).connectURL?.host(), "claude.ai")
        for status in ["available", "unavailable", "unknown"] {
            XCTAssertNil(try decode(["status": status, "setupUrl": chatGPT]).connectURL, status)
        }
        for url in ["http://chatgpt.com/apps/linear", "https://chatgpt.com/settings", "https://chatgpt.com:8443/apps/linear",
                    "https://evil.test/apps/linear", "https://user@chatgpt.com/apps/linear", "https://chatgpt.com/apps/linear?next=x",
                    "https://claude.ai/settings/connectors#x", "not a url"] {
            XCTAssertNil(try decode(["status": "not_connected", "setupUrl": url]).connectURL, url)
        }
        XCTAssertNil(try decode(["status": "not_connected", "setupUrl": NSNull()]).connectURL)
    }

    func testPushPreviewAuthenticatesContentAndRouting() throws {
        let key = try XCTUnwrap(Data(pushBase64URL: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"))
        var payload: [String: String] = ["eventId": "00000000-0000-4000-8000-000000000003", "preview": "AAECAwQFBgcICQoLPCC3a7WXrW3sLbWxyssZDve_6Fq-FDEfXUXfpy1ZMIIxIJ7MgvEiqESJS924twUA3mlQoGrmk-oPpxopKNPF2tIQ5BujoVQOajXGJ4utNMpK_6cWHw42UpyN7r4Xhtjh5sl7m4hjqnX6ZRyIaidUEZoTCs7DsUDpOYxokeyIHmObJmW1wIulfH-OOspOqi2hU9uXWlF-4GigfYsYlKSMnOD26nyGkYUahVn_nLb48s7vq-dW49kW3QH97OUZqsSHWiL5jmyxuqhDP7yI8yX7vsMYkQ5JdciUXPn1tDVqo9g3SqDDgwI5BwYo3GeVcAwhiFEOVdxA-dd4HQYfq-4n8GG0geT4vmpDh07aOXZYGzlquQ-OD51DUQ", "registrationId": "00000000-0000-4000-8000-000000000001", "routeId": "00000000-0000-4000-8000-000000000002"]
        let decoded = try PushPreview.decrypt(payload, key: key)
        XCTAssertEqual(decoded.title, "Todo app · Approval requested")
        XCTAssertEqual(decoded.body, "Run the tests?\nnpm test\n/workspace/todos")
        XCTAssertEqual(decoded.approval?.approvalId, "runtime:approval-1")
        XCTAssertEqual(decoded.threadIdentifier.count, 64)
        var other = decoded
        other.hostInstallationId = "another-host"
        XCTAssertNotEqual(decoded.threadIdentifier, other.threadIdentifier)
        other = decoded; other.conversationId = "another-thread"
        XCTAssertNotEqual(decoded.threadIdentifier, other.threadIdentifier)
        other = decoded
        XCTAssertEqual(decoded.threadIdentifier, other.threadIdentifier, "A new route does not change a thread's grouping")
        var changed = payload
        var bytes = [UInt8](try XCTUnwrap(Data(pushBase64URL: payload["preview"]!)))
        bytes[30] ^= 1; changed["preview"] = Data(bytes).base64URL
        XCTAssertThrowsError(try PushPreview.decrypt(changed, key: key), "Action references and grouping must authenticate with the text")
        for field in ["registrationId", "routeId", "eventId"] {
            var wrong = payload; wrong[field] = UUID().uuidString.lowercased()
            XCTAssertThrowsError(try PushPreview.decrypt(wrong, key: key))
        }
        XCTAssertThrowsError(try PushPreview.decrypt(payload, key: Data(repeating: 0, count: 32)))
        payload["preview"] = String(repeating: "a", count: 3241)
        XCTAssertThrowsError(try PushPreview.decrypt(payload, key: key))
    }

    @MainActor private final class NotificationStorage {
        var values: [String: Data] = [:]
        var failures: [[String: String]] = []
    }
    private let notificationRegistration = "00000000-0000-4000-8000-000000000001"
    private let notificationRoute = "00000000-0000-4000-8000-000000000002"
    private let notificationNonce = "00000000-0000-4000-8000-000000000004"
    private var notificationKey: Data { Data(0..<32) }
    private func notificationRequest(_ id: String, nonce: String? = nil, feedback: Bool = false) -> Data {
        Data(#"{"approvalId":"\#(id)","actionNonce":"\#(nonce ?? notificationNonce)","conversationId":"project-chat","method":"item/commandExecution/requestApproval","params":{"supportsDenyMessage":\#(feedback),"kind":"command","command":"npm test","cwd":"/workspace/todos","reason":"Run the tests?","availableDecisions":["accept","decline"]}}"#.utf8)
    }
    private func notificationPayload(_ id: String, host: String = "camera-unit-host", feedback: Bool = false) throws -> [String: String] {
        let preview = PushPreview(title: "Todo app · Approval requested", body: "Run the tests?\nnpm test\n/workspace/todos",
            hostInstallationId: host, conversationId: "project-chat", approval: .init(approvalId: id, actionNonce: notificationNonce, supportsDenyMessage: feedback))
        return try encryptedPush(preview)
    }
    private func encryptedPush(_ preview: PushPreview, registration: String? = nil) throws -> [String: String] {
        let registration = registration ?? notificationRegistration
        var payload = ["registrationId": registration, "routeId": notificationRoute, "eventId": UUID().uuidString]
        let aad = Data("wonder-push-v1\n\(registration)\n\(notificationRoute)\n\(payload["eventId"]!)".utf8)
        payload["preview"] = try AES.GCM.seal(JSONEncoder().encode(preview), using: SymmetricKey(data: notificationKey), authenticating: aad).combined!.base64URL
        return payload
    }
    @MainActor private func notificationHarness(_ model: ConnectionModel, selectionDirectory: URL? = nil) throws -> (push: PushNotifications, storage: NotificationStorage, library: ConnectionLibrary) {
        let storage = NotificationStorage(), saved = Self.cameraSavedConnection()
        var record = PushRegistration(host: saved.credential.hostInstallationId, device: saved.credential.deviceId,
                                      endpoint: "https://push.example.test", key: Data(repeating: 1, count: 32))
        record.id = notificationRegistration
        storage.values["push-registrations-v1"] = try JSONEncoder().encode([record.host: record])
        let push = PushNotifications(read: { storage.values[$0] }, write: { storage.values[$1] = $0 }, registerForPush: {},
                                     previewKey: { _ in self.notificationKey }, selectionDirectory: selectionDirectory, reportActionFailure: { storage.failures.append($0) })
        let library = ConnectionLibrary(diagnosticModels: [model])
        push.attach(library)
        return (push, storage, library)
    }
    private func enqueueNotificationLookup(_ requests: [Data], conversation: String = "project-chat") throws {
        try enqueueRenewal()
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/routes/\(notificationRoute)", body: Data(#"{"conversationId":"\#(conversation)"}"#.utf8))
        let list = Data(("[" + requests.map { String(decoding: $0, as: UTF8.self) }.joined(separator: ",") + "]").utf8)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/approvals", body: list)
    }

    /// Two refreshes of the pending requests can finish out of order. The
    /// older one's empty list must neither hide a request the newer one showed
    /// nor delete the answer the user saved for it.

    @MainActor func testNotificationActionSignsExactRequestAndRetainsUncertainRetry() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }, payload = try notificationPayload("one")
        let harness = try notificationHarness(model), path = "/api/v1/approvals/one/resolve"
        try enqueueNotificationLookup([notificationRequest("one")])
        MessageRecoveryURLProtocol.fail(path: path, error: .networkConnectionLost)
        await harness.push.receiveAction(payload, decision: "accept")
        XCTAssertTrue(harness.push.canRetryAction)
        XCTAssertNotNil(harness.push.routingError)
        for _ in 0..<10 where harness.storage.failures.isEmpty { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertEqual(harness.storage.failures.count, 1)
        let intent = try XCTUnwrap(model.savedDecisions["one"])
        XCTAssertEqual(intent.decision, "accept")
        try enqueueNotificationLookup([notificationRequest("one")])
        MessageRecoveryURLProtocol.enqueue(path: path)
        MessageRecoveryURLProtocol.hold(path: "/api/v1/approvals", after: 1)
        defer { MessageRecoveryURLProtocol.releaseHeld() }
        await harness.push.receiveAction(payload, decision: "accept", timeout: .milliseconds(500))
        XCTAssertFalse(harness.push.canRetryAction, "The signed acknowledgement must not wait for an optional attention refresh")
        XCTAssertNil(harness.push.routingError)
        let bodies = MessageRecoveryURLProtocol.bodies(path: path)
        XCTAssertEqual(bodies.count, 2)
        for body in bodies {
            let value = try XCTUnwrap(JSONSerialization.jsonObject(with: body) as? [String: Any])
            XCTAssertEqual(value["idempotencyKey"] as? String, intent.idempotencyKey)
            XCTAssertEqual(value["actionNonce"] as? String, notificationNonce)
            XCTAssertEqual(value["decision"] as? String, "accept")
            XCTAssertFalse((value["signature"] as? String ?? "").isEmpty)
        }
    }

    @MainActor func testNotificationInstructionCategoriesRequireAuthenticationAndCapability() throws {
        for feedback in [false, true] {
            let preview = try PushPreview.decrypt(notificationPayload("one", feedback: feedback), key: notificationKey)
            let category = try XCTUnwrap(PushAppDelegate.approvalCategories.first { $0.identifier == preview.categoryIdentifier })
            XCTAssertEqual(category.actions.count, feedback ? 3 : 2)
            XCTAssertTrue(category.actions.allSatisfy { $0.options.contains(.authenticationRequired) })
            let input = category.actions.compactMap { $0 as? UNTextInputNotificationAction }
            XCTAssertEqual(input.count, feedback ? 1 : 0)
            XCTAssertEqual(input.first?.identifier, feedback ? PushPreview.denyMessageAction : nil)
        }
    }

    private var notificationQuestionBody: String { "Tone\nWhich tone?\nBrief: One paragraph\nDetailed" }
    private func notificationQuestionPayload(registration: String? = nil) throws -> [String: String] {
        let preview = PushPreview(title: "Todo app · Question", body: notificationQuestionBody,
            hostInstallationId: "camera-unit-host", conversationId: "project-chat",
            approval: .init(approvalId: "question", actionNonce: notificationNonce, questionId: "tone:0"))
        return try encryptedPush(preview, registration: registration)
    }
    private func notificationQuestionRequest(_ condition: String = "") throws -> Data {
        var question: [String: Any] = ["id":"tone:0", "header":"Tone", "question":"Which tone?", "isOther":true, "isSecret":false,
            "options":[["label":"Brief", "description":"One paragraph"], ["label":"Detailed"]]]
        if condition == "secret" { question["isSecret"] = true }
        if condition == "choices" { question["isOther"] = false }
        if condition == "multiSelect" { question["multiSelect"] = true }
        if condition == "id" { question["id"] = "another" }
        var params: [String: Any] = ["questions": condition == "multiple" ? [question, question] : [question]]
        if condition != "capability" {
            params["notificationQuestion"] = ["id":"tone:0", "body":condition == "body" ? "Changed question" : notificationQuestionBody]
        }
        return try JSONSerialization.data(withJSONObject: ["approvalId":"question", "actionNonce":condition == "nonce" ? "changed" : notificationNonce,
            "conversationId":"project-chat", "method":"item/tool/requestUserInput", "params":params])
    }

    @MainActor func testNotificationQuestionReplyIsAuthenticatedAndPreservesExactSavedAnswer() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model), payload = try notificationQuestionPayload()
        let preview = try PushPreview.decrypt(payload, key: notificationKey)
        let category = try XCTUnwrap(PushAppDelegate.approvalCategories.first { $0.identifier == preview.categoryIdentifier })
        XCTAssertEqual(category.actions.count, 1)
        let input = try XCTUnwrap(category.actions.first as? UNTextInputNotificationAction)
        XCTAssertEqual(input.identifier, PushPreview.replyAction)
        XCTAssertTrue(input.options.contains(.authenticationRequired))
        let answer = "  Café, please.\nKeep the accents — thanks.  ", path = "/api/v1/approvals/question/resolve"
        try enqueueNotificationLookup([notificationQuestionRequest()])
        MessageRecoveryURLProtocol.fail(path: path, error: .networkConnectionLost)
        await harness.push.receiveAction(payload, decision: "respond", message: answer)
        let intent = try XCTUnwrap(model.savedDecisions["question"]), response = try XCTUnwrap(intent.responseJson)
        let value = try JSONDecoder().decode([String: [String: [String: [String]]]].self, from: Data(response.utf8))
        XCTAssertEqual(value["answers"]?["tone:0"]?["answers"], [answer])
        XCTAssertEqual(intent.decision, "respond")
        await harness.push.receiveAction(payload, decision: "respond", message: "Changed answer")
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 1)
        let restored = PushNotifications(read: { harness.storage.values[$0] }, write: { harness.storage.values[$1] = $0 }, registerForPush: {},
            previewKey: { _ in self.notificationKey }, reportActionFailure: { harness.storage.failures.append($0) })
        restored.attach(harness.library)
        try enqueueNotificationLookup([notificationQuestionRequest()])
        MessageRecoveryURLProtocol.enqueue(path: path)
        restored.retryActions()
        try await waitForRecoveryRequests(path: path, count: 2)
        for _ in 0..<50 where restored.canRetryAction { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(restored.canRetryAction)
        for body in MessageRecoveryURLProtocol.bodies(path: path) {
            let value = try XCTUnwrap(JSONSerialization.jsonObject(with: body) as? [String: Any])
            XCTAssertEqual(value["responseJson"] as? String, response)
            XCTAssertEqual(value["idempotencyKey"] as? String, intent.idempotencyKey)
            XCTAssertEqual(value["actionNonce"] as? String, notificationNonce)
            XCTAssertFalse((value["signature"] as? String ?? "").isEmpty)
        }
        for _ in 0..<10 where harness.storage.failures.isEmpty { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(harness.storage.failures.isEmpty)
        for failure in harness.storage.failures {
            XCTAssertEqual(failure, payload, "Recovery carries encrypted content, never the question or answer")
        }
    }

    @MainActor func testNotificationQuestionRejectsIncompleteChangedAndUnsupportedReplies() async throws {
        for condition in ["blank", "oversized", "missingAnswer", "command", "nonce", "route", "registration", "id", "body", "capability", "secret", "choices", "multiSelect", "multiple"] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
            try enqueueNotificationLookup([notificationQuestionRequest(condition)], conversation: condition == "route" ? "other-chat" : "project-chat")
            let text: String?
            switch condition {
            case "blank": text = " \n"
            case "oversized": text = String(repeating: "é", count: 1001)
            case "missingAnswer": text = nil
            default: text = "Friendly"
            }
            let registration = condition == "registration" ? UUID().uuidString : nil
            await harness.push.receiveAction(try notificationQuestionPayload(registration: registration), decision: condition == "command" ? "accept" : "respond", message: text)
            XCTAssertNotNil(harness.push.routingError, condition)
            XCTAssertFalse(harness.push.canRetryAction, condition)
            XCTAssertNil(model.savedDecisions["question"], condition)
            XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/approvals/question/resolve").isEmpty, condition)
        }
    }

    private func notificationOptions(_ count: Int) -> [PushPreview.QuestionOption] {
        (0..<count).map { .init(label: $0 == 0 ? "Brief" : "Option \($0 + 1)", description: $0 == 0 ? "One paragraph" : nil) }
    }
    private func notificationOptionPayload(_ count: Int = 2) throws -> [String: String] {
        try encryptedPush(PushPreview(title: "Todo app · Question", body: "Tone\nWhich tone?",
            hostInstallationId: "camera-unit-host", conversationId: "project-chat",
            question: .init(approvalId: "question", actionNonce: notificationNonce, questionId: "tone:0", questionOptions: notificationOptions(count))))
    }
    private func notificationOptionRequest(_ count: Int = 2, condition: String = "") throws -> Data {
        var raw = try XCTUnwrap(JSONSerialization.jsonObject(with: notificationQuestionRequest(condition)) as? [String: Any])
        var params = try XCTUnwrap(raw["params"] as? [String: Any])
        var questions = try XCTUnwrap(params["questions"] as? [[String: Any]])
        var options = notificationOptions(count)
        if condition == "label" { options[0] = .init(label: "Changed", description: "One paragraph") }
        if condition == "description" { options[0] = .init(label: "Brief", description: "Changed") }
        if condition == "order" { options.reverse() }
        questions[0]["options"] = try JSONSerialization.jsonObject(with: JSONEncoder().encode(options))
        if condition == "prompt" { questions[0]["question"] = "Changed prompt" }
        params.removeValue(forKey: "notificationQuestion")
        if count <= 4, !["capability", "secret", "multiSelect", "multiple"].contains(condition) {
            let prompt = [questions[0]["header"] as? String ?? "", questions[0]["question"] as? String ?? ""].filter { !$0.isEmpty }
            let body = (prompt + options.map { option in option.description.map { option.label + ": " + $0 } ?? option.label }).joined(separator: "\n")
            params["notificationQuestion"] = ["id": questions[0]["id"] as? String ?? "tone:0", "body": body]
        }
        params["questions"] = questions
        raw["params"] = params
        return try JSONSerialization.data(withJSONObject: raw)
    }

    @MainActor func testNotificationOptionAndOtherAnswersUseSignedDurableResolution() async throws {
        let cases: [(Int, Int?, String?)] = [(2, 1, nil), (3, 2, nil), (4, 3, nil), (5, 4, nil), (2, nil, "  Café, please.\nKeep the accents.  ")]
        for (count, index, text) in cases {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
            try enqueueNotificationLookup([notificationOptionRequest(count)])
            let path = "/api/v1/approvals/question/resolve"
            MessageRecoveryURLProtocol.enqueue(path: path)
            await harness.push.receiveAction(try notificationOptionPayload(count), decision: "respond", message: text, optionIndex: index)
            XCTAssertNil(harness.push.routingError)
            XCTAssertFalse(harness.push.canRetryAction)
            XCTAssertTrue(harness.storage.failures.isEmpty)
            let body = try XCTUnwrap(MessageRecoveryURLProtocol.bodies(path: path).first)
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 1)
            let value = try XCTUnwrap(JSONSerialization.jsonObject(with: body) as? [String: Any])
            let response = try XCTUnwrap(value["responseJson"] as? String)
            let answer = try JSONDecoder().decode([String: [String: [String: [String]]]].self, from: Data(response.utf8))
            XCTAssertEqual(answer["answers"]?["tone:0"]?["answers"], [index.map { notificationOptions(count)[$0].label } ?? text!])
            XCTAssertEqual(value["actionNonce"] as? String, notificationNonce)
            XCTAssertFalse((value["signature"] as? String ?? "").isEmpty)
            XCTAssertFalse((value["idempotencyKey"] as? String ?? "").isEmpty)
        }
    }

    @MainActor func testNotificationOptionsRejectStaleChoicesAndInvalidIndices() async throws {
        for condition in ["label", "description", "order", "nonce", "prompt", "id", "secret", "multiSelect", "multiple", "negative", "pastEnd", "offline"] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
            if condition == "offline" { MessageRecoveryURLProtocol.fail(path: "/api/v1/approvals", error: .notConnectedToInternet) }
            try enqueueNotificationLookup([notificationOptionRequest(condition: condition)])
            let payload = try notificationOptionPayload()
            await harness.push.receiveAction(payload, decision: "respond", optionIndex: condition == "negative" ? -1 : condition == "pastEnd" ? 2 : 0)
            XCTAssertNotNil(harness.push.routingError, condition)
            XCTAssertEqual(harness.push.canRetryAction, condition == "offline", condition)
            XCTAssertNil(model.savedDecisions["question"], condition)
            XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/approvals/question/resolve").isEmpty, condition)
            if condition == "offline" {
                for _ in 0..<10 where harness.storage.failures.isEmpty { try await Task.sleep(for: .milliseconds(10)) }
                XCTAssertEqual(harness.storage.failures, [payload])
                XCTAssertTrue(harness.push.routingError?.contains("Reply not confirmed") == true)
            }
        }
    }

    @MainActor func testNotificationOptionRetryKeepsOriginalChoiceAcrossColdLaunch() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model), payload = try notificationOptionPayload(5)
        let path = "/api/v1/approvals/question/resolve"
        try enqueueNotificationLookup([notificationOptionRequest(5)])
        MessageRecoveryURLProtocol.fail(path: path, error: .networkConnectionLost)
        await harness.push.receiveAction(payload, decision: "respond", optionIndex: 4)
        XCTAssertTrue(harness.push.canRetryAction)
        XCTAssertNotNil(harness.push.routingError)
        let intent = try XCTUnwrap(model.savedDecisions["question"])
        await harness.push.receiveAction(payload, decision: "respond", optionIndex: 0)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 1)
        let restored = PushNotifications(read: { harness.storage.values[$0] }, write: { harness.storage.values[$1] = $0 }, registerForPush: {},
            previewKey: { _ in self.notificationKey }, reportActionFailure: { harness.storage.failures.append($0) })
        restored.attach(harness.library)
        try enqueueNotificationLookup([notificationOptionRequest(5)])
        MessageRecoveryURLProtocol.enqueue(path: path)
        restored.retryActions()
        try await waitForRecoveryRequests(path: path, count: 2)
        for _ in 0..<50 where restored.canRetryAction { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(restored.canRetryAction)
        for data in MessageRecoveryURLProtocol.bodies(path: path) {
            let body = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
            XCTAssertEqual(body["responseJson"] as? String, intent.responseJson)
            XCTAssertEqual(body["idempotencyKey"] as? String, intent.idempotencyKey)
        }
    }

    func testNotificationSelectionHandoffBindsEncryptedPayloadAndKeepsFirstChoice() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let payload = try notificationOptionPayload(5), another = try notificationOptionPayload(5)
        try PushQuestionSelection.save(4, payload: payload, directory: root)
        XCTAssertEqual(try PushQuestionSelection.read(payload, directory: root), 4)
        XCTAssertNil(try PushQuestionSelection.read(another, directory: root))
        XCTAssertThrowsError(try PushQuestionSelection.save(1, payload: payload, directory: root))
        XCTAssertEqual(try PushQuestionSelection.read(payload, directory: root), 4)
        XCTAssertThrowsError(try PushQuestionSelection.save(32, payload: another, directory: root))
        try PushQuestionSelection.remove(payload, directory: root)
        XCTAssertNil(try PushQuestionSelection.read(payload, directory: root))
        for _ in 0..<32 { try PushQuestionSelection.save(0, payload: notificationOptionPayload(), directory: root) }
        XCTAssertThrowsError(try PushQuestionSelection.save(0, payload: notificationOptionPayload(), directory: root), "The extension handoff queue must remain bounded")
    }

    @MainActor func testNotificationContentHandoffSavesDurableReplyBeforeClearingSelection() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let selections = root.appendingPathComponent("selection")
        let model = renewalModel(root: root.appendingPathComponent("model")) { _ in }
        let harness = try notificationHarness(model, selectionDirectory: selections), payload = try notificationOptionPayload(5)
        try PushQuestionSelection.save(4, payload: payload, directory: selections)
        try enqueueNotificationLookup([notificationOptionRequest(5)])
        let path = "/api/v1/approvals/question/resolve"
        MessageRecoveryURLProtocol.enqueue(path: path)
        let handled = await harness.push.receiveForwardedSelection(payload)
        XCTAssertTrue(handled)
        XCTAssertNil(harness.push.routingError)
        XCTAssertFalse(harness.push.canRetryAction)
        XCTAssertNil(try PushQuestionSelection.read(payload, directory: selections))
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 1)
        XCTAssertTrue(model.savedDecisions["question"]?.responseJson?.contains("Option 5") == true)

        let another = try notificationOptionPayload(5)
        try PushQuestionSelection.save(2, payload: another, directory: selections)
        let locked = PushNotifications(read: { _ in nil }, write: { _, _ in throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) },
            registerForPush: {}, previewKey: { _ in self.notificationKey }, selectionDirectory: selections, reportActionFailure: { _ in })
        let retained = await locked.receiveForwardedSelection(another)
        XCTAssertTrue(retained)
        XCTAssertTrue(locked.routingError?.contains("could not be saved") == true)
        XCTAssertEqual(try PushQuestionSelection.read(another, directory: selections), 2)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 1, "A failed private receipt save must never send an answer")
    }

    @MainActor func testNotificationInstructionPreservesOriginalAcrossColdLaunchAndChangedRetries() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
        let payload = try notificationPayload("one", feedback: true), text = "  Inspect café.txt.\nKeep it unchanged.  "
        let path = "/api/v1/approvals/one/resolve"
        try enqueueNotificationLookup([notificationRequest("one", feedback: true)])
        MessageRecoveryURLProtocol.fail(path: path, error: .networkConnectionLost)
        await harness.push.receiveAction(payload, decision: "decline", message: text)
        XCTAssertTrue(harness.push.canRetryAction)
        let intent = try XCTUnwrap(model.savedDecisions["one"])
        let original = try XCTUnwrap(intent.responseJson)
        let decoded = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(original.utf8)) as? [String: String])
        XCTAssertEqual(decoded["message"], text)
        await harness.push.receiveAction(payload, decision: "decline", message: "A different instruction")
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 1, "A second text action cannot replace a saved instruction")
        // A direct in-app retry also cannot replace or silently discard signed text.
        let request = try JSONDecoder().decode(AttentionRequest.self, from: notificationRequest("one", feedback: true))
        let changed = await model.resolve(request, decision: "decline", responseJSON: "{\"decision\":\"decline\",\"message\":\"changed\"}")
        guard case .unconfirmed = changed else { return XCTFail("Changed reply must not be accepted") }
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 1)
        let restored = PushNotifications(read: { harness.storage.values[$0] }, write: { harness.storage.values[$1] = $0 }, registerForPush: {},
            previewKey: { _ in self.notificationKey }, reportActionFailure: { harness.storage.failures.append($0) })
        restored.attach(harness.library)
        try enqueueNotificationLookup([notificationRequest("one", feedback: true)])
        MessageRecoveryURLProtocol.enqueue(path: path)
        restored.retryActions()
        try await waitForRecoveryRequests(path: path, count: 2)
        for _ in 0..<50 where restored.canRetryAction { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(restored.canRetryAction)
        for body in MessageRecoveryURLProtocol.bodies(path: path) {
            let value = try XCTUnwrap(JSONSerialization.jsonObject(with: body) as? [String: Any])
            XCTAssertEqual(value["responseJson"] as? String, original)
            XCTAssertEqual(value["idempotencyKey"] as? String, intent.idempotencyKey)
        }
        for _ in 0..<10 where harness.storage.failures.isEmpty { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(harness.storage.failures.isEmpty)
        for failure in harness.storage.failures {
            XCTAssertEqual(failure, payload, "Recovery includes only the original encrypted payload, never the instruction")
            XCTAssertFalse(try String(decoding: JSONEncoder().encode(failure), as: UTF8.self).contains("Inspect"))
        }
    }

    @MainActor func testNotificationInstructionRejectsEmptyOversizedAndChangedCapability() async throws {
        for (text, decision, advertised, current) in [(" \n", "decline", true, true), (String(repeating: "é", count: 1001), "decline", true, true),
            ("Read the guide", "accept", true, true), ("Read the guide", "decline", false, true), ("Read the guide", "decline", true, false)] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
            try enqueueNotificationLookup([notificationRequest("one", feedback: current)])
            await harness.push.receiveAction(try notificationPayload("one", feedback: advertised), decision: decision, message: text)
            XCTAssertNotNil(harness.push.routingError)
            XCTAssertFalse(harness.push.canRetryAction)
            XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/approvals/one/resolve").isEmpty)
        }
    }

    @MainActor func testNotificationActionRejectsStaleOwnershipAndFailedFreshLookup() async throws {
        for condition in ["host", "nonce", "route", "missing", "lookup"] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
            if condition == "lookup" {
                MessageRecoveryURLProtocol.fail(path: "/api/v1/approvals", error: .notConnectedToInternet)
            }
            try enqueueNotificationLookup(condition == "missing" ? [] : [notificationRequest("one", nonce: condition == "nonce" ? "changed" : nil)],
                                          conversation: condition == "route" ? "another-chat" : "project-chat")
            await harness.push.receiveAction(try notificationPayload("one", host: condition == "host" ? "other-host" : "camera-unit-host"), decision: "accept")
            XCTAssertNotNil(harness.push.routingError, condition)
            XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/approvals/one/resolve").isEmpty, condition)
            XCTAssertEqual(harness.push.canRetryAction, condition == "lookup", condition)
            XCTAssertNil(model.savedDecisions["one"], condition)
        }
    }

    @MainActor func testNotificationActionsKeepConcurrentReceiptsSeparate() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
        for _ in 0..<4 { try enqueueNotificationLookup([notificationRequest("one"), notificationRequest("two", feedback: true)]) }
        for id in ["one", "two"] { MessageRecoveryURLProtocol.enqueue(path: "/api/v1/approvals/\(id)/resolve") }
        let first = Task { await harness.push.receiveAction(try notificationPayload("one"), decision: "accept") }
        let second = Task { await harness.push.receiveAction(try notificationPayload("two", feedback: true), decision: "decline", message: "Read the guide first.") }
        try await first.value; try await second.value
        XCTAssertFalse(harness.push.canRetryAction)
        XCTAssertTrue(harness.storage.failures.isEmpty)
        XCTAssertEqual(model.savedDecisions["one"]?.decision, "accept")
        XCTAssertEqual(model.savedDecisions["two"]?.decision, "decline")
        XCTAssertTrue(model.savedDecisions["two"]?.responseJson?.contains("Read the guide first.") == true)
        XCTAssertNil(model.savedDecisions["one"]?.responseJson)
        for id in ["one", "two"] { XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/approvals/\(id)/resolve").count, 1) }
    }

    @MainActor func testNotificationActionCancellationCannotSendAfterIdentityReplacement() async throws {
        for question in [false, true] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
            let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
            try enqueueNotificationLookup([question ? notificationQuestionRequest() : notificationRequest("one")])
            MessageRecoveryURLProtocol.hold(path: "/api/v1/approvals")
            let payload = try question ? notificationQuestionPayload() : notificationPayload("one")
            let action = Task { await harness.push.receiveAction(payload, decision: question ? "respond" : "accept", message: question ? "Friendly" : nil, timeout: .seconds(2)) }
            try await waitForRecoveryRequests(path: "/api/v1/approvals")
            model.connection = nil
            try await action.value
            MessageRecoveryURLProtocol.releaseHeld()
            try await Task.sleep(for: .milliseconds(30))
            XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: question ? "/api/v1/approvals/question/resolve" : "/api/v1/approvals/one/resolve").isEmpty)
            XCTAssertNotNil(harness.push.routingError)
        }
    }

    @MainActor func testNotificationActionPersistenceAndQueueLimitDoNotClaimSavedReply() async throws {
        let payload = try notificationPayload("one")
        let push = PushNotifications(read: { _ in nil }, write: { _, _ in throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) },
            registerForPush: {}, previewKey: { _ in self.notificationKey }, reportActionFailure: { _ in })
        await push.receiveAction(payload, decision: "accept")
        XCTAssertTrue(push.canRetryAction, "An explicit retry can try saving the in-memory receipt again")
        XCTAssertTrue(push.routingError?.contains("could not be saved") == true)
        var receipts: [String: Any] = [:]
        for index in 0..<32 { receipts[String(index)] = ["payload": payload, "decision": "accept"] }
        let data = try JSONSerialization.data(withJSONObject: receipts)
        let full = PushNotifications(read: { $0 == "push-actions-v1" ? data : nil }, write: { _, _ in XCTFail("A full queue must not replace a saved reply") },
            registerForPush: {}, previewKey: { _ in self.notificationKey }, reportActionFailure: { _ in })
        await full.receiveAction(payload, decision: "decline")
        XCTAssertTrue(full.routingError?.contains("Too many replies") == true)
    }

    @MainActor func testNotificationActionLockedIdentityAndConflictingRetryNeverSend() async throws {
        var failures = 0
        let push = PushNotifications(read: { _ in nil }, write: { _, _ in }, registerForPush: {},
                                     previewKey: { _ in throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) },
                                     reportActionFailure: { _ in failures += 1 })
        await push.receiveAction(try notificationPayload("one"), decision: "accept")
        for _ in 0..<10 where failures == 0 { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertEqual(failures, 1)
        XCTAssertFalse(push.canRetryAction)
        XCTAssertNotNil(push.routingError)
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
        try enqueueNotificationLookup([notificationRequest("one")])
        MessageRecoveryURLProtocol.fail(path: "/api/v1/approvals/one/resolve", error: .networkConnectionLost)
        let payload = try notificationPayload("one")
        await harness.push.receiveAction(payload, decision: "accept")
        await harness.push.receiveAction(payload, decision: "decline")
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/approvals/one/resolve").count, 1)
        XCTAssertEqual(model.savedDecisions["one"]?.decision, "accept")
        // The row and the refusal name the saved choice instead of a generic retry.
        let request = try JSONDecoder().decode(AttentionRequest.self, from: notificationRequest("one"))
        let saved = try XCTUnwrap(model.savedDecisions["one"])
        XCTAssertEqual(request.savedChoice(saved)?.title, "Allow once")
        let result = await model.resolve(request, decision: "decline")
        guard case .unconfirmed = result else { return XCTFail("A different reply must not replace an uncertain Allow") }
        XCTAssertEqual(model.attentionErrors["one"], request.savedReplyConflict(saved))
        XCTAssertTrue(model.attentionErrors["one"]?.contains("You already chose “Allow once”") == true)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/approvals/one/resolve").count, 1)
        XCTAssertEqual(model.savedDecisions["one"]?.decision, "accept")
    }

    @MainActor func testPushSetupStatusAndOwnershipSurvivePersistence() throws {
        let saved = Self.cameraSavedConnection()
        var record = PushRegistration(host: saved.credential.hostInstallationId, device: saved.credential.deviceId, endpoint: "https://push.example.test", key: Data(repeating: 1, count: 32))
        XCTAssertFalse(record.macRegistered)
        XCTAssertTrue(record.belongs(to: saved.credential))
        record.macRegistered = true
        XCTAssertTrue(record.macRegistered)
        record.enabled = false
        let restored = try JSONDecoder().decode(PushRegistration.self, from: JSONEncoder().encode(record))
        XCTAssertFalse(restored.enabled)
        let differentDevice = PushRegistration(host: saved.credential.hostInstallationId, device: "repaired-device", endpoint: record.endpoint, key: record.key)
        XCTAssertFalse(differentDevice.belongs(to: saved.credential))
    }

    /// Preference, enrollment and reply keys form one authoritative snapshot. A partial read must not
    /// authorize cleanup, migration, or replacement of an existing reply queue.
    @MainActor func testPushLockedLoadPreservesSnapshotAndRetriesWithoutRestart() async throws {
        for failedKey in [PushNotifications.preferenceKey, "push-registrations-v1", "push-requested-devices-v1", "push-actions-v1", "push-receipt-v1"] {
            MessageRecoveryURLProtocol.reset()
            let saved = Self.cameraSavedConnection(), host = saved.credential.hostInstallationId
            let library = try pushStorageLibrary(saved)
            var record = PushRegistration(host: host, device: saved.credential.deviceId, endpoint: "https://push.example.test", key: Data(repeating: 1, count: 32))
            record.id = notificationRegistration; record.token = "0102"; record.macRegistered = true
            record.previewVersion = 1; record.registeredAt = Date()
            let payload = try notificationPayload("one")
            let actionID = notificationRegistration + ":one:" + notificationNonce
            var values = [
                "push-registrations-v1": try JSONEncoder().encode([host: record]),
                "push-requested-devices-v1": try JSONEncoder().encode([host: record.device, "unrelated": "other-device"]),
                "push-actions-v1": try JSONSerialization.data(withJSONObject: [actionID: ["payload": payload, "decision": "accept"]]),
                "push-receipt-v1": try JSONSerialization.data(withJSONObject: ["registrationId": notificationRegistration, "routeId": notificationRoute])
            ]
            let original = values
            var locked = true, reads = 0, writes = 0
            let push = PushNotifications(read: { key in
                reads += 1
                if locked && key == failedKey { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
                return values[key]
            }, write: { values[$1] = $0; writes += 1 }, authorize: { XCTFail("Saved enrollment must not be replaced"); return false },
                registerForPush: {}, accountService: { "https://push.example.test" }, previewKey: { _ in self.notificationKey }, reportActionFailure: { _ in })
            push.attach(library)
            push.receivedToken(Data([1, 2])); push.registrationFailed(URLError(.notConnectedToInternet)); push.refresh()
            await push.receiveAction(try notificationPayload("two"), decision: "decline", timeout: .milliseconds(30))
            push.retryActions(); push.retryOpening()
            XCTAssertEqual(values, original, failedKey)
            XCTAssertEqual(writes, 0, "No default or partial view may be saved: " + failedKey)
            XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/approvals/two/resolve").isEmpty)

            locked = false
            push.receivedToken(Data([1, 2]))
            XCTAssertTrue(push.isEnabled(host), failedKey)
            XCTAssertEqual(push.registrationID(host: host), notificationRegistration)
            XCTAssertTrue(push.canRetryAction, "The original saved reply is restored")
            XCTAssertTrue(push.canRetryOpening, "The saved tap is restored")
            let loadedReads = reads
            push.refresh(); push.receivedToken(Data([1, 2])); push.refresh()
            XCTAssertEqual(reads, loadedReads, "Successful loading happens only once")
            XCTAssertEqual(values["push-actions-v1"], original["push-actions-v1"])
            XCTAssertEqual(values["push-requested-devices-v1"], original["push-requested-devices-v1"])
            push.setForeground(false)
        }
    }

    @MainActor private func pushStorageLibrary(_ saved: SavedConnection) throws -> ConnectionLibrary {
        try pushStorageLibrary([saved])
    }

    @MainActor private func pushStorageLibrary(_ saved: [SavedConnection]) throws -> ConnectionLibrary {
        var connections = SavedConnections()
        for connection in saved { connections.save(connection) }
        let data = try JSONEncoder().encode(connections)
        // diagnosticModels is a preview and deliberately skips push refresh.
        // Use a loaded, non-preview library to exercise real cleanup callbacks.
        return ConnectionLibrary(storage: .init(read: { $0 == "connections-v1" ? data : nil }, save: { _, _ in }, forgetLegacy: {}))
    }

    @MainActor func testPushMalformedLoadDefersNewIntentWithoutReplacingOtherRecords() throws {
        for failedKey in [PushNotifications.preferenceKey, "push-registrations-v1", "push-requested-devices-v1", "push-actions-v1", "push-receipt-v1"] {
            let saved = Self.cameraSavedConnection(), host = saved.credential.hostInstallationId
            let library = try pushStorageLibrary(saved), model = library.model(for: saved)
            var record = PushRegistration(host: host, device: saved.credential.deviceId, endpoint: "https://push.example.test", key: Data(repeating: 1, count: 32))
            record.id = notificationRegistration
            var other = PushRegistration(host: "unrelated", device: "other-device", endpoint: record.endpoint, key: Data(repeating: 2, count: 32))
            other.id = UUID().uuidString; other.enabled = false; other.revocationCompleted = true; other.senderSecret = "keep-this-capability"
            var values = [
                "push-registrations-v1": try JSONEncoder().encode([host: record, other.host: other]),
                "push-requested-devices-v1": try JSONEncoder().encode([host: record.device, other.host: other.device]),
                "push-actions-v1": try JSONSerialization.data(withJSONObject: ["saved": ["payload": try notificationPayload("one"), "decision": "accept"]]),
                "push-receipt-v1": try JSONSerialization.data(withJSONObject: ["registrationId": notificationRegistration, "routeId": notificationRoute])
            ]
            let original = values, newRoute = UUID().uuidString
            var malformed = true, writable = false
            let push = PushNotifications(read: { key in malformed && key == failedKey ? Data("malformed".utf8) : values[key] }, write: {
                guard writable else { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
                values[$1] = $0
            }, authorize: { throw CancellationError() }, registerForPush: {}, accountService: { "https://push.example.test" })
            push.attach(library); push.disable(model)
            push.receiveTap(registration: notificationRegistration, route: newRoute)
            let dismissed = failedKey == "push-receipt-v1"
            if dismissed { push.dismissRoutingError() }
            push.refresh(); push.retryOpening()
            XCTAssertEqual(values, original, "Malformed data must never be treated as missing: " + failedKey)
            malformed = false
            push.refresh()
            XCTAssertEqual(values, original, "A deferred preference must still wait for a successful write")
            writable = true
            push.refresh(); push.refresh()
            XCTAssertFalse(push.isEnabled(host))
            XCTAssertNil(push.settingsAlert, "Recovery clears the obsolete locked-storage warning")
            XCTAssertFalse(try JSONDecoder().decode(Bool.self, from: XCTUnwrap(values[PushNotifications.preferenceKey])))
            XCTAssertEqual(values["push-requested-devices-v1"], original["push-requested-devices-v1"], "The migration input is no longer rewritten")
            let records = try JSONDecoder().decode([String: PushRegistration].self, from: XCTUnwrap(values["push-registrations-v1"]))
            XCTAssertEqual(records.count, 2); XCTAssertEqual(records[host]?.enabled, false)
            XCTAssertEqual(records[other.host]?.id, other.id); XCTAssertEqual(records[other.host]?.key, other.key)
            XCTAssertEqual(records[other.host]?.senderSecret, other.senderSecret)
            XCTAssertEqual(values["push-actions-v1"], original["push-actions-v1"])
            if dismissed { XCTAssertEqual(values["push-receipt-v1"], Data()) }
            else {
                let receipt = try JSONDecoder().decode([String: String].self, from: XCTUnwrap(values["push-receipt-v1"]))
                XCTAssertEqual(receipt["routeId"], newRoute, "A newer tap wins after the saved snapshot loads")
            }
            push.setForeground(false)
        }
    }

    @MainActor func testPushLockedLoadHonorsAccountEndAndNewPairing() throws {
        for enableReplacement in [false, true] {
            let saved = Self.cameraSavedConnection(), host = saved.credential.hostInstallationId
            var library = try pushStorageLibrary(saved)
            var record = PushRegistration(host: host, device: saved.credential.deviceId, endpoint: "https://push.example.test", key: Data(repeating: 1, count: 32))
            record.id = notificationRegistration
            var other = PushRegistration(host: "unrelated", device: "other-device", endpoint: record.endpoint, key: Data(repeating: 2, count: 32))
            other.enabled = false; other.revocationCompleted = true; other.id = UUID().uuidString
            var values = ["push-registrations-v1": try JSONEncoder().encode([host: record, other.host: other]),
                          "push-requested-devices-v1": try JSONEncoder().encode([host: record.device, other.host: other.device])]
            let original = values
            var locked = true, signedIn = true
            let push = PushNotifications(read: {
                if locked { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
                return values[$0]
            }, write: { values[$1] = $0 }, authorize: { throw CancellationError() }, registerForPush: {},
                accountService: { signedIn ? "https://push.example.test" : nil })
            push.attach(library)
            signedIn = false; push.accountEnded()
            signedIn = true
            if enableReplacement {
                var fields = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(saved)) as? [String: Any])
                var credential = try XCTUnwrap(fields["credential"] as? [String: Any])
                credential["deviceId"] = "replacement-device"; fields["credential"] = credential
                let replacement = try JSONDecoder().decode(SavedConnection.self, from: JSONSerialization.data(withJSONObject: fields))
                library = try pushStorageLibrary(replacement)
                push.attach(library); push.enable(library.model(for: replacement))
            }
            XCTAssertEqual(values, original, "Sign-out and re-pair must wait for the authoritative snapshot")
            locked = false
            push.receivedToken(Data([1, 2])); push.refresh()
            XCTAssertEqual(try JSONDecoder().decode(Bool.self, from: XCTUnwrap(values[PushNotifications.preferenceKey])), enableReplacement,
                           "Sign-out survives unlock unless the owner subsequently enables the app-wide preference")
            XCTAssertEqual(values["push-requested-devices-v1"], original["push-requested-devices-v1"])
            XCTAssertEqual(push.isEnabled(host), enableReplacement)
            let records = try JSONDecoder().decode([String: PushRegistration].self, from: XCTUnwrap(values["push-registrations-v1"]))
            XCTAssertEqual(records[host]?.device, saved.credential.deviceId)
            XCTAssertEqual(records[host]?.id, notificationRegistration)
            XCTAssertEqual(records[host]?.enabled, false, "The old capability must be retired before a replacement can enroll")
            XCTAssertEqual(records[other.host]?.id, other.id)
            push.setForeground(false)
        }
    }

    @MainActor func testPushLockedActionLoadPreservesOriginalReplyAndDeduplicatesRetry() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
        let payload = try notificationPayload("one"), path = "/api/v1/approvals/one/resolve"
        try enqueueNotificationLookup([notificationRequest("one")])
        MessageRecoveryURLProtocol.fail(path: path, error: .networkConnectionLost)
        await harness.push.receiveAction(payload, decision: "accept")
        let original = try XCTUnwrap(harness.storage.values["push-actions-v1"])
        var locked = true
        let push = PushNotifications(read: {
            if locked && $0 == "push-actions-v1" { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
            return harness.storage.values[$0]
        }, write: { harness.storage.values[$1] = $0 }, authorize: { true }, registerForPush: {},
            accountService: { "https://push.example.test" }, previewKey: { _ in self.notificationKey }, reportActionFailure: { _ in })
        push.attach(harness.library)
        await push.receiveAction(payload, decision: "decline")
        XCTAssertEqual(harness.storage.values["push-actions-v1"], original)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 1)
        locked = false
        push.refresh()
        XCTAssertTrue(push.canRetryAction)
        await push.receiveAction(payload, decision: "decline")
        XCTAssertEqual(harness.storage.values["push-actions-v1"], original, "Recovery must not replace an unconfirmed answer")
        try enqueueNotificationLookup([notificationRequest("one")])
        MessageRecoveryURLProtocol.fail(path: path, error: .networkConnectionLost)
        let first = Task { await push.receiveAction(payload, decision: "accept") }
        let duplicate = Task { await push.receiveAction(payload, decision: "accept") }
        await first.value; await duplicate.value
        XCTAssertTrue(push.canRetryAction)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 2, "Concurrent callbacks share one in-flight reply")
        try enqueueNotificationLookup([notificationRequest("one")])
        MessageRecoveryURLProtocol.enqueue(path: path)
        await push.receiveAction(payload, decision: "accept")
        XCTAssertFalse(push.canRetryAction)
        push.refresh()
        XCTAssertFalse(push.canRetryAction, "Repeated refresh must not resurrect a cleared receipt")
        let bodies = try MessageRecoveryURLProtocol.bodies(path: path).map { try XCTUnwrap(JSONSerialization.jsonObject(with: $0) as? [String: Any]) }
        XCTAssertEqual(bodies.count, 3)
        let intent = try XCTUnwrap(model.savedDecisions["one"])
        for body in bodies {
            XCTAssertEqual(body["idempotencyKey"] as? String, intent.idempotencyKey)
            XCTAssertEqual(body["decision"] as? String, "accept")
        }
    }

    @MainActor func testPushLockedLoadAppliesOffIntentBeforeActionRetry() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }, harness = try notificationHarness(model)
        let saved = Self.cameraSavedConnection(), host = saved.credential.hostInstallationId
        var record = try JSONDecoder().decode([String: PushRegistration].self, from: XCTUnwrap(harness.storage.values["push-registrations-v1"]))
        record[host]?.macRegistered = true; record[host]?.token = "0102"
        harness.storage.values["push-registrations-v1"] = try JSONEncoder().encode(record)
        var locked = true
        let push = PushNotifications(read: {
            if locked { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
            return harness.storage.values[$0]
        }, write: { harness.storage.values[$1] = $0 }, registerForPush: {}, accountService: { "https://push.example.test" },
            previewKey: { _ in self.notificationKey }, reportActionFailure: { _ in })
        push.attach(harness.library); push.receivedToken(Data([1, 2])); push.disable(model)
        locked = false
        // This callback loads the snapshot without the normal refresh cleanup.
        await push.receiveAction(try notificationPayload("one"), decision: "accept")
        XCTAssertFalse(push.isEnabled(host)); XCTAssertFalse(push.canFollowLiveActivity(host))
        XCTAssertNil(push.registrationID(host: host))
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/approvals/one/resolve").isEmpty)
        do {
            _ = try await push.liveActivityToken(host: host, kind: "update", token: "synthetic")
            XCTFail("A deferred off preference must prevent ActivityKit registration too")
        } catch { /* No request is allowed before refresh retires the record. */ }
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").isEmpty)
        XCTAssertNil(push.settingsAlert)
    }

    /// Records account service calls; `statuses` answers by path (default 200).
    private final class AccountService: @unchecked Sendable {
        var calls: [(path: String, body: [String: Any], bearer: String?)] = []
        var statuses: [String: Int] = [:]
        var values: [String: Data] = [:]
        let tokens = Data(#"{"sessionId":"00000000-0000-4000-8000-0000000000aa","accessToken":"access-one","refreshToken":"refresh-one","accessExpiresIn":3600,"refreshExpiresIn":7776000}"#.utf8)
        @MainActor func account() -> WonderAccount {
            WonderAccount(service: "https://push.example.test", read: { self.values[$0] }, write: { self.values[$1] = $0 }, remove: { self.values[$0] = nil },
                          post: { path, body, bearer in
                self.calls.append((path, (try? JSONSerialization.jsonObject(with: body) as? [String: Any]) ?? [:], bearer))
                if let status = self.statuses[path], status != 200 { throw PairingFailure.response(status) }
                return path.hasPrefix("/v1/auth/apple") || path == "/v1/auth/refresh" ? self.tokens : Data("{}".utf8)
            })
        }
    }
    @MainActor private func signIn(_ account: WonderAccount) async {
        let request = ASAuthorizationAppleIDProvider().createRequest()
        account.prepare(request)
        XCTAssertEqual(request.requestedScopes ?? [], [], "Cupola asks Apple for neither name nor email")
        XCTAssertEqual(request.nonce?.count, 64, "Apple receives only the SHA-256 of the nonce")
        await account.signIn(identityToken: "header.claims.signature")
    }

    @MainActor func testAccountSignInBindsTheSessionToThisDeviceAndSurvivesRelaunch() async throws {
        let service = AccountService(), account = service.account()
        account.ended = {}
        XCTAssertFalse(account.isSignedIn)
        await signIn(account)
        XCTAssertTrue(account.isSignedIn); XCTAssertNil(account.error)
        let call = try XCTUnwrap(service.calls.first)
        XCTAssertEqual(call.path, "/v1/auth/apple")
        XCTAssertEqual(call.body["identityToken"] as? String, "header.claims.signature")
        let nonce = try XCTUnwrap(call.body["nonce"] as? String), deviceKey = try XCTUnwrap(call.body["deviceKey"] as? String)
        let key = try P256.Signing.PublicKey(x963Representation: XCTUnwrap(Data(base64URL: deviceKey)))
        let signature = try P256.Signing.ECDSASignature(rawRepresentation: XCTUnwrap(Data(base64URL: XCTUnwrap(call.body["signature"] as? String))))
        XCTAssertTrue(key.isValidSignature(signature, for: Data("signin|apple|\(nonce)".utf8)))
        let current = try await account.accessToken()
        XCTAssertEqual(current, "access-one")
        let relaunched = service.account()
        XCTAssertTrue(relaunched.isSignedIn, "The session is kept in this device's Keychain")
        // Refresh is signed by the same device key.
        service.values["account-session-v1"] = try JSONEncoder().encode(AccountSession(sessionId: "00000000-0000-4000-8000-0000000000aa", accessToken: "old", accessExpiry: Date(),
            refreshToken: "refresh-one", refreshExpiry: Date().addingTimeInterval(86400), signedInAt: Date()))
        let expiring = service.account()
        let renewed = try await expiring.accessToken()
        XCTAssertEqual(renewed, "access-one")
        let refresh = try XCTUnwrap(service.calls.last)
        XCTAssertEqual(refresh.path, "/v1/auth/refresh")
        let refreshSignature = try P256.Signing.ECDSASignature(rawRepresentation: XCTUnwrap(Data(base64URL: XCTUnwrap(refresh.body["signature"] as? String))))
        XCTAssertTrue(key.isValidSignature(refreshSignature, for: Data("refresh|00000000-0000-4000-8000-0000000000aa|\(try XCTUnwrap(refresh.body["timestamp"] as? Int))".utf8)))
    }

    @MainActor func testAccountDeleteAndSignOutTurnNotificationsOffAndEndTheSession() async throws {
        let service = AccountService(), account = service.account()
        var ended = 0
        account.ended = { ended += 1 }
        await signIn(account)
        service.statuses["/v1/account/delete"] = 503
        let failed = await account.deleteAccount()
        XCTAssertFalse(failed); XCTAssertTrue(account.isSignedIn, "An unconfirmed deletion keeps the account")
        XCTAssertNotNil(account.error); XCTAssertEqual(ended, 0)
        service.statuses["/v1/account/delete"] = nil
        let deleted = await account.deleteAccount()
        XCTAssertTrue(deleted); XCTAssertFalse(account.isSignedIn); XCTAssertEqual(ended, 1)
        XCTAssertEqual(service.calls.last?.path, "/v1/account/delete"); XCTAssertEqual(service.calls.last?.bearer, "access-one")
        XCTAssertNil(service.values["account-session-v1"])
        await signIn(account)
        service.statuses["/v1/auth/signout"] = 503
        await account.signOut()
        XCTAssertFalse(account.isSignedIn, "Sign-out ends the session on this iPhone even offline"); XCTAssertEqual(ended, 2)
        XCTAssertEqual(service.calls.last?.path, "/v1/auth/signout")
    }

    /// Each push-setup failure names its own cause and recovery.
    func testPushSetupFailuresNameTheirCause() {
        let table: [(Error, PushFailure.Stage, PushFailure)] = [
            (URLError(.notConnectedToInternet), .challenge, .serviceUnreachable),
            (PairingFailure.response(400), .challenge, .appleRejectedDevice),
            (PairingFailure.response(410), .challenge, .appleRejectedDevice),
            (PairingFailure.response(429), .challenge, .rateLimited),
            (PairingFailure.response(503), .challenge, .serviceDown),
            (PairingFailure.response(403), .challenge, .serviceRefused),
            (PairingFailure.response(403), .proof, .setupExpired),
            (PairingFailure.response(400), .proof, .serviceRefused),
            (URLError(.timedOut), .computer, .unavailable),
            (PairingFailure.response(404), .computer, .computerAccess),
            (PairingFailure.response(500), .computer, .computerFailed)
        ]
        for (error, stage, expected) in table {
            XCTAssertEqual(PushFailure.classify(error, stage) as? PushFailure, expected, "\(error) at \(stage)")
        }
        XCTAssertTrue(PushFailure.classify(CancellationError(), .challenge) is CancellationError)
        XCTAssertTrue(PushFailure.classify(AccountFailure.signedOut, .proof) is AccountFailure)
        XCTAssertEqual(Set(table.map { $0.2.errorDescription }).count, Set(table.map(\.2)).count, "Causes read differently")
        XCTAssertTrue(PushFailure.registrationMessage(NSError(domain: NSCocoaErrorDomain, code: 3010)).contains("Simulator"))
        XCTAssertTrue(PushFailure.registrationMessage(NSError(domain: NSCocoaErrorDomain, code: 3000)).contains("TestFlight"))
    }

    @MainActor func testPushChallengeTimeoutIgnoresStaleProof() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = recoveryModel(root: root), saved = Self.cameraSavedConnection(), host = saved.credential.hostInstallationId
        let library = ConnectionLibrary(diagnosticModels: [model])
        var record = PushRegistration(host: host, device: saved.credential.deviceId,
                                      endpoint: "https://push.example.test", key: Data(repeating: 1, count: 32))
        record.nonce = "current"
        var values = ["push-registrations-v1": try JSONEncoder().encode([host: record])]
        let push = PushNotifications(read: { values[$0] }, write: { values[$1] = $0 },
                                     authorize: { true }, registerForPush: {})
        push.attach(library)
        push.enable(model)
        push.challengeTimedOut(host, nonce: "stale")
        XCTAssertEqual(push.setupState(host), .settingUp)
        push.challengeTimedOut(host, nonce: "current")
        guard case .some(.needsRetry(let message)) = push.setupState(host) else {
            return XCTFail("An unanswered current challenge should offer retry")
        }
        XCTAssertTrue(message.contains("Apple"))
        let pending = try JSONDecoder().decode([String: PushRegistration].self,
                                               from: XCTUnwrap(values["push-registrations-v1"]))
        XCTAssertEqual(pending[host]?.nonce, "current", "A late APNs proof must remain eligible after the warning")
        push.disable(model)
    }

    /// Owner feedback 34: a setup failure on a computer that is not confirmed
    /// connected stays a retry state in Settings and raises no alert.
    @MainActor func testSetupFailureOnUnconfirmedComputerRaisesNoAlert() async throws {
        for connected in [false, nil, true] as [Bool?] {
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let model = recoveryModel(root: root), host = Self.cameraSavedConnection().credential.hostInstallationId
            model.macConnected = connected
            let library = ConnectionLibrary(diagnosticModels: [model])
            var values: [String: Data] = [:]
            let push = PushNotifications(read: { values[$0] }, write: { values[$1] = $0 },
                                         authorize: { throw SigningIdentityFailure.missing }, registerForPush: {},
                                         accountService: { "https://push.example.test" })
            defer { push.disable(model) }
            push.attach(library); push.enable(model)
            for _ in 0..<100 { if case .some(.needsRetry) = push.setupState(host) { break }; try await Task.sleep(for: .milliseconds(10)) }
            guard case .some(.needsRetry) = push.setupState(host) else { return XCTFail("Setup failure keeps a retry state, connected: \(String(describing: connected))") }
            XCTAssertEqual(push.settingsAlert == nil, connected != true, "connected: \(String(describing: connected))")
        }
    }

    @MainActor func testPushOffCancelsDelayedPermissionWithoutReenabling() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = recoveryModel(root: root), host = Self.cameraSavedConnection().credential.hostInstallationId
        let library = ConnectionLibrary(diagnosticModels: [model])
        var values: [String: Data] = [:]
        var permission: CheckedContinuation<Bool, Error>?
        let push = PushNotifications(read: { values[$0] }, write: { values[$1] = $0 }, authorize: {
            try await withCheckedThrowingContinuation { permission = $0 }
        }, registerForPush: { XCTFail("A cancelled preference must never enroll") })
        push.attach(library); push.enable(model)
        for _ in 0..<100 { if permission != nil { break }; try await Task.sleep(for: .milliseconds(10)) }
        let pending = try XCTUnwrap(permission)
        push.disable(model)
        pending.resume(returning: true)
        try await Task.sleep(for: .milliseconds(30))
        XCTAssertFalse(push.isEnabled(host))
        XCTAssertNil(push.settingsAlert)
        XCTAssertNil(values["push-registrations-v1"])
    }

    @MainActor func testPushPreferenceWriteFailureDoesNotPretendItSaved() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = recoveryModel(root: root), host = Self.cameraSavedConnection().credential.hostInstallationId
        let library = ConnectionLibrary(diagnosticModels: [model])
        let push = PushNotifications(read: { _ in nil }, write: { _, _ in throw CocoaError(.fileWriteNoPermission) }, authorize: { XCTFail("An unsaved preference must not enroll"); return true }, registerForPush: {})
        push.attach(library); push.enable(model)
        XCTAssertFalse(push.isEnabled(host))
        XCTAssertEqual(push.settingsAlert?.title, "Couldn’t change notifications")
    }

    @MainActor func testPushPreferenceMigrationPreservesOwnership() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = recoveryModel(root: root), saved = Self.cameraSavedConnection()
        let library = ConnectionLibrary(diagnosticModels: [model])
        let record = PushRegistration(host: saved.credential.hostInstallationId, device: saved.credential.deviceId, endpoint: "https://push.example.test", key: Data(repeating: 1, count: 32))
        let data = try JSONEncoder().encode([record.host: record])
        let migrated = PushNotifications(read: { $0 == "push-registrations-v1" ? data : nil }, write: { _, _ in }, registerForPush: {})
        migrated.attach(library)
        XCTAssertTrue(migrated.isEnabled(record.host))
        let wrongDevice = try JSONEncoder().encode([record.host: "old-device"])
        let repaired = PushNotifications(read: { $0 == "push-requested-devices-v1" ? wrongDevice : nil }, write: { _, _ in }, registerForPush: {})
        repaired.attach(library)
        XCTAssertFalse(repaired.isEnabled(record.host), "A stale pairing was not On and must not enable every computer")
        XCTAssertFalse(repaired.canFollowLiveActivity(record.host), "Intent cannot grant the old device's delivery authority")
    }

    /// Contract: one saved switch enrolls all current pairings, survives a
    /// relaunch, and supplies intent for a pairing created later. The old
    /// host list must never override an explicit app-wide Off on relaunch.
    @MainActor func testPushAppWideMigrationAndLaterPairing() async throws {
        for legacyOn in [false, true] {
            let first = Self.cameraSavedConnection(hostID: "first")
            let second = Self.cameraSavedConnection(hostID: "second")
            let later = Self.cameraSavedConnection(hostID: "later")
            let library = try pushStorageLibrary([first, second])
            var values = ["push-requested-devices-v1": try JSONEncoder().encode(legacyOn ? ["first": first.credential.deviceId] : [:])]
            var attempts = 0, preferenceWrites = 0
            let push = PushNotifications(read: { values[$0] }, write: {
                values[$1] = $0
                if $1 == PushNotifications.preferenceKey { preferenceWrites += 1 }
            }, authorize: { attempts += 1; throw CancellationError() }, registerForPush: {},
                accountService: { "https://push.example.test" })
            defer { push.setForeground(false) }
            push.attach(library)
            XCTAssertEqual(push.enabled, legacyOn)
            XCTAssertEqual(push.isEnabled("first"), legacyOn)
            XCTAssertEqual(push.isEnabled("second"), legacyOn, "Any old On enrolls the rest")
            XCTAssertEqual(preferenceWrites, 1, "Migration persists once")
            push.setAll(enabled: true, library: library)
            for _ in 0..<100 { if attempts == 2 { break }; try await Task.sleep(for: .milliseconds(10)) }
            XCTAssertEqual(attempts, 2, "Both current computers enter enrollment")
            try library.savePairing(later)
            push.refresh()
            XCTAssertTrue(push.isEnabled("later"))
            for _ in 0..<100 { if attempts == 3 { break }; try await Task.sleep(for: .milliseconds(10)) }
            XCTAssertEqual(attempts, 3, "Pairing refresh enrolls the new computer without another toggle")
            push.setAll(enabled: false, library: library)
            XCTAssertFalse(push.enabled)
            XCTAssertTrue(library.paired.allSatisfy { !push.isEnabled($0.credential.hostInstallationId) })
            let savedWrites = preferenceWrites
            let restored = PushNotifications(read: { values[$0] }, write: {
                values[$1] = $0
                if $1 == PushNotifications.preferenceKey { preferenceWrites += 1 }
            }, authorize: { XCTFail("Saved Off cannot enroll"); throw CancellationError() }, registerForPush: {},
                accountService: { "https://push.example.test" })
            restored.attach(library)
            defer { restored.setForeground(false) }
            XCTAssertFalse(restored.enabled)
            XCTAssertFalse(restored.isEnabled("later"))
            XCTAssertEqual(preferenceWrites, savedWrites, "An existing preference is never migrated again")
        }
        let empty = try pushStorageLibrary([])
        var values: [String: Data] = [:]
        let push = PushNotifications(read: { values[$0] }, write: { values[$1] = $0 }, authorize: { throw CancellationError() },
                                     registerForPush: {}, accountService: { "https://push.example.test" })
        defer { push.setForeground(false) }
        push.attach(empty)
        XCTAssertFalse(push.enabled)
        push.setAll(enabled: true, library: empty)
        try empty.savePairing(Self.cameraSavedConnection(hostID: "later"))
        push.refresh()
        XCTAssertTrue(push.isEnabled("later"), "On can be stored before the first computer is paired")
    }

    @MainActor func testPushAppWideOffWhilePairingsAreLockedStillRetiresSavedCapabilities() throws {
        let saved = Self.cameraSavedConnection()
        let host = saved.credential.hostInstallationId
        let record = PushRegistration(host: host, device: saved.credential.deviceId, endpoint: "https://push.example.test", key: Data(repeating: 1, count: 32))
        var values = ["push-registrations-v1": try JSONEncoder().encode([host: record]),
                      PushNotifications.preferenceKey: try JSONEncoder().encode(true)]
        let library = ConnectionLibrary(storage: .init(read: { _ in throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }, save: { _, _ in }, forgetLegacy: {}))
        let push = PushNotifications(read: { values[$0] }, write: { values[$1] = $0 }, registerForPush: {},
                                     accountService: { "https://push.example.test" })
        push.setAll(enabled: false, library: library)
        XCTAssertFalse(push.enabled)
        let records = try JSONDecoder().decode([String: PushRegistration].self, from: XCTUnwrap(values["push-registrations-v1"]))
        XCTAssertEqual(records[host]?.enabled, false)
    }

    @MainActor func testCacheBackgroundGrantEndsSynchronouslyOnExpirationAndExactlyOnce() {
        var expiration: (@MainActor @Sendable () -> Void)?
        var ended: [UIBackgroundTaskIdentifier] = []
        let identifier = UIBackgroundTaskIdentifier(rawValue: 42)
        let grant = CacheBackgroundTask(begin: { expiration = $0; return identifier }, end: { ended.append($0) })
        XCTAssertTrue(ended.isEmpty)
        expiration?()
        XCTAssertEqual(ended, [identifier], "UIKit requires completion before expiration returns, without an actor hop")
        grant.finish(); expiration?()
        XCTAssertEqual(ended, [identifier])
        let completed = CacheBackgroundTask(begin: { expiration = $0; return identifier }, end: { ended.append($0) })
        completed.finish(); expiration?(); completed.finish()
        XCTAssertEqual(ended, [identifier, identifier], "Normal completion and late expiration end only once")
        let cancelled = CacheBackgroundTask(begin: { expiration = $0; return identifier }, end: { ended.append($0) })
        cancelled.finish(); expiration?()
        XCTAssertEqual(ended.count, 3, "Foreground/cancellation uses the same idempotent completion")
        let denied = CacheBackgroundTask(begin: { expiration = $0; return .invalid }, end: { _ in XCTFail("Invalid grants must not be ended") })
        denied.finish(); expiration?()
        let immediate = CacheBackgroundTask(begin: { expire in expire(); return identifier }, end: { ended.append($0) })
        XCTAssertEqual(ended.count, 4)
        immediate.finish()
        XCTAssertEqual(ended.count, 4)
    }

    @MainActor func testBackgroundFlushKeepsLatestProjectionUnverifiedWithoutRewritingUnchangedBody() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        let saved = Self.cameraSavedConnection()
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        let path = "/api/v1/conversations/\(chat.id)"
        let body = String(repeating: "Saved conversation body. ", count: 8_000)
        let data = try JSONSerialization.data(withJSONObject: [
            "conversationId": chat.id, "hostEpoch": "epoch", "lastSequence": 7,
            "messages": [["messageId": "m", "body": body, "state": "completed", "createdAt": "1700000000000", "attachmentIds": []]],
            "assistantMessages": [], "thread": ["hydrated": true]
        ])
        MessageRecoveryURLProtocol.enqueue(path: path, body: data)
        await model.refreshConversation(chat)
        XCTAssertFalse(model.cachedConversationIds.contains(chat.id))
        model.setForeground(false)
        let persisted = try await ProjectionWriter.load(store)
        XCTAssertTrue(persisted.snapshots.isEmpty, "Launch loads metadata without decoding history")
        let persistedRecord = try XCTUnwrap(persisted.bodyRecords?[chat.id])
        let savedBody = try await ProjectionWriter.loadConversation(store, conversation: chat.id, record: persistedRecord)
        XCTAssertEqual(savedBody.messages.first?.body, body, "Newest accepted projection must flush before completion")
        XCTAssertEqual(persisted.dirty, [chat.id])
        XCTAssertTrue(model.cachedConversationIds.contains(chat.id))
        XCTAssertNil(model.macConnected)
        let checkpoint = store.directory.appendingPathComponent("history-" + persistedRecord.bodyID.uuidString + ".json")
        let original = try Data(contentsOf: checkpoint)
        let inode = try FileManager.default.attributesOfItem(atPath: checkpoint.path)[.systemFileNumber] as? NSNumber
        model.setForeground(false)
        _ = try await ProjectionWriter.load(store)
        XCTAssertEqual(try Data(contentsOf: checkpoint), original)
        XCTAssertEqual(try FileManager.default.attributesOfItem(atPath: checkpoint.path)[.systemFileNumber] as? NSNumber, inode)
        let index = store.directory.appendingPathComponent("read-cache-v4.json")
        XCTAssertLessThan(try Data(contentsOf: index).count, 8_192)
        // Foreground alone cannot make suspended data authoritative. The
        // intercepted offline check fails, then a fresh snapshot clears it.
        model.setForeground(true)
        await model.check()
        XCTAssertTrue(model.cachedConversationIds.contains(chat.id))
        MessageRecoveryURLProtocol.enqueue(path: path, body: data)
        await model.refreshConversation(chat)
        XCTAssertFalse(model.cachedConversationIds.contains(chat.id))
        model.retireAfterPairingReplacement()
        _ = try await ProjectionWriter.load(store)
    }

    @MainActor func testBackgroundCacheWriteFailureIsReportedAndForgetCannotResurrectCache() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        let saved = Self.cameraSavedConnection()
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/\(chat.id)", body:
            Data(#"{"conversationId":"recovery-chat","hostEpoch":"epoch","lastSequence":7,"messages":[],"assistantMessages":[],"thread":{"hydrated":true}}"#.utf8))
        await model.refreshConversation(chat)
        model.setForeground(false)
        _ = try await ProjectionWriter.load(store)
        let overlay = store.directory.appendingPathComponent("read-cache-v4.json")
        try FileManager.default.removeItem(at: overlay)
        try FileManager.default.createDirectory(at: overlay, withIntermediateDirectories: false)
        model.setForeground(false)
        do { _ = try await ProjectionWriter.load(store) } catch { /* Actual cache failure is explicit. */ }
        for _ in 0..<100 where model.storageFailure != .chatsNotSaved { await Task.yield() }
        XCTAssertEqual(model.storageFailure, .chatsNotSaved)
        // The failure is shown under the computer in Chats, with Try again.
        XCTAssertEqual(SidebarPresenter.notice(model), .storageFailed(message: DeviceStorageFailure.chatsNotSaved.message))
        // The next save that succeeds clears it; nothing else has to.
        try FileManager.default.removeItem(at: overlay)
        model.setForeground(false)
        for _ in 0..<200 where model.storageFailure != nil { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertNil(model.storageFailure)
        XCTAssertNil(SidebarPresenter.notice(model))
        await model.forget()
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.directory.path))
        let forgotten = try await ProjectionWriter.load(store)
        XCTAssertTrue(forgotten.knownConversationIDs.isEmpty)
    }

    /// Launch must show saved chats from the on-device cache without waiting
    /// for the Mac, and an unreachable Mac must not blank or revoke them.
    @MainActor func testSavedChatsLoadWithoutTheMacAndStayMarkedUnverified() async throws {
        MessageRecoveryURLProtocol.reset()
        let saved = Self.cameraSavedConnection()
        let store = ReadStore(root: FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Wonder/Hosts"),
                              host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        defer { try? store.remove() }
        var previous = ProjectionState()
        previous.install(try JSONDecoder().decode(ConversationSnapshot.self, from: Data(#"{"conversationId":"saved-chat","hostEpoch":"epoch","lastSequence":7,"messages":[{"messageId":"m","body":"Saved question","state":"completed","createdAt":"1700000000000","attachmentIds":[]}],"assistantMessages":[],"thread":{"hydrated":true}}"#.utf8)))
        try store.save(previous)
        MessageRecoveryURLProtocol.goOffline()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [MessageRecoveryURLProtocol.self]
        let model = ConnectionModel(saved: saved, persistConnection: { _ in }, api: PairingAPI(configuration: configuration))
        await model.check(renew: true)
        XCTAssertTrue(model.snapshots.isEmpty, "Launch keeps all bodies on disk")
        let chat = ChatSummary(conversationId: "saved-chat", title: "Saved chat", lastMessagePreview: nil, lastMessageAt: nil,
            messageCount: 1, deliveryState: nil, hasUnread: false, isArchived: false, isPinned: false)
        await model.open(chat, readOnly: true)
        XCTAssertEqual(model.snapshots["saved-chat"]?.messages.first?.body, "Saved question", "Offline open restores the disk body before waiting for the Mac")
        XCTAssertEqual(model.macConnected, false)
        XCTAssertFalse(model.accessEnded, "An unreachable Mac is not revoked access")
        XCTAssertTrue(model.cachedConversationIds.contains("saved-chat"), "Saved content stays marked unverified")
        XCTAssertTrue(model.loadingConversationIDs.isEmpty)
        XCTAssertNil(model.conversationLoadFailures["saved-chat"])
    }

    @MainActor func testHistoryPressurePreservesWindowPinsDraftsAndOfflineReopening() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root); MessageRecoveryURLProtocol.reset() }
        let (model, open) = try recoveryProject(root: root)
        let saved = Self.cameraSavedConnection()
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        func page(_ id: String, running: Bool = false) throws -> ConversationSnapshot {
            try JSONDecoder().decode(ConversationSnapshot.self, from: JSONSerialization.data(withJSONObject: [
                "conversationId": id, "hostEpoch": "epoch", "lastSequence": 7,
                "messages": [["messageId": "m", "body": "Saved \(id)", "state": "completed", "createdAt": "1", "attachmentIds": []]],
                "assistantMessages": [], "thread": ["hydrated": true, "nextCursor": "older-\(id)",
                    "turns": [["id": "turn", "status": running ? "inProgress" : "completed", "items": []]]]
            ]))
        }
        let pinned = try JSONDecoder().decode(ProjectConversationDetail.self, from: Data(#"{"conversationId":"pinned","projectId":"project","projectName":"Todo app","title":"Pinned chat","family":"codex","accessMode":"workspace","workingFolder":"/fixture","workingFolderName":"Todo app","isPinned":true,"hasUnread":false,"hasNativeSession":true,"folderInProject":true}"#.utf8))
        model.projects.installPreviewFilesConversation(pinned)
        model.presentConversation(open)
        let otherWindow = UUID()
        model.setHistoryVisible("window-chat", window: otherWindow)
        model.editDraft("Keep my words", chat: "closed")
        try store.savePosition("saved-anchor", conversation: "closed")
        try model.diagnosticInstallHistory([page(open.id), page("pinned"), page("window-chat"), page("working", running: true), page("closed")])
        await model.diagnosticWaitForHistoryMaintenance()
        model.releaseUnderMemoryPressure()
        await model.diagnosticWaitForHistoryMaintenance()
        XCTAssertEqual(Set(model.snapshots.keys), [open.id, "pinned", "window-chat", "working"])
        XCTAssertEqual(model.composers["closed"]?.draft, "Keep my words")
        XCTAssertEqual(try store.loadPosition(conversation: "closed"), "saved-anchor")
        XCTAssertTrue(try store.load().knownConversationIDs.contains("closed"))
        // A catalog pass must not delete offline history outside its 30 recents.
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: projectCatalog(pinned: false))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/" + open.id, body: try JSONEncoder().encode(page(open.id)))
        await model.loadChats()
        await model.diagnosticWaitForHistoryMaintenance()
        XCTAssertTrue(try store.load().knownConversationIDs.contains("closed"))
        MessageRecoveryURLProtocol.goOffline()
        model.macConnected = false
        let closed = ChatSummary(conversationId: "closed", title: "Closed chat", lastMessagePreview: nil, lastMessageAt: nil,
            messageCount: 1, deliveryState: nil, hasUnread: false, isArchived: false, isPinned: false)
        await model.open(closed, readOnly: true)
        XCTAssertEqual(model.snapshots["closed"]?.messages.first?.body, "Saved closed")
        XCTAssertEqual(model.snapshots["closed"]?.thread.nextCursor, "older-closed")
        XCTAssertEqual(model.composers["closed"]?.draft, "Keep my words")
        XCTAssertEqual(model.rows(for: closed).first?.text, "Saved closed")
        model.dismissConversation(closed)
        model.releaseUnderMemoryPressure()
        await model.diagnosticWaitForHistoryMaintenance()
        XCTAssertNil(model.snapshots["closed"], "A last selection whose window closed is no longer protected")
        XCTAssertEqual(model.composers["closed"]?.draft, "Keep my words")
        model.setHistoryVisible(nil, window: otherWindow)
        model.retireAfterPairingReplacement()
        _ = try await ProjectionWriter.load(store)
    }

    func testUsageGatePreservesModelScopeResetUnknownAndOverage() throws {
        let now = Date(timeIntervalSince1970: 2_000)
        func entry(_ json: String, age: TimeInterval = 0) throws -> CodexUsageCacheEntry {
            CodexUsageCacheEntry(response: try JSONDecoder().decode(CodexUsageResponse.self, from: Data(json.utf8)), fetchedAt: now.addingTimeInterval(-age))
        }
        let json = #"{"agentFamily":"claude","checkedAtMs":2000000,"windows":[{"id":"seven_day_opus","label":"Weekly","usedPercent":100,"remainingPercent":0,"resetsAt":3000}]}"#
        let cached = try entry(json)
        XCTAssertNotNil(cached.exhaustedWindow(model: "claude:claude-opus-5-5", now: now))
        XCTAssertNil(cached.exhaustedWindow(model: "claude:claude-sonnet-5-5", now: now))
        XCTAssertNil(cached.exhaustedWindow(model: "claude:haiku", now: now))
        XCTAssertNil(cached.exhaustedWindow(model: "claude:opus", now: Date(timeIntervalSince1970: 3000)))
        XCTAssertNil(try entry(json, age: 300).exhaustedWindow(model: "claude:opus", now: now))
        XCTAssertNil(try entry(json.replacingOccurrences(of: #""usedPercent":100,"remainingPercent":0"#, with: #""usedPercent":99.9,"remainingPercent":0.1"#)).exhaustedWindow(model: "claude:opus", now: now))
        XCTAssertNil(try entry(json.replacingOccurrences(of: #""checkedAtMs":2000000"#, with: #""checkedAtMs":2000000,"additionalUsageAvailable":true"#)).exhaustedWindow(model: "claude:opus", now: now))
        XCTAssertNil(try entry(#"{"checkedAtMs":2000000,"windows":[]}"#).exhaustedWindow(model: "", now: now))
    }

    // Contract: a provider's reset time counts whether it is reported in
    // seconds or milliseconds, for the Send gate and the limit row alike.
    // Regression: a past reset in milliseconds read as seconds kept Send
    // blocked until the cached usage aged out.
    func testUsageResetInSecondsOrMillisecondsReleasesTheSendGate() throws {
        let now = Date(timeIntervalSince1970: 1_800_000_000)
        func entry(resetsAt: UInt64) throws -> CodexUsageCacheEntry {
            let json = #"{"agentFamily":"codex","checkedAtMs":1800000000000,"windows":[{"id":"primary","label":"5 hours","usedPercent":100,"remainingPercent":0,"windowDurationMins":300,"resetsAt":\#(resetsAt)}]}"#
            return CodexUsageCacheEntry(response: try JSONDecoder().decode(CodexUsageResponse.self, from: Data(json.utf8)), fetchedAt: now)
        }
        let cases: [(name: String, raw: UInt64, reset: TimeInterval, blocks: Bool)] = [
            ("future seconds", 1_800_000_600, 1_800_000_600, true),
            ("past seconds", 1_799_999_400, 1_799_999_400, false),
            ("future milliseconds", 1_800_000_600_000, 1_800_000_600, true),
            ("past milliseconds", 1_799_999_400_000, 1_799_999_400, false),
        ]
        for item in cases {
            let cached = try entry(resetsAt: item.raw)
            XCTAssertEqual(cached.response.windows[0].resetDate, Date(timeIntervalSince1970: item.reset), item.name)
            XCTAssertEqual(cached.exhaustedWindow(model: "fixture-model", now: now) != nil, item.blocks, item.name)
            XCTAssertEqual(ConnectionModel.usageGateExpired(cached, now: now), !item.blocks, item.name)
            XCTAssertEqual(PlanLimitRow.rows(cached.response).first?.resetsAt, Date(timeIntervalSince1970: item.reset), item.name)
        }
        XCTAssertNil(CodexUsageWindow.normalizedReset(0), "No reset time is unknown, not now")
    }

    @MainActor func testExhaustedUsagePreflightPreservesDraftAndNeverSends() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        model.editDraft("Keep this message", chat: chat.id)
        let family = try XCTUnwrap(model.agentFamily(chat))
        let usage = try JSONSerialization.data(withJSONObject: ["agentFamily": family.rawValue, "checkedAtMs": 1,
            "windows": [["id": "primary", "label": "5 hours", "usedPercent": 100, "remainingPercent": 0]], "additionalUsageAvailable": false])
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/account/usage", body: usage)
        XCTAssertTrue(model.canSend(chat), "Missing usage is unknown, not exhausted")
        try await model.loadUsage(family: family, maxAge: 0)
        await model.send(chat)
        XCTAssertFalse(model.canSend(chat))
        XCTAssertNotNil(model.usageLimitMessage(chat))
        XCTAssertEqual(model.composers[chat.id]?.draft, "Keep this message")
        XCTAssertNil(model.composers[chat.id]?.pending)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/recovery-chat/messages").isEmpty)
        let other = CodexUsageCacheEntry(response: try JSONDecoder().decode(CodexUsageResponse.self, from: usage), fetchedAt: Date())
        model.usageCache = [(family == .codex ? AgentFamily.claude : .codex): [model.assignmentScope: other]]
        XCTAssertTrue(model.canSend(chat), "Another provider's limit must not block this model")
    }

    func testCodexUsageResponseDecodesTheHostContract() throws {
        let data = Data(#"{"checkedAtMs":1700000000000,"windows":[{"id":"five-hours","label":"5 hours","usedPercent":27,"remainingPercent":73,"windowDurationMins":300,"resetsAt":1700018000000},{"id":"weekly","label":"Weekly","usedPercent":41,"remainingPercent":59,"windowDurationMins":10080,"resetsAt":1700604800000}]}"#.utf8)
        let response = try JSONDecoder().decode(CodexUsageResponse.self, from: data)
        XCTAssertEqual(response.windows.map(\.label), ["5 hours", "Weekly"])
        XCTAssertEqual(response.windows.map(\.roundedRemainingPercent), [73, 59])
        XCTAssertEqual(response.windows[0].windowDurationMins, 300)
    }

    @MainActor func testThumbnailsDeduplicateCacheAndInvalidateRevisions() async throws {
        let data = Self.imageData()
        let probe = ImageLoadProbe(data: data)
        let cache = ToolImagePreviews()
        let scope = UUID()
        let original = try Self.imageRequest(scope: scope, data: data)
        async let first = cache.image(for: original) { try await probe.load() }
        async let second = cache.image(for: original) { try await probe.load() }
        let (a, b) = try await (first, second)
        XCTAssertTrue(a === b)
        _ = try await cache.image(for: original) { try await probe.load() }
        let initialCalls = await probe.calls; XCTAssertEqual(initialCalls, 1)

        for request in [try Self.imageRequest(scope: scope, data: data, revision: "changed"),
                        try Self.imageRequest(scope: UUID(), data: data)] {
            _ = try await cache.image(for: request) { try await probe.load() }
        }
        let removed = try Self.imageRequest(scope: scope, data: data, state: "removed")
        do {
            _ = try await cache.image(for: removed) {
                _ = try await probe.load()
                throw FileFailure.integrity
            }
            XCTFail("A removed file reused its old thumbnail")
        } catch FileFailure.integrity { }
        let revisedCalls = await probe.calls; XCTAssertEqual(revisedCalls, 4)
        cache.invalidate(); XCTAssertEqual(cache.cachedBytes, 0)
        _ = try await cache.image(for: original) { try await probe.load() }
        let reloadedCalls = await probe.calls; XCTAssertEqual(reloadedCalls, 5)
    }

    @MainActor func testThumbnailWorkIsBoundedAndCancelledConsumersDoNotReload() async throws {
        let data = Self.imageData()
        let probe = ImageLoadProbe(data: data, delay: .milliseconds(200))
        let cache = ToolImagePreviews(maximumBytes: 250_000)
        let scope = UUID()
        let requests = try (0..<8).map { try Self.imageRequest(scope: scope, data: data, revision: String($0)) }
        var tasks = requests.prefix(2).map { key in Task { try await cache.image(for: key) { try await probe.load() } } }
        while await probe.calls < 2 { await Task.yield() }
        tasks += requests.dropFirst(2).map { key in Task { try await cache.image(for: key) { try await probe.load() } } }
        // This request is queued behind the two active workers.
        tasks[7].cancel()
        for (index, task) in tasks.enumerated() {
            do { _ = try await task.value; XCTAssertNotEqual(index, 7) }
            catch is CancellationError { XCTAssertEqual(index, 7) }
        }
        let maximum = await probe.maximumActive; XCTAssertEqual(maximum, 2)
        let calls = await probe.calls; XCTAssertEqual(calls, 7)
        XCTAssertLessThanOrEqual(cache.cachedBytes, 250_000)
        // An evicted image must be loaded again, without retaining all history.
        _ = try await cache.image(for: requests[0]) { try await probe.load() }
        let afterEviction = await probe.calls; XCTAssertEqual(afterEviction, 8)

        let pending = Task { try await cache.image(for: requests[7]) { try await probe.load() } }
        while await probe.calls < 9 { await Task.yield() }
        cache.invalidate()
        do { _ = try await pending.value; XCTFail("Invalidated work returned a stale image") }
        catch is CancellationError { }
        XCTAssertEqual(cache.cachedBytes, 0)
        _ = try await cache.image(for: requests[7]) { try await probe.load() }
        let afterCancellation = await probe.calls; XCTAssertEqual(afterCancellation, 10)
        XCTAssertLessThanOrEqual(cache.cachedBytes, 250_000)
    }

    @MainActor private static func imageData() -> Data {
        let format = UIGraphicsImageRendererFormat(); format.scale = 1
        return UIGraphicsImageRenderer(size: CGSize(width: 240, height: 240), format: format).pngData { context in
            UIColor.systemBlue.setFill(); context.fill(CGRect(x: 0, y: 0, width: 240, height: 240))
        }
    }
    private static func imageRequest(scope: UUID, data: Data, revision: String = "original", state: String = "available",
                                     id: String = "image", size: PicturePreviewSize = .single) throws -> ToolImageRequest {
        let value: [String: Any] = ["id": id, "name": "image.png", "mimeType": "image/png", "byteSize": data.count, "sha256": ConversationFile.digest(data), "state": state, "updatedAt": revision]
        let file = try JSONDecoder().decode(ConversationFile.self, from: JSONSerialization.data(withJSONObject: value))
        return ToolImageRequest(scope: scope, chatID: "chat", file: file, size: size)
    }

    @MainActor func testProjectPreparationRejectsOverlappingSendsAndPreservesClickedDraft() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        model.editDraft("Original message", chat: chat.id)
        let activityPath = "/api/v1/conversations/recovery-chat/history/activity"
        MessageRecoveryURLProtocol.hold(path: activityPath)
        let first = Task { @MainActor in await model.send(chat) }
        defer { MessageRecoveryURLProtocol.releaseHeld() }
        for _ in 0..<100 {
            if !MessageRecoveryURLProtocol.bodies(path: activityPath, includingEmpty: true).isEmpty { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(model.preparingSends.contains(chat.id))
        XCTAssertFalse(model.canSend(chat))
        model.editDraft("Later edit", chat: chat.id)
        await model.send(chat)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: activityPath, includingEmpty: true).count, 1)
        MessageRecoveryURLProtocol.releaseHeld()
        await first.value
        let bodies = MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/recovery-chat/messages")
        XCTAssertEqual(bodies.count, 1)
        XCTAssertEqual(try JSONDecoder().decode(SendRequest.self, from: XCTUnwrap(bodies.first)).body, "Original message")
        XCTAssertFalse(model.preparingSends.contains(chat.id))
    }

    @MainActor func testAsyncReplyRejectsOversizedTextBeforePersistingOrSending() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = recoveryModel(root: root)
        let chat = try Self.cameraChat(id: "recovery-chat")
        let question = try recoveryQuestion()
        await model.replyAsync(question, chat: chat, answers: [String(repeating: "é", count: 4097)], skip: false)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/recovery-chat/questions/q").isEmpty)
        XCTAssertNil(try ReadStore(root: root, host: "camera-unit-host", device: "camera-unit-device").loadIntent(conversation: "async-reply-q"))
        XCTAssertNotNil(model.attentionErrors[question.id])
        XCTAssertFalse(model.retryableAsyncReplies.contains(question.id))
    }

    @MainActor func testAmbiguousAsyncReplyRetainsExactSavedPayloadAcrossRestart() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let chat = try Self.cameraChat(id: "recovery-chat")
        let question = try recoveryQuestion()
        let path = "/api/v1/conversations/recovery-chat/questions/q"
        MessageRecoveryURLProtocol.fail(path: path, error: .timedOut)
        await recoveryModel(root: root).replyAsync(question, chat: chat, answers: ["Original answer"], skip: false)
        MessageRecoveryURLProtocol.enqueue(path: path, status: 204)
        await recoveryModel(root: root).replyAsync(question, chat: chat, answers: [], skip: true)
        let bodies = MessageRecoveryURLProtocol.bodies(path: path)
        XCTAssertEqual(bodies.count, 2)
        XCTAssertEqual(bodies.first, bodies.last, "An ambiguous reply must not silently become Skip")
    }

    @MainActor func testSharedConnectionMaintenanceSurvivesAnotherWindowClosing() {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let library = ConnectionLibrary(diagnosticModels: [recoveryModel(root: root)])
        let first = UUID(), second = UUID()
        library.setScene(first, phase: .active)
        library.setScene(second, phase: .active)
        XCTAssertEqual(library.foregroundOwner, first)
        library.setScene(second, phase: nil)
        XCTAssertEqual(library.foregroundOwner, first)
        library.setScene(second, phase: .active)
        library.setScene(first, phase: nil)
        XCTAssertEqual(library.foregroundOwner, second)
        library.setScene(second, phase: nil)
        XCTAssertNil(library.foregroundOwner)
    }

    // Creation handoff owns one first-message identity across restart. A lost
    // creation response must not replace another saved composer or send twice.
    @MainActor func testCreationMessageTransfersToDurableOutboxOnce() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let chat = try Self.cameraChat(id: "creation-outbox")
        let requestID = UUID().uuidString
        let model = recoveryModel(root: root)
        try model.prepareCreationMessage(chat, body: "Help with testing", requestID: requestID)
        let restarted = recoveryModel(root: root)
        try restarted.prepareCreationMessage(chat, body: "Help with testing", requestID: requestID)
        XCTAssertEqual(restarted.composers[chat.id]?.pending?.request.clientMessageId, requestID)
        XCTAssertEqual(restarted.composers[chat.id]?.pending?.request.body, "Help with testing")
        XCTAssertThrowsError(try restarted.prepareCreationMessage(chat, body: "Different message", requestID: UUID().uuidString))
        XCTAssertEqual(restarted.composers[chat.id]?.pending?.request.body, "Help with testing")

        // Delivery can reconcile before the New Chat draft is retired. Replay
        // must leave the accepted composer empty and must not upload the note.
        MessageRecoveryURLProtocol.reset()
        let snapshot = try JSONDecoder().decode(ConversationSnapshot.self,
            from: JSONSerialization.data(withJSONObject: ["conversationId": chat.id,
                "hostEpoch": "epoch", "lastSequence": 1,
                "messages": [["messageId": "accepted", "clientMessageId": requestID,
                    "body": "Help with testing", "state": "completed", "createdAt": "1700000000000",
                    "attachmentIds": ["annotation-file"]]], "assistantMessages": [],
                "thread": ["hydrated": true]]))
        var reconciled = try XCTUnwrap(restarted.composers[chat.id])
        reconciled.reconcile(snapshot)
        try ReadStore(root: root, host: "camera-unit-host", device: "camera-unit-device")
            .saveComposer(reconciled, conversation: chat.id)
        let recovered = recoveryModel(root: root)
        recovered.snapshots[chat.id] = snapshot
        let note = try ArtifactAnnotation(projectId: "project", conversationId: chat.id,
            rootId: "workspace", path: "README.md", source: Data("Source".utf8),
            startLine: 1, endLine: 1, note: "Check this").stagedFile()
        try await recovered.prepareCreation(chat, body: "Help with testing", requestID: requestID, files: [note])
        XCTAssertEqual(recovered.composers[chat.id]?.draft, "")
        XCTAssertEqual(recovered.composers[chat.id]?.attachmentCount, 0)
        XCTAssertNil(recovered.composers[chat.id]?.pending)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/\(chat.id)/files", includingEmpty: true).isEmpty)
        recovered.editDraft("Next question", chat: chat.id)
        try await recovered.prepareCreation(chat, body: "Help with testing", requestID: requestID, files: [note])
        XCTAssertEqual(recovered.composers[chat.id]?.draft, "Next question", "Replay must preserve a newer draft")
    }

    // Choosing another destination retains both drafts and any uncertain
    // request. Connection removal clears only the selected Mac's drafts.
    @MainActor func testNewChatDraftFlushesBeforeBackgroundAndKillAcrossThreeCycles() async throws {
        let host = "background-new-chat-" + UUID().uuidString
        defer { NewChatDraftStore.remove(host: host) }
        let notifications = NotificationCenter()
        let file = try StagedFile(name: "kept.txt", mimeType: "text/plain", data: Data("Kept attachment".utf8))
        let attachment = try NewChatDraftStore.stage(file)
        defer {
            let root = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            try? FileManager.default.removeItem(at: root.appendingPathComponent("NewChatAttachments").appendingPathComponent(file.id))
        }
        var draft = NewChatDraft(destination: .project(id: "owned-project"), text: "", family: .codex)
        draft.attachments = [attachment]
        XCTAssertTrue(NewChatDraftStore.save(draft, host: host))
        for cycle in 1...3 {
            var owner: NewChatDraftPersistence? = NewChatDraftPersistence(notifications: notifications)
            weak var releasedOwner = owner
            let before = draft.text
            draft.text += "Cycle \(cycle)\n"
            owner?.update(host: host, draft: draft)
            XCTAssertEqual(NewChatDraftStore.load(host: host)?.text, before, "Still inside the save debounce")
            notifications.post(name: cycle == 1 ? UIApplication.willResignActiveNotification : UIApplication.didEnterBackgroundNotification, object: nil)
            XCTAssertEqual(NewChatDraftStore.load(host: host), draft, "Backgrounding flushes before suspension")
            owner = nil
            XCTAssertNil(releasedOwner, "The delayed save must not keep an abandoned owner alive")
            draft = try XCTUnwrap(NewChatDraftStore.load(host: host))
            XCTAssertEqual(try NewChatDraftStore.files(try XCTUnwrap(draft.attachments)).first?.data, file.data)
        }
        let owner = NewChatDraftPersistence(notifications: notifications)
        draft.text += "Immediate termination"
        owner.update(host: host, draft: draft)
        notifications.post(name: UIApplication.willTerminateNotification, object: nil)
        XCTAssertEqual(NewChatDraftStore.load(host: host), draft)
        draft.text += " and a pause"
        owner.update(host: host, draft: draft)
        try await Task.sleep(for: .milliseconds(700))
        XCTAssertEqual(NewChatDraftStore.load(host: host), draft, "Ordinary debounce still saves")
    }

    @MainActor func testNewChatDraftsRestorePerDestinationAndStayConnectionScoped() throws {
        let host = "draft-test-" + UUID().uuidString
        let otherHost = "other-" + host
        defer { NewChatDraftStore.remove(host: host); NewChatDraftStore.remove(host: otherHost) }
        var first = NewChatDraft(destination: .project(id: "first-project"), text: "First project notes")
        let file = try StagedFile(name: "draft.txt", mimeType: "text/plain", data: Data("Owned bytes".utf8))
        let descriptor = try NewChatDraftStore.stage(file)
        let storedFile = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("NewChatAttachments").appendingPathComponent(file.id)
        defer { try? FileManager.default.removeItem(at: storedFile) }
        first.attachments = [descriptor]
        let projectDraft = NewChatDraftStore.selecting(.project(id: "draft-project"), from: first, host: host, capabilities: ProviderCapabilities.fixtureMac)
        XCTAssertEqual(projectDraft.text, first.text, "Choosing an empty project keeps the owner's words")
        XCTAssertEqual(projectDraft.attachments, first.attachments)
        XCTAssertEqual(NewChatDraftStore.load(host: host, destination: .project(id: "first-project")), first,
                       "Carrying to a project must keep the outgoing draft recoverable")
        var otherProject = projectDraft
        otherProject.text = "Independent project draft"
        let secondProject = NewChatDraftStore.selecting(.project(id: "second-project"), from: otherProject, host: host, capabilities: ProviderCapabilities.fixtureMac)
        XCTAssertEqual(secondProject.text, otherProject.text)
        var editedSecond = secondProject
        editedSecond.text = "Second project words"
        let returnedProject = NewChatDraftStore.selecting(.project(id: "draft-project"), from: editedSecond, host: host, capabilities: ProviderCapabilities.fixtureMac)
        XCTAssertEqual(returnedProject.text, otherProject.text)
        XCTAssertEqual(NewChatDraftStore.load(host: host, destination: .project(id: "second-project"))?.text, editedSecond.text)
        let third = NewChatDraftStore.selecting(.project(id: "third-project"), from: first, host: host, capabilities: ProviderCapabilities.fixtureMac)
        XCTAssertEqual(third.text, first.text, "An empty Project draft carries the current words")
        var edited = third; edited.text = "Third project notes"
        let recovered = NewChatDraftStore.selecting(.project(id: "first-project"), from: edited, host: host, capabilities: ProviderCapabilities.fixtureMac)
        XCTAssertEqual(recovered.text, "First project notes")
        let staged = try NewChatDraftStore.files(try XCTUnwrap(recovered.attachments))
        XCTAssertEqual(staged.first?.data, file.data)
        XCTAssertNil(staged.first?.uploaded, "A new destination must upload local bytes through its own authenticated route")
        let moved = NewChatDraft.switching(from: recovered, toSaved: nil)
        XCTAssertNil(moved.destination)
        XCTAssertEqual(moved.attachments, recovered.attachments)
        try Data("Changed bytes".utf8).write(to: storedFile)
        XCTAssertThrowsError(try NewChatDraftStore.files(try XCTUnwrap(moved.attachments)))
        XCTAssertEqual(NewChatDraftStore.load(host: host, destination: .project(id: "third-project"))?.text, "Third project notes")
        first.freeze()
        XCTAssertEqual(NewChatDraftStore.selecting(.project(id: "third-project"), from: first, host: host, capabilities: ProviderCapabilities.fixtureMac), first)
        NewChatDraftStore.save(edited, host: otherHost)
        // Selection now saves the displayed draft immediately. Explicitly
        // navigate away before delivering a late result to the first Project draft.
        let active = NewChatDraftStore.selecting(.project(id: "third-project"), from: recovered, host: host, capabilities: ProviderCapabilities.fixtureMac)
        XCTAssertEqual(active.text, "Third project notes")
        XCTAssertEqual(NewChatDraftStore.load(host: host)?.requestID, active.requestID)
        XCTAssertNotEqual(active.requestID, recovered.requestID)
        NewChatDraftStore.remove(host: host)
        XCTAssertNil(NewChatDraftStore.load(host: host, destination: .project(id: "first-project")))
        XCTAssertEqual(NewChatDraftStore.load(host: otherHost)?.text, "Third project notes")
    }

    // Contract: leaving an unconfirmed creation unlocks a separate draft while
    // preserving the original retry identity, attachment bytes and pairing.
    // Reviewing, confirming or rejecting it cannot erase newer project words.
    /// Recovery is a projection of durable records: zero, one, many and unreadable
    /// records survive owner reconstruction; queued sends stay with the outbox.
    @MainActor func testNewChatRecoveryPreservesZeroOneManyAndCorruptRecords() throws {
        let host = "recovery-" + UUID().uuidString
        defer { NewChatDraftStore.remove(host: host) }
        XCTAssertTrue(NewChatDraftStore.recoveryRecords(host: host).isEmpty)
        var first = NewChatDraft(destination: .project(id: "project"), text: "First unsent words")
        first.freeze()
        XCTAssertTrue(NewChatDraftStore.save(first, host: host))
        XCTAssertEqual(NewChatDraftStore.load(host: host), first, "one record reopens as the New chat draft, with its retry identity")
        XCTAssertEqual(NewChatDraftStore.recoveryRecords(host: host).compactMap(\.draft), [first])
        var second = NewChatDraft(destination: .project(id: "project"), text: "Second unsent words")
        second.freeze()
        XCTAssertTrue(NewChatDraftStore.save(second, host: host))
        var queued = NewChatDraft(destination: .project(id: "project"), text: "Deliver when reachable")
        queued.queue(device: "owned-device")
        XCTAssertNotNil(NewChatDraftStore.startNew(from: queued, host: host))
        let records = NewChatDraftStore.recoveryRecords(host: host)
        XCTAssertEqual(Set(records.compactMap { $0.draft?.requestID }), [first.requestID, second.requestID])
        XCTAssertEqual(NewChatDraftStore.queued(host: host).map(\.requestID), [queued.requestID])

        // Legacy preferences and file-backed records both participate. A write
        // cannot overwrite corrupt bytes before the owner can discard them.
        let key = "wonder.newchat.draft." + Data(host.utf8).base64EncodedString()
        let corruptKey = key + ".pending.corrupt"
        let bytes = Data("{broken saved content".utf8)
        UserDefaults.standard.set(bytes, forKey: corruptKey)
        let corrupt = try XCTUnwrap(NewChatDraftStore.recoveryRecords(host: host).first { $0.draft == nil })
        XCTAssertEqual(corrupt.id, corruptKey)
        XCTAssertEqual(UserDefaults.standard.data(forKey: corruptKey), bytes)
        XCTAssertEqual(NewChatDraftStore.recoveryRecords(host: host).count, 3)
        let directory = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("NewChatDrafts", isDirectory: true)
        try bytes.write(to: directory.appendingPathComponent(ConversationFile.digest(Data(key.utf8))), options: .atomic)
        XCTAssertTrue(NewChatDraftStore.save(NewChatDraft(text: "New words"), host: host))
        XCTAssertEqual(NewChatDraftStore.load(host: host)?.text, "New words")
        XCTAssertEqual(NewChatDraftStore.recoveryRecords(host: host).filter { $0.draft == nil }.count, 2)
        let backedUp = try XCTUnwrap(NewChatDraftStore.recoveryRecords(host: host).first { $0.id.contains(".unreadable.") })
        XCTAssertEqual(try Data(contentsOf: directory.appendingPathComponent(ConversationFile.digest(Data(backedUp.id.utf8)))), bytes)
        XCTAssertTrue(NewChatDraftStore.discard(corrupt, host: host))
        XCTAssertNil(UserDefaults.standard.object(forKey: corruptKey))
        XCTAssertTrue(NewChatDraftStore.discard(try XCTUnwrap(records.first { $0.draft == first }), host: host))
        XCTAssertFalse(NewChatDraftStore.recoveryRecords(host: host).contains { $0.draft?.requestID == first.requestID })
        XCTAssertEqual(NewChatDraftStore.queued(host: host).map(\.requestID), [queued.requestID])
        var uncertain = NewChatDraft(text: "T3 Code message awaiting confirmation")
        uncertain.startsInT3Code = true
        uncertain.t3LaunchUnconfirmed = "Check your computer before sending again."
        XCTAssertTrue(NewChatDraftStore.save(uncertain, host: host, separately: true, makeCurrent: false))
        var stale = uncertain
        stale.t3LaunchUnconfirmed = nil
        XCTAssertTrue(NewChatDraftStore.save(stale, host: host, separately: true, makeCurrent: false))
        XCTAssertEqual(NewChatDraftStore.recoveryRecords(host: host).first { $0.draft?.requestID == uncertain.requestID }?.draft, uncertain,
                       "a stale save cannot remove an archived uncertain-launch warning")
    }

    // Contract: saving a project pin updates the thread, global Pinned list and
    // offline detail together. Regression: a details-sheet save left the old
    // cache behind, and its overlapping GET put the unpinned detail back.
    @MainActor func testProjectPinSaveSurvivesStaleDetailAndRelaunch() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        defer { MessageRecoveryURLProtocol.releaseHeld(); model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        try await prepareProject(model)
        let path = "/api/v1/project-conversations/project-chat"
        MessageRecoveryURLProtocol.enqueue(path: path, method: "GET", body: projectDetail(pinned: false))
        MessageRecoveryURLProtocol.hold(path: path, method: "GET")
        let stale = Task { try await model.projects.loadDetail("project-chat") }
        for _ in 0..<100 {
            if MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count >= 2 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count, 2)
        MessageRecoveryURLProtocol.enqueue(path: path, method: "PATCH", body: projectDetail(pinned: true))
        try await model.projects.updateConversation("project-chat", fields: ["isPinned": true])
        XCTAssertEqual(model.projects.pinned.map(\.thread.conversationId), ["project-chat"])
        MessageRecoveryURLProtocol.releaseHeld()
        let refreshed = try await stale.value
        XCTAssertTrue(refreshed.isPinned)
        XCTAssertTrue(try XCTUnwrap(model.projects.threads["project"]?.threads.first).isPinned)
        await ProjectCatalogWriter.shared.settle()
        let restarted = recoveryModel(root: root)
        XCTAssertEqual(restarted.projects.details["project-chat"]?.isPinned, true)
        XCTAssertEqual(restarted.projects.pinned.map(\.thread.conversationId), ["project-chat"])
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/project-chat/messages").isEmpty)
    }

    // Contract: a pin save joining an older catalog read still obtains a new
    // catalog; late thread pages cannot undo its pin. The transport controls
    // the actual production request boundary rather than mocking the library.
    @MainActor func testProjectPinRefreshAndThreadPageKeepNewerSave() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        defer { MessageRecoveryURLProtocol.releaseHeld(); model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        try await prepareProject(model)
        let pagePath = "/api/v1/projects/project/threads"
        MessageRecoveryURLProtocol.enqueue(path: pagePath, body: projectPage(pinned: false))
        MessageRecoveryURLProtocol.hold(path: pagePath)
        model.projects.loadThreads("project")
        for _ in 0..<100 {
            if MessageRecoveryURLProtocol.bodies(path: pagePath, includingEmpty: true).count >= 2 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/project-chat", body: projectDetail(pinned: true))
        try await model.projects.updateConversation("project-chat", fields: ["isPinned": true])
        MessageRecoveryURLProtocol.releaseHeld()
        for _ in 0..<100 {
            if model.projects.threads["project"]?.isLoading == false { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(try XCTUnwrap(model.projects.threads["project"]?.threads.first).isPinned)

        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: projectCatalog(pinned: true))
        MessageRecoveryURLProtocol.hold(path: "/api/v1/projects")
        let stale = Task { await model.projects.refresh() }
        for _ in 0..<100 {
            if MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects", includingEmpty: true).count >= 2 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/project-chat", body: projectDetail(pinned: false))
        try await model.projects.updateConversation("project-chat", fields: ["isPinned": false])
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: projectCatalog(pinned: false))
        MessageRecoveryURLProtocol.enqueue(path: pagePath, body: projectPage(pinned: false))
        MessageRecoveryURLProtocol.enqueue(path: pagePath, body: projectPage(pinned: false))
        let afterSave = Task { await model.projects.refresh() }
        await Task.yield()
        MessageRecoveryURLProtocol.releaseHeld()
        await stale.value; await afterSave.value
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects", includingEmpty: true).count, 3)
        XCTAssertTrue(model.projects.pinned.isEmpty)
        XCTAssertEqual(model.projects.details["project-chat"]?.isPinned, false)
    }

    // I1: closing or backgrounding Files while Add folder waits for the
    // computer drops the late answer: no reread, no message, no spinner.
    @MainActor func testFilesAddFolderAnsweredAfterLeavingChangesNothing() async {
        let adder = WorkspaceFolderAdder()
        var reloads = 0, failures = 0
        for outcome: Error? in [nil, PairingFailure.response(500)] {
            let (gate, open) = AsyncStream<Void>.makeStream()
            adder.start(add: {
                for await _ in gate { break }
                if let outcome { throw outcome }
            }, added: { reloads += 1 }, failed: { _ in failures += 1 })
            XCTAssertTrue(adder.adding)
            adder.cancel()
            XCTAssertFalse(adder.adding)
            open.yield(); open.finish()
            try? await Task.sleep(for: .milliseconds(50))
            XCTAssertEqual(reloads, 0, "\(String(describing: outcome))")
            XCTAssertEqual(failures, 0, "\(String(describing: outcome))")
            XCTAssertFalse(adder.adding)
        }
        // Closing Files during the reread that follows a successful add cancels
        // it, so the late folder list changes nothing.
        var updates = 0
        let (reread, answer) = AsyncStream<Void>.makeStream()
        let (entered, enter) = AsyncStream<Void>.makeStream()
        adder.start(add: {}, added: {
            enter.yield()
            for await _ in reread { break }
            guard !Task.isCancelled else { return }
            updates += 1
        }, failed: { _ in failures += 1 })
        for await _ in entered { break }
        adder.cancel()
        answer.yield(); answer.finish()
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(updates, 0, "closing Files cancels the reread after Add folder")
        XCTAssertEqual(failures, 0)
        XCTAssertFalse(adder.adding)
        // Staying on Files, the answer rereads the workspace or explains the failure.
        adder.start(add: {}, added: { reloads += 1 }, failed: { _ in failures += 1 })
        adder.start(add: { throw PairingFailure.response(500) }, added: { reloads += 1 }, failed: { _ in failures += 1 })
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(reloads, 1, "a second tap while adding is ignored")
        adder.start(add: { throw PairingFailure.response(500) }, added: { reloads += 1 }, failed: { _ in failures += 1 })
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(failures, 1)
        XCTAssertFalse(adder.adding)
    }

    // Contract: an Existing folder create the computer refuses frees its form;
    // one it never answers is kept, and Start over followed by the same Project
    // resends that request, so a Project the computer already made is returned,
    // not added twice. A pending create found in the catalog counts as made.
    @MainActor func testExistingFolderCreateRefusalFreesFormAndUncertainCreateKeepsItsRequest() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        let drafts = try XCTUnwrap(model.managementDrafts)
        func clear() { drafts.remove(PendingProjectCreate.formKey); drafts.remove(PendingProjectCreate.setAsideKey) }
        clear()
        defer { clear(); model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        let create = "/api/v1/projects"
        func catalog(_ projects: String) -> Data { Data(#"{"projects":[\#(projects)],"families":[],"modesVersion":1,"pinned":[]}"#.utf8) }
        let made = #"{"id":"made","name":"App","isIncluded":true,"isPinned":false,"rootsRevision":1,"folders":[{"id":"f0","path":"/repo","name":"repo","isPrimary":true,"isAvailable":true}],"createdAt":"fixture"}"#
        func sentRequestIDs() throws -> [String] {
            try MessageRecoveryURLProtocol.bodies(path: create).compactMap {
                try XCTUnwrap(JSONSerialization.jsonObject(with: $0) as? [String: Any])["requestId"] as? String
            }
        }
        MessageRecoveryURLProtocol.enqueue(path: create, method: "GET", body: catalog(""))
        await model.projects.refresh()
        XCTAssertEqual(NoProjectsNotice.text(supportsProjects: model.projects.supportsProjects, projects: model.projects.projects, macName: "Studio"),
                       "No Projects on Studio", "An empty computer says so instead of only disabling Send")
        let app = PendingProjectCreate.Input(name: "App", folders: ["/repo"], primaryIndex: 0)

        // A refusal with the computer's reason frees the form at once.
        MessageRecoveryURLProtocol.enqueue(path: create, method: "POST", status: 422, body: Data("That folder holds protected computer settings.".utf8))
        guard case .refused(let reason) = await PendingProjectCreate.create(app, library: model.projects, drafts: drafts) else { return XCTFail("a refusal must free the form") }
        XCTAssertEqual(reason, "That folder holds protected computer settings.")
        XCTAssertNil(PendingProjectCreate.pending(drafts), "a refused create is not frozen on Retry create")

        // No answer: the request is kept for Retry.
        MessageRecoveryURLProtocol.fail(path: create, error: .timedOut)
        guard case .unconfirmed = await PendingProjectCreate.create(app, library: model.projects, drafts: drafts) else { return XCTFail("an unanswered create must be kept") }
        XCTAssertEqual(PendingProjectCreate.pending(drafts), app)

        // Start over frees the form; another Project gets its own request.
        PendingProjectCreate.setAside(drafts)
        XCTAssertNil(PendingProjectCreate.pending(drafts))
        let other = PendingProjectCreate.Input(name: "Other", folders: ["/other"], primaryIndex: 0)
        MessageRecoveryURLProtocol.enqueue(path: create, method: "POST", status: 503, body: Data("Busy".utf8))
        guard case .unconfirmed = await PendingProjectCreate.create(other, library: model.projects, drafts: drafts) else { return XCTFail("a 503 is not an answer") }
        PendingProjectCreate.setAside(drafts)

        // The same Project again reuses the unanswered request, which the computer had made.
        MessageRecoveryURLProtocol.enqueue(path: create, method: "POST", body: Data(made.utf8))
        guard case .created(let project) = await PendingProjectCreate.create(app, library: model.projects, drafts: drafts) else { return XCTFail("the retry must return the made Project") }
        XCTAssertEqual(project.id, "made")
        let ids = try sentRequestIDs()
        XCTAssertEqual(ids.count, 4)
        XCTAssertNotEqual(ids[0], ids[1], "a refused request is not reused")
        XCTAssertNotEqual(ids[2], ids[1], "another Project gets its own request")
        XCTAssertEqual(ids[3], ids[1], "the same Project after Start over resends its unanswered request")
        XCTAssertEqual(drafts.load(PendingProjectCreate.setAsideKey)?.values.count, 1, "only the answered request is forgotten")
        XCTAssertNil(PendingProjectCreate.pending(drafts))

        // A request the computer says made a different Project frees the form and rereads the catalog.
        MessageRecoveryURLProtocol.enqueue(path: create, method: "POST", status: 409, body: Data("This request was already used for a different project.".utf8))
        MessageRecoveryURLProtocol.enqueue(path: create, method: "GET", body: catalog(made))
        guard case .refused(let conflict) = await PendingProjectCreate.create(other, library: model.projects, drafts: drafts) else { return XCTFail("a 409 is an answer") }
        XCTAssertFalse(conflict.contains("request"), "no protocol wording")
        XCTAssertNil(drafts.load(PendingProjectCreate.setAsideKey), "the answered set-aside request is forgotten")
        XCTAssertEqual(model.projects.projects.map(\.id), ["made"])

        // Reopening a pending create that shows in the catalog finds it made.
        XCTAssertEqual(PendingProjectCreate.reconciled(app, in: model.projects.projects)?.id, "made")
        XCTAssertNil(PendingProjectCreate.reconciled(other, in: model.projects.projects))
        XCTAssertTrue(PendingProjectCreate.isRefusal(PairingFailure.response(400)))
        XCTAssertFalse(PendingProjectCreate.isRefusal(PairingFailure.response(401)), "access ended before an answer may follow a create")
        XCTAssertFalse(PendingProjectCreate.isRefusal(URLError(.networkConnectionLost)))
    }

    // Contract: a settings read that started before an auto-archive save and
    // returns after it cannot put the old rule back on screen.
    @MainActor func testAutoArchiveSaveFencesStaleSettingsRead() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        defer { MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        let path = ProjectDiscoverySettings.path
        func settings(days: Int) throws -> Data {
            try JSONEncoder().encode(ProjectDiscoverySettings(includeWorktrees: true,
                autoArchive: AutoArchiveRule(idleDays: days, onMerge: true)))
        }
        MessageRecoveryURLProtocol.enqueue(path: path, method: "GET", body: try settings(days: 3))
        MessageRecoveryURLProtocol.hold(path: path, method: "GET")
        let stale = Task { await model.projects.refreshWorktreeSettings() }
        try await waitForRecoveryRequests(path: path)
        MessageRecoveryURLProtocol.enqueue(path: path, method: "PUT", body: try settings(days: 7))
        await model.projects.setAutoArchive(AutoArchiveRule(idleDays: 7, onMerge: true))
        XCTAssertEqual(model.projects.autoArchive?.idleDays, 7)
        MessageRecoveryURLProtocol.releaseHeld()
        await stale.value
        XCTAssertEqual(model.projects.autoArchive?.idleDays, 7, "A stale read replaced the saved rule")
        XCTAssertEqual(model.projects.includeWorktrees, true, "The read still updates the worktree setting")
    }

    // Contract: a "Continuing work" save is not undone by a slower settings
    // read begun before it, and a refused resume Cancel says so and keeps
    // the resume, while "already gone" (404) counts as cancelled.
    @MainActor func testContinuationSaveFencesStaleReadAndResumeCancelReportsFailure() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        defer { MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        let path = ProjectDiscoverySettings.path
        func settings(restart: Bool) throws -> Data {
            try JSONEncoder().encode(ProjectDiscoverySettings(includeWorktrees: true, resumeAfterLimit: true, continueAfterRestart: restart))
        }
        MessageRecoveryURLProtocol.enqueue(path: path, method: "GET", body: try settings(restart: true))
        MessageRecoveryURLProtocol.hold(path: path, method: "GET")
        let stale = Task { await model.projects.refreshWorktreeSettings() }
        try await waitForRecoveryRequests(path: path)
        MessageRecoveryURLProtocol.enqueue(path: path, method: "PUT", body: try settings(restart: false))
        await model.projects.setContinuation(ContinuationSettings(resumeAfterLimit: true, continueAfterRestart: false))
        XCTAssertEqual(model.projects.continuation?.continueAfterRestart, false)
        MessageRecoveryURLProtocol.releaseHeld()
        await stale.value
        XCTAssertEqual(model.projects.continuation?.continueAfterRestart, false, "A stale read replaced the saved choice")

        let cancel = "/api/v1/conversations/project-chat/usage-resume"
        MessageRecoveryURLProtocol.enqueue(path: cancel, method: "DELETE", status: 503, body: Data("busy".utf8))
        await model.projects.cancelResume("project-chat")
        XCTAssertEqual(model.projects.resumeFailures["project-chat"], "The resume could not be cancelled. Try again.")
        XCTAssertFalse(model.projects.cancellingResume.contains("project-chat"))
        MessageRecoveryURLProtocol.enqueue(path: cancel, method: "DELETE", status: 404)
        await model.projects.cancelResume("project-chat")
        XCTAssertNil(model.projects.resumeFailures["project-chat"])
    }

    // Contract: confirmed archives survive stale catalog responses without
    // discarding drafts; a desktop restore makes the native thread visible again.
    @MainActor func testProjectArchiveFencesStalePageAndPreservesDraftOnRestore() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        defer { MessageRecoveryURLProtocol.releaseHeld(); model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        // Reuse the Project fixtures with the native Codex family.
        func codex(_ data: Data) -> Data {
            Data(String(decoding: data, as: UTF8.self).replacingOccurrences(of: "claude", with: "codex").utf8)
        }
        let path = "/api/v1/project-conversations/project-chat"
        let pagePath = "/api/v1/projects/project/threads"
        let page = codex(projectPage(pinned: true))
        let detail = codex(projectDetail(pinned: true))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: codex(projectCatalog(pinned: true)))
        await model.projects.refresh()
        MessageRecoveryURLProtocol.enqueue(path: pagePath, body: page)
        model.projects.loadThreads("project")
        for _ in 0..<100 {
            if model.projects.threads["project"]?.hasLoaded == true { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        MessageRecoveryURLProtocol.enqueue(path: path, body: detail)
        try await model.projects.loadDetail("project-chat")
        model.editDraft("Keep my unsent words", chat: "project-chat")
        MessageRecoveryURLProtocol.enqueue(path: pagePath, body: page)
        MessageRecoveryURLProtocol.hold(path: pagePath)
        model.projects.loadThreads("project")
        for _ in 0..<100 {
            if MessageRecoveryURLProtocol.bodies(path: pagePath, includingEmpty: true).count >= 2 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        var archived = try XCTUnwrap(JSONSerialization.jsonObject(with: detail) as? [String: Any])
        archived["isArchived"] = true
        MessageRecoveryURLProtocol.enqueue(path: path, method: "PATCH", body: try JSONSerialization.data(withJSONObject: archived))
        try await model.projects.updateConversation("project-chat", fields: ["isArchived": true])
        MessageRecoveryURLProtocol.releaseHeld()
        for _ in 0..<100 {
            if model.projects.threads["project"]?.isLoading == false { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(model.projects.pinned.isEmpty)
        XCTAssertTrue(try XCTUnwrap(model.projects.threads["project"]).threads.isEmpty)
        XCTAssertEqual(model.projects.details["project-chat"]?.isArchived, true)
        XCTAssertEqual(model.composers["project-chat"]?.draft, "Keep my unsent words")

        // A new catalog is authoritative after a restore in Codex Desktop.
        MessageRecoveryURLProtocol.enqueue(path: pagePath, body: page)
        model.projects.loadThreads("project")
        for _ in 0..<100 {
            if model.projects.threads["project"]?.isLoading == false { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        archived["isArchived"] = false
        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONSerialization.data(withJSONObject: archived))
        try await model.projects.loadDetail("project-chat")
        XCTAssertEqual(model.projects.threads["project"]?.threads.first?.reference, "codex:fixture")
        XCTAssertEqual(model.projects.details["project-chat"]?.isArchived, false)
        XCTAssertEqual(model.composers["project-chat"]?.draft, "Keep my unsent words")
    }

    // Contract: bulk archive removes only confirmed successes from every
    // placement, keeps the failed identity selected, and preserves paging and
    // drafts even when an older page completes after the archive response.
    @MainActor func testProjectBulkArchiveRetainsFailuresPagingAndDrafts() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        defer { MessageRecoveryURLProtocol.releaseHeld(); model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        let rows: [[String: Any]] = (0..<3).map { index in
            ["reference": "claude:batch-\(index)", "conversationId": "batch-\(index)", "title": "Thread \(index)",
             "family": "claude", "updatedAt": 3 - index, "isPinned": index == 0, "hasUnread": false, "isWorking": false]
        }
        var catalog = try XCTUnwrap(JSONSerialization.jsonObject(with: projectCatalog(pinned: false)) as? [String: Any])
        catalog["archiveVersion"] = 3
        catalog["pinned"] = [["projectId": "project", "thread": rows[0]]]
        let page = try JSONSerialization.data(withJSONObject: ["threads": rows, "nextCursor": "next-page", "partial": []])
        let pagePath = "/api/v1/projects/project/threads"
        let archivePath = "/api/v1/projects/project/threads/archive"
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: try JSONSerialization.data(withJSONObject: catalog))
        await model.projects.refresh()
        MessageRecoveryURLProtocol.enqueue(path: pagePath, body: page)
        model.projects.loadThreads("project")
        for _ in 0..<100 {
            if model.projects.threads["project"]?.hasLoaded == true { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        let loaded = try XCTUnwrap(model.projects.threads["project"]).threads
        XCTAssertEqual(loaded.count, 3)
        model.editDraft("Keep my unsent draft", chat: "batch-0")
        MessageRecoveryURLProtocol.enqueue(path: pagePath, body: page)
        MessageRecoveryURLProtocol.hold(path: pagePath)
        model.projects.loadThreads("project")
        for _ in 0..<100 {
            if MessageRecoveryURLProtocol.bodies(path: pagePath, includingEmpty: true).count == 2 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        MessageRecoveryURLProtocol.enqueue(path: archivePath, body: Data(#"{"results":[{"reference":"claude:batch-0","conversationId":"batch-0"},{"reference":"claude:batch-1","error":"Still working"},{"reference":"claude:batch-2","conversationId":"batch-2"}]}"#.utf8))
        let selection = ThreadArchiveSelection()
        let targets = loaded.map { ThreadArchiveSelection.Target(host: "host", scope: model.assignmentScope, project: "project", thread: $0) }
        selection.selectAll(targets + [targets[0]]) // Pinned/project duplication is one selection.
        XCTAssertEqual(selection.selected.count, 3)
        selection.requestArchive()
        var archived: [String] = []
        await selection.run(resolve: { _ in model }, archived: { archived.append($0.id) })
        XCTAssertEqual(archived, [targets[0].id, targets[2].id])
        XCTAssertEqual(selection.selected.map(\.id), [targets[1].id])
        XCTAssertEqual(selection.failures.map(\.message), ["Still working"])
        XCTAssertEqual(selection.notice, "Archived 2; 1 could not be archived.")
        XCTAssertTrue(selection.isSelecting)
        XCTAssertTrue(model.projects.pinned.isEmpty)
        XCTAssertEqual(model.projects.threads["project"]?.threads.map(\.reference), ["claude:batch-1"])
        XCTAssertEqual(model.projects.threads["project"]?.nextCursor, "next-page")
        XCTAssertEqual(model.composers["batch-0"]?.draft, "Keep my unsent draft")
        MessageRecoveryURLProtocol.releaseHeld()
        for _ in 0..<100 {
            if model.projects.threads["project"]?.isLoading == false { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(model.projects.threads["project"]?.threads.map(\.reference), ["claude:batch-1"])
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects", includingEmpty: true).count, 1, "Archive must not reload the list")
        let sent = try XCTUnwrap(MessageRecoveryURLProtocol.bodies(path: archivePath).first)
        let request = try XCTUnwrap(JSONSerialization.jsonObject(with: sent) as? [String: Any])
        XCTAssertEqual((request["threads"] as? [[String: Any]])?.compactMap { $0["reference"] as? String }, loaded.map(\.reference))
        // A provider continuation can have been buffered before the archive,
        // even when Show more starts afterward. It cannot resurrect successes.
        MessageRecoveryURLProtocol.enqueue(path: pagePath, body: page)
        model.projects.loadThreads("project", more: true)
        for _ in 0..<100 {
            if model.projects.threads["project"]?.isLoading == false { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(model.projects.threads["project"]?.threads.map(\.reference), ["claude:batch-1"])
        XCTAssertEqual(model.projects.threads["project"]?.nextCursor, "next-page")
        MessageRecoveryURLProtocol.enqueue(path: archivePath, body: Data(#"{"results":[{"reference":"claude:batch-1","conversationId":"batch-1"}]}"#.utf8))
        selection.requestArchive()
        await selection.run(resolve: { _ in model }, archived: { _ in })
        XCTAssertTrue(selection.selected.isEmpty)
        XCTAssertFalse(selection.isSelecting)
        XCTAssertEqual(selection.notice, "Archived 1")
        XCTAssertTrue(try XCTUnwrap(model.projects.threads["project"]).threads.isEmpty)
        XCTAssertEqual(model.projects.threads["project"]?.nextCursor, "next-page")
    }

    @MainActor func testProjectBulkArchiveUsesOrderedSingleWritesOnOlderHosts() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        defer { MessageRecoveryURLProtocol.releaseHeld(); model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        var catalog = try XCTUnwrap(JSONSerialization.jsonObject(with: projectCatalog(pinned: true)) as? [String: Any])
        catalog["archiveVersion"] = 2
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: try JSONSerialization.data(withJSONObject: catalog))
        await model.projects.refresh()
        let first = try XCTUnwrap(model.projects.pinned.first?.thread)
        let second = try JSONDecoder().decode(ProjectThreadSummary.self, from: Data(#"{"reference":"claude:other","conversationId":"other-chat","title":"Other thread","family":"claude","updatedAt":1,"isPinned":false,"hasUnread":false,"isWorking":false}"#.utf8))
        let firstPath = "/api/v1/project-conversations/project-chat"
        let secondPath = "/api/v1/project-conversations/other-chat"
        MessageRecoveryURLProtocol.enqueue(path: firstPath, method: "PATCH", status: 409, body: Data("Still working".utf8), contentType: "text/plain")
        MessageRecoveryURLProtocol.hold(path: firstPath, method: "PATCH")
        var detail = try XCTUnwrap(JSONSerialization.jsonObject(with: projectDetail(pinned: false)) as? [String: Any])
        detail["conversationId"] = "other-chat"; detail["isArchived"] = true
        MessageRecoveryURLProtocol.enqueue(path: secondPath, method: "PATCH", body: try JSONSerialization.data(withJSONObject: detail))
        let pending = Task { try await model.projects.archive("project", threads: [first, second]) }
        for _ in 0..<100 {
            if !MessageRecoveryURLProtocol.bodies(path: firstPath).isEmpty { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: secondPath).isEmpty, "The next write must wait for the first outcome")
        MessageRecoveryURLProtocol.releaseHeld()
        let results = try await pending.value
        XCTAssertEqual(results.map(\.reference), [first.reference, second.reference])
        XCTAssertNotNil(results[0].error)
        XCTAssertNil(results[1].error)
        XCTAssertEqual(model.projects.pinned.map(\.thread.reference), [first.reference])
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: secondPath).count, 1)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects/project/threads/archive").isEmpty)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects", includingEmpty: true).count, 1)
    }

    // Contract: each request is bounded to 50 identities; a malformed or late
    // receipt cannot remove unrelated rows or acknowledge a replaced pairing.
    @MainActor func testProjectBulkArchiveBoundsAndRejectsUntrustedReceipts() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        defer { MessageRecoveryURLProtocol.releaseHeld(); model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        var catalog = try XCTUnwrap(JSONSerialization.jsonObject(with: projectCatalog(pinned: false)) as? [String: Any])
        catalog["archiveVersion"] = 3
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: try JSONSerialization.data(withJSONObject: catalog))
        await model.projects.refresh()
        let rows: [ProjectThreadSummary] = try (0..<51).map { index in
            try JSONDecoder().decode(ProjectThreadSummary.self, from: JSONSerialization.data(withJSONObject: [
                "reference": "claude:batch-\(index)", "conversationId": "batch-\(index)", "title": "Thread",
                "family": "claude", "updatedAt": 1, "isPinned": false, "hasUnread": false, "isWorking": false]))
        }
        let path = "/api/v1/projects/project/threads/archive"
        for batch in [Array(rows.prefix(50)), Array(rows.suffix(1))] {
            MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONSerialization.data(withJSONObject: ["results": batch.map {
                ["reference": $0.reference, "conversationId": $0.conversationId!]
            }]))
        }
        let selection = ThreadArchiveSelection()
        selection.selectAll(rows.map { ThreadArchiveSelection.Target(host: "host", scope: model.assignmentScope, project: "project", thread: $0) })
        selection.requestArchive()
        await selection.run(resolve: { _ in model }, archived: { _ in })
        let requests = try MessageRecoveryURLProtocol.bodies(path: path).map { try XCTUnwrap(JSONSerialization.jsonObject(with: $0) as? [String: Any]) }
        XCTAssertEqual(requests.compactMap { ($0["threads"] as? [Any])?.count }, [50, 1])
        XCTAssertEqual(selection.notice, "Archived 51")
        MessageRecoveryURLProtocol.enqueue(path: path, body: Data(#"{"results":[{"reference":"claude:foreign","conversationId":"foreign"}]}"#.utf8))
        do { _ = try await model.projects.archive("project", threads: [rows[0]]); XCTFail("Accepted another thread's receipt") }
        catch { XCTAssertTrue(error is ReadFailure) }
        let receipt = Data(#"{"results":[{"reference":"claude:batch-0","conversationId":"batch-0"}]}"#.utf8)
        MessageRecoveryURLProtocol.enqueue(path: path, body: receipt)
        MessageRecoveryURLProtocol.hold(path: path, method: "POST")
        let cancelled = Task { try await model.projects.archive("project", threads: [rows[0]]) }
        for _ in 0..<100 {
            if MessageRecoveryURLProtocol.bodies(path: path).count == 4 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        cancelled.cancel()
        MessageRecoveryURLProtocol.releaseHeld()
        do { _ = try await cancelled.value; XCTFail("Acknowledged a cancelled request") } catch {}
        MessageRecoveryURLProtocol.enqueue(path: path, body: receipt)
        _ = try await model.projects.archive("project", threads: [rows[0]]) // No leaked busy reservation.
        MessageRecoveryURLProtocol.enqueue(path: path, body: Data(#"{"results":[{"reference":"claude:batch-0","conversationId":"batch-0"}]}"#.utf8))
        MessageRecoveryURLProtocol.hold(path: path, method: "POST")
        let late = Task { try await model.projects.archive("project", threads: [rows[0]]) }
        for _ in 0..<100 {
            if MessageRecoveryURLProtocol.bodies(path: path).count == 6 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        model.accessEnded = true
        MessageRecoveryURLProtocol.releaseHeld()
        do { _ = try await late.value; XCTFail("Acknowledged an ended pairing") }
        catch { XCTAssertTrue(error is CancellationError) }
    }

    // Contract: old hosts expose no plan controls or mode writes; a response
    // for an ended pairing cannot claim a successful mode save.
    @MainActor func testProjectPlanSaveRequiresCapabilityAndCurrentPairing() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        defer { MessageRecoveryURLProtocol.releaseHeld(); model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        let chat = try Self.cameraChat(id: "project-chat")
        let oldResult = await setPlanMode(true, model: model, library: model.projects, chat: chat)
        XCTAssertFalse(oldResult)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/project-conversations/project-chat").isEmpty)
        try await prepareProject(model)
        let path = "/api/v1/project-conversations/project-chat"
        MessageRecoveryURLProtocol.enqueue(path: path, body: projectDetail(pinned: false, plan: true))
        let enabled = await setPlanMode(true, model: model, library: model.projects, chat: chat)
        XCTAssertTrue(enabled)
        MessageRecoveryURLProtocol.enqueue(path: path, body: projectDetail(pinned: false, plan: false))
        MessageRecoveryURLProtocol.hold(path: path, method: "PATCH")
        let save = Task { await setPlanMode(false, model: model, library: model.projects, chat: chat) }
        for _ in 0..<100 {
            if MessageRecoveryURLProtocol.bodies(path: path).count >= 2 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        model.accessEnded = true
        MessageRecoveryURLProtocol.releaseHeld()
        let staleResult = await save.value
        XCTAssertFalse(staleResult)
        XCTAssertEqual(model.projects.details["project-chat"]?.planMode, true)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/project-chat/messages").isEmpty)
    }

    @MainActor func testSidebarOrderObservationDoesNotPersistActivityOrPublishAgain() throws {
        let suite = "sidebar-order-test-" + UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let store = SidebarOrderStore(defaults: defaults, key: "order")
        let a = SidebarOrderID(host: "mac", item: "a"), b = SidebarOrderID(host: "mac", item: "b")
        func host(_ activity: Int) -> SidebarHostProjects {
            let rows = [ProjectThreadSummary(reference: "a", conversationId: nil, title: "A", family: .codex, updatedAt: activity),
                        ProjectThreadSummary(reference: "b", conversationId: nil, title: "B", family: .codex, updatedAt: 2)]
            var state = ProjectThreadsState()
            state.apply(ProjectThreadsPage(threads: rows, nextCursor: nil, partial: []), replacing: true)
            return SidebarHostProjects(hostID: "mac", name: "Mac", isOnline: true, supportsProjects: true,
                projects: [ProjectSummary(id: "p", name: "P", folders: [])], threads: ["p": state])
        }
        store.observe([host(1)])
        XCTAssertNil(defaults.data(forKey: "order"), "Untouched catalogs never persist recency as manual intent")
        XCTAssertTrue(store.move(a, relativeTo: b, after: true, visible: [a, b], section: .chats, catalogs: [host(1)]))
        let saved = defaults.data(forKey: "order")
        var publications = 0
        let subscription = store.objectWillChange.sink { publications += 1 }
        for activity in 3...20 { store.observe([host(activity)]) }
        XCTAssertEqual(publications, 0, "Activity must not cause another presenter rebuild through the order store")
        XCTAssertEqual(defaults.data(forKey: "order"), saved)
        XCTAssertEqual(store.value.sorted([a, b], in: .chats, id: { $0 }), [b, a])
        withExtendedLifetime(subscription) {}
    }

    // Contract: the same Project ID with new roots must not accept an older
    // in-flight page. Filtering uses this existing catalog owner, not a second cache.
    @MainActor func testSidebarPageRejectsReplacedProjectRoots() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root, hostID: "sidebar-" + UUID().uuidString)
        model.projects.forgetCache()
        defer { MessageRecoveryURLProtocol.releaseHeld(); model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: projectCatalog(pinned: false))
        await model.projects.refresh()
        let path = "/api/v1/projects/project/threads"
        MessageRecoveryURLProtocol.hold(path: path)
        model.projects.loadThreads("project")
        try await waitForRecoveryRequests(path: path, count: 1)
        let newCatalog = String(decoding: projectCatalog(pinned: false), as: UTF8.self).replacingOccurrences(of: "\"rootsRevision\":1", with: "\"rootsRevision\":2")
        let newPage = String(decoding: projectPage(pinned: false), as: UTF8.self).replacingOccurrences(of: "\"title\":\"Thread\"", with: "\"title\":\"New folder\"")
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: Data(newCatalog.utf8))
        // Replies are consumed on release; the new request is released first.
        MessageRecoveryURLProtocol.enqueue(path: path, body: Data(newPage.utf8))
        MessageRecoveryURLProtocol.enqueue(path: path, body: projectPage(pinned: false))
        await model.projects.refresh()
        try await waitForRecoveryRequests(path: path, count: 2)
        MessageRecoveryURLProtocol.releaseNewestHeld()
        for _ in 0..<100 where model.projects.threads["project"]?.hasLoaded != true { try await Task.sleep(for: .milliseconds(10)) }
        MessageRecoveryURLProtocol.releaseHeld()
        await Task.yield()
        XCTAssertEqual(model.projects.projects.first?.rootsRevision, 2)
        XCTAssertEqual(model.projects.threads["project"]?.threads.first?.title, "New folder")
        XCTAssertEqual(model.projects.threads["project"]?.isLoading, false)
    }

    // Contract: a confirmed read clears the thread's unread dot everywhere it
    // shows and after relaunch. Regression: Project chats are absent from the
    // old inbox, so its unread check prevented the actual acknowledgement path.
    @MainActor func testProjectReadClearsUnreadAcrossSidebarAndRelaunch() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        defer { model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        model.setForeground(true)
        await model.loadChats()
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: projectCatalog(pinned: false))
        await model.projects.refresh()
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects/project/threads", body: projectPage(pinned: false, unread: true))
        model.projects.loadThreads("project")
        for _ in 0..<100 {
            if model.projects.threads["project"]?.hasLoaded == true { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/project-chat", body: projectDetail(pinned: false, unread: true))
        let detail = try await model.projects.loadDetail("project-chat")
        let chat = model.projectChat(detail)
        XCTAssertFalse(model.chats.contains { $0.id == chat.id })
        XCTAssertTrue(model.hasUnread(chat.id))
        let path = "/api/v1/conversations/project-chat"
        MessageRecoveryURLProtocol.enqueue(path: path, method: "GET", body: Data(#"{"conversationId":"project-chat","hostEpoch":"epoch","lastSequence":1,"messages":[],"assistantMessages":[],"thread":{"hydrated":true}}"#.utf8))
        await model.open(chat, readOnly: true)
        let visible = try XCTUnwrap(model.readReceipt(for: chat.id))
        MessageRecoveryURLProtocol.enqueue(path: path, method: "PATCH", status: 503)
        await model.acknowledgeVisibleRead(visible)
        XCTAssertTrue(model.hasUnread(chat.id), "A failed read must keep its dot until a retry succeeds")
        MessageRecoveryURLProtocol.enqueue(path: path, method: "PATCH", status: 204, body: Data())
        await model.acknowledgeVisibleRead(visible)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 2)
        XCTAssertFalse(model.projects.hasUnread("project-chat"))
        XCTAssertEqual(model.projects.threads["project"]?.threads.first?.hasUnread, false)
        XCTAssertEqual(model.projects.details["project-chat"]?.hasUnread, false)
        await ProjectCatalogWriter.shared.settle()
        let restarted = recoveryModel(root: root)
        XCTAssertFalse(restarted.projects.hasUnread("project-chat"))

        // Explicit sidebar actions persist without opening a conversation.
        XCTAssertNil(restarted.visibleChat)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/project-chat", method: "PATCH", body: projectDetail(pinned: false, unread: true))
        try await restarted.projects.updateConversation(chat.id, fields: ["hasUnread": true])
        XCTAssertTrue(restarted.hasUnread(chat.id))
        XCTAssertEqual(restarted.projects.threads["project"]?.threads.first?.hasUnread, true)
        await ProjectCatalogWriter.shared.settle()
        XCTAssertTrue(recoveryModel(root: root).projects.hasUnread(chat.id))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/project-chat", method: "PATCH", body: projectDetail(pinned: false, unread: false))
        try await restarted.projects.updateConversation(chat.id, fields: ["hasUnread": false])
        await ProjectCatalogWriter.shared.settle()
        XCTAssertFalse(recoveryModel(root: root).projects.hasUnread(chat.id))
        XCTAssertNil(restarted.visibleChat)

        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/project-chat", method: "PATCH", body: projectDetail(pinned: false, unread: true))
        try await model.projects.updateConversation(chat.id, fields: ["hasUnread": true])
        await model.acknowledgeVisibleRead(visible)
        XCTAssertTrue(model.hasUnread(chat.id), "An explicit unread action on the visible thread must survive automatic reading")
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 2)
        model.projects.noteOpened(chat.id)
        MessageRecoveryURLProtocol.enqueue(path: path, method: "PATCH", status: 204, body: Data())
        await model.acknowledgeVisibleRead(visible)
        XCTAssertFalse(model.hasUnread(chat.id), "Opening the thread again resumes automatic reading")
        model.setForeground(false)
    }

    // Contract: a catalog request started before a read change cannot put back
    // the old dot. The real request/cache boundary owns this refresh race.
    @MainActor func testProjectReadSurvivesOlderSidebarRefresh() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        defer { MessageRecoveryURLProtocol.releaseHeld(); model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: projectCatalog(pinned: false))
        await model.projects.refresh()
        let path = "/api/v1/projects/project/threads"
        MessageRecoveryURLProtocol.enqueue(path: path, body: projectPage(pinned: false, unread: true))
        model.projects.loadThreads("project")
        for _ in 0..<100 {
            if model.projects.threads["project"]?.hasLoaded == true { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/project-chat", body: projectDetail(pinned: false, unread: true))
        try await model.projects.loadDetail("project-chat")
        MessageRecoveryURLProtocol.enqueue(path: path, body: projectPage(pinned: false, unread: true))
        MessageRecoveryURLProtocol.hold(path: path)
        model.projects.loadThreads("project")
        try await waitForRecoveryRequests(path: path, count: 2)
        model.projects.markRead("project-chat")
        MessageRecoveryURLProtocol.releaseHeld()
        for _ in 0..<100 {
            if model.projects.threads["project"]?.isLoading == false { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertFalse(model.hasUnread("project-chat"))
        XCTAssertEqual(model.projects.threads["project"]?.threads.first?.hasUnread, false)
        await ProjectCatalogWriter.shared.settle()
        XCTAssertFalse(recoveryModel(root: root).projects.hasUnread("project-chat"))
    }

    // Even an older host advertising added accounts must receive an own-sign-in
    // creation request. Removing the phone picker must also remove wire routing.
    @MainActor func testNewChatUsesComputerSignInWithOlderHostCapabilities() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let model = recoveryModel(root: root)
        model.projects.forgetCache()
        defer { model.projects.forgetCache(); try? FileManager.default.removeItem(at: root) }
        var codex = ProviderCapabilities.fixtureMac(.codex); codex.accounts = true
        let capabilities = String(decoding: try JSONEncoder().encode(codex), as: UTF8.self)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: Data(#"{"projects":[{"id":"project","name":"Project","isIncluded":true,"isPinned":false,"rootsRevision":1,"folders":[],"createdAt":"fixture"}],"families":[{"family":"codex","available":true,"capabilities":\#(capabilities)}],"modesVersion":1,"pinned":[]}"#.utf8))
        await model.projects.refresh()
        let path = "/api/v1/projects/project/threads"
        MessageRecoveryURLProtocol.enqueue(path: path, method: "POST", body: Data(#"{"conversation":{"reference":"wonder:chat","conversationId":"chat","title":"Chat","family":"codex","updatedAt":2,"isPinned":false,"hasUnread":false,"isWorking":false},"receipt":null}"#.utf8))
        let draft = NewChatDraft(destination: .project(id: "project"), text: "Hi", family: .codex, model: "fixture-model")
        _ = try await model.projects.createThread(projectID: "project", draft: draft, body: "Hi", deviceID: Self.cameraSavedConnection().credential.deviceId)
        let bodies = MessageRecoveryURLProtocol.bodies(path: path)
        XCTAssertEqual(bodies.count, 1)
        let sent = try XCTUnwrap(JSONSerialization.jsonObject(with: try XCTUnwrap(bodies.first)) as? [String: Any])
        XCTAssertEqual(sent["body"] as? String, "Hi")
        XCTAssertNil(sent["accountId"], "New chats always use the computer's sign-in")
    }

    // The catalog is encoded and written off the main thread. A write queued
    // before Forget must not restore the forgotten pairing's catalog, and a
    // burst of saves while a write runs ends with the newest one.
    func testCatalogWriterFencesForgetAndKeepsTheNewestSave() async {
        let writer = ProjectCatalogWriter()
        let key = "wonder.projects.writer-" + UUID().uuidString
        defer { UserDefaults.standard.removeObject(forKey: key) }
        let started = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        writer.schedule(key) { started.signal(); release.wait(); return Data("before forget".utf8) }
        await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in
            DispatchQueue.global().async { started.wait(); done.resume() }
        }
        writer.forget(key)
        release.signal()
        await writer.settle()
        XCTAssertNil(UserDefaults.standard.data(forKey: key), "A write begun before Forget is dropped")

        writer.schedule(key) { started.signal(); release.wait(); return Data("first".utf8) }
        await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in
            DispatchQueue.global().async { started.wait(); done.resume() }
        }
        writer.schedule(key) { Data("second".utf8) }
        writer.schedule(key) { Data("newest".utf8) }
        release.signal()
        await writer.settle()
        XCTAssertEqual(UserDefaults.standard.data(forKey: key), Data("newest".utf8))
    }

    // A Project conversation without a provider session still has a durable
    // conversation ID and must be reachable from the Widget's recent chats.

    @MainActor private func prepareProject(_ model: ConnectionModel) async throws {
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects", body: projectCatalog(pinned: false))
        await model.projects.refresh()
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects/project/threads", body: projectPage(pinned: false))
        model.projects.loadThreads("project")
        for _ in 0..<100 {
            if model.projects.threads["project"]?.hasLoaded == true { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(model.projects.threads["project"]?.hasLoaded, true)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/project-chat", body: projectDetail(pinned: false))
        try await model.projects.loadDetail("project-chat")
    }

    @MainActor func testVoiceEndingWhilePermissionIsPendingCannotMintOrStartAudio() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        var permission: CheckedContinuation<Bool, Never>?
        let controller = VoiceController(model: model, requestPermission: {
            await withCheckedContinuation { permission = $0 }
        })
        controller.start(chat: chat)
        let startup = try XCTUnwrap(controller.startupForTesting)
        for _ in 0..<10_000 {
            if permission != nil { break }
            await Task.yield()
        }
        let continuation = try XCTUnwrap(permission)
        controller.end()
        continuation.resume(returning: true)
        await startup.value
        XCTAssertFalse(controller.isActive)
        XCTAssertEqual(controller.phase, .idle)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/voice/sessions", includingEmpty: true).isEmpty,
                      "Ending voice during a delayed permission result must not mint a new session")
    }

    @MainActor func testVoiceCommandsFenceReplacementPairing() async throws {
        for replacement in [false, true] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let (model, chat) = try recoveryProject(root: root)
            model.voice = VoiceController(model: model, requestPermission: { false })
            model.voice.start(chat: chat)
            let startup = try XCTUnwrap(model.voice.startupForTesting)
            let commands = VoiceAppCommands(model: model)
            if replacement { model.connection = Self.cameraSavedConnection(hostID: "replacement-host") }
            else { model.accessEnded = true }
            await startup.value
            XCTAssertFalse(model.voice.isActive, "Ending the connection's authority must still end its voice session")
            XCTAssertNil(model.voice.chatID)
            do {
                _ = try await commands.perform(.createProject(name: "Must not create"))
                XCTFail("Revoked or replaced voice commands must not act")
            } catch { }
            XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects").isEmpty)
        }
    }

    @MainActor func testVoiceCommandsRefuseStaleApprovalAndUnavailableSendNow() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        let commands = VoiceAppCommands(model: model)
        for command in [VoiceAppCommand.approval(thread: chat.id, request: "expired", choice: "accept", nonce: "expired"),
                        .sendNow(thread: chat.id, clientMessageID: "expired")] {
            do { _ = try await commands.perform(command); XCTFail("A stale command must fail") }
            catch { }
        }
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/approvals/expired/resolve").isEmpty)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/\(chat.id)/queue/expired").isEmpty)
    }

    private func projectDetail(pinned: Bool, plan: Bool = false, unread: Bool = false) -> Data {
        Data(#"{"conversationId":"project-chat","projectId":"project","projectName":"Project","title":"Thread","family":"claude","model":"claude:sonnet","effort":"high","accessMode":"workspace","workingFolder":"/fixture","workingFolderName":"fixture","isPinned":\#(pinned),"hasUnread":\#(unread),"hasNativeSession":true,"folderInProject":true,"claudeApproval":"ask","planMode":\#(plan)}"#.utf8)
    }
    private func projectPage(pinned: Bool, unread: Bool = false) -> Data {
        Data(#"{"threads":[{"reference":"claude:fixture","conversationId":"project-chat","title":"Thread","family":"claude","updatedAt":1,"isPinned":\#(pinned),"hasUnread":\#(unread),"isWorking":false}],"nextCursor":null,"partial":[]}"#.utf8)
    }
    private func projectCatalog(pinned: Bool) -> Data {
        let pins = pinned ? #"[{"projectId":"project","thread":{"reference":"claude:fixture","conversationId":"project-chat","title":"Thread","family":"claude","updatedAt":1,"isPinned":true,"hasUnread":false,"isWorking":false}}]"# : "[]"
        return Data(#"{"projects":[{"id":"project","name":"Project","isIncluded":true,"isPinned":false,"rootsRevision":1,"folders":[],"createdAt":"fixture"}],"families":\#(Self.fixtureFamilies),"modesVersion":1,"pinned":\#(pins)}"#.utf8)
    }

    /// Every provider with the fixture Mac's published record, as catalog JSON.
    private static var fixtureFamilies: String {
        let families = AgentFamily.allCases.map { ["family": $0.rawValue, "available": true, "capabilities": ProviderCapabilities.fixtureMacJSON($0)] }
        return String(decoding: (try? JSONSerialization.data(withJSONObject: families)) ?? Data("[]".utf8), as: UTF8.self)
    }

    @MainActor private func recoveryModel(root: URL, chat: ChatSummary? = nil, hostID: String = "camera-unit-host") -> ConnectionModel {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [MessageRecoveryURLProtocol.self]
        return ConnectionModel(cameraFixtureStoreRoot: root, saved: Self.cameraSavedConnection(hostID: hostID), chat: chat,
            api: PairingAPI(configuration: configuration), replayEnabled: false)
    }

    @MainActor private func recoveryProject(root: URL, family: String = "codex", hostID: String = "camera-unit-host") throws -> (ConnectionModel, ChatSummary) {
        let model = recoveryModel(root: root, hostID: hostID)
        let detail = try JSONDecoder().decode(ProjectConversationDetail.self, from: Data(#"{"conversationId":"recovery-chat","projectId":"project","projectName":"Todo app","title":"Review tasks","family":"\#(family)","model":"fixture-model","accessMode":"workspace","workingFolder":"/fixture","workingFolderName":"Todo app","isPinned":false,"hasUnread":false,"hasNativeSession":true,"folderInProject":true}"#.utf8))
        model.projects.installPreviewFilesConversation(detail)
        let chat = model.projectChat(detail)
        model.chats = [chat]
        model.snapshots[chat.id] = try JSONDecoder().decode(ConversationSnapshot.self, from: Data(#"{"conversationId":"recovery-chat","hostEpoch":"epoch","lastSequence":1,"messages":[],"assistantMessages":[],"thread":{"hydrated":true,"turns":[{"id":"settled-turn","status":"completed","items":[]}]}}"#.utf8))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat/history/activity", body: Data(#"{"latestTurnId":"settled-turn","latestTurnStatus":"completed","runningElsewhere":false}"#.utf8))
        return (model, chat)
    }

    // Contract: a chat the computer answers "not found" for leaves its unsent
    // work as a kept message, fenced from sending, that outlives deselection
    // and relaunch. Regression: the 404 deleted draft, files and outbox.
    @MainActor func testHistoryNotFoundKeepsDraftAttachmentsAndOutbox() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let saved = Self.cameraSavedConnection()
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        var intent = ComposerIntent()
        intent.draft = "Waiting for the computer"
        intent.stagedFiles = [try StagedFile(id: "queued-note", name: "Queued.txt", mimeType: "text/plain", data: Data("Queued note".utf8))]
        try intent.enqueue(device: saved.credential.deviceId, clientMessageID: "queued-message")
        intent.draft = "Still typing"
        intent.stagedFiles = [try StagedFile(id: "draft-note", name: "Draft.txt", mimeType: "text/plain", data: Data("Draft note".utf8))]
        try store.saveComposer(intent, conversation: "recovery-chat")
        let (model, chat) = try recoveryProject(root: root)
        model.presentConversation(chat)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat", status: 404)
        await model.refreshConversation(chat)

        XCTAssertTrue(model.projects.unavailable.contains(chat.id), "The shell must render the missing-chat recovery state, not an empty transcript")
        XCTAssertNil(model.selectedChat, "The vanished chat closes")
        XCTAssertNil(model.composers[chat.id])
        XCTAssertNil(try store.loadIntent(conversation: chat.id), "No chat draft is left for the outbox to drain")
        let kept = try XCTUnwrap(model.orphanedDrafts.first)
        XCTAssertEqual(model.orphanedDrafts.count, 1)
        XCTAssertEqual(kept.conversationId, chat.id)
        XCTAssertEqual(kept.title, "Review tasks")
        XCTAssertEqual(kept.intent.draft, "Still typing")
        XCTAssertEqual(kept.intent.stagedFiles?.map(\.data), [Data("Draft note".utf8)])
        XCTAssertEqual(kept.intent.outbox?.map(\.id), ["queued-message"], "The unsent identity is kept")
        XCTAssertEqual(kept.intent.outbox?.first?.stagedFiles?.map(\.data), [Data("Queued note".utf8)])
        XCTAssertEqual(kept.intent.outbox?.first?.refusal, ConnectionModel.removedChatRefusal)
        XCTAssertNil(kept.intent.nextOutboxMessage(device: saved.credential.deviceId), "Nothing sends it on its own")
        XCTAssertEqual(kept.text, "Waiting for the computer\n\nStill typing")

        let relaunched = recoveryModel(root: root)
        XCTAssertNil(relaunched.selectedChat)
        XCTAssertEqual(relaunched.orphanedDrafts.map(\.id), [kept.id], "Still there with no chat selected, after relaunch")
        relaunched.discardOrphanedDraft(kept.id)
        XCTAssertEqual(relaunched.orphanedDrafts.count, 0)
        XCTAssertEqual(recoveryModel(root: root).orphanedDrafts.count, 0, "Discard is durable")
    }

    // Contract: only an empty composer goes with a vanished chat, and an
    // unreadable saved draft is never deleted.
    @MainActor func testHistoryNotFoundDeletesOnlyEmptyDrafts() async throws {
        let saved = Self.cameraSavedConnection()
        for unreadable in [false, true] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
            let original = Data("{not a draft".utf8)
            if unreadable { try store.saveIntent(original, conversation: "recovery-chat") }
            else { var blank = ComposerIntent(); blank.draft = "  \n"; try store.saveComposer(blank, conversation: "recovery-chat") }
            let (model, chat) = try recoveryProject(root: root)
            let detail = try XCTUnwrap(model.projects.details[chat.id])
            let snapshot = try XCTUnwrap(model.snapshots[chat.id])
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat", status: 404)
            await model.refreshConversation(chat)
            XCTAssertTrue(model.projects.unavailable.contains(chat.id), "An empty missing chat also renders recovery")
            // Detail and history requests overlap on open. A late valid detail
            // cannot turn a history 404 back into an endless loading transcript.
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/recovery-chat", body: try JSONEncoder().encode(detail))
            _ = try await model.projects.loadDetail(chat.id)
            XCTAssertTrue(model.projects.unavailable.contains(chat.id), "History absence wins over a late detail read")
            XCTAssertNil(model.snapshots[chat.id])
            XCTAssertEqual(model.orphanedDrafts.count, 0)
            XCTAssertEqual(try store.loadIntent(conversation: chat.id), unreadable ? original : nil)
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat", body: try JSONEncoder().encode(snapshot))
            await model.refreshConversation(chat)
            XCTAssertFalse(model.projects.unavailable.contains(chat.id), "Only successful history makes the chat available again")
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat", status: 404)
            await model.refreshConversation(chat)
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/recovery-chat", status: 404)
            await model.projects.refreshDetail(chat.id)
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat", body: try JSONEncoder().encode(snapshot))
            await model.refreshConversation(chat)
            XCTAssertTrue(model.projects.unavailable.contains(chat.id), "Successful history cannot erase a detail 404 either")
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/recovery-chat", body: try JSONEncoder().encode(detail))
            _ = try await model.projects.loadDetail(chat.id)
            XCTAssertFalse(model.projects.unavailable.contains(chat.id))
            // Both reads can fail on one open. Recovering either endpoint first
            // must keep the recovery screen until the other endpoint succeeds.
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat", status: 404)
            await model.refreshConversation(chat)
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/recovery-chat", status: 404)
            await model.projects.refreshDetail(chat.id)
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/recovery-chat", body: try JSONEncoder().encode(detail))
            _ = try await model.projects.loadDetail(chat.id)
            XCTAssertTrue(model.projects.unavailable.contains(chat.id), "Detail recovery cannot hide a simultaneous history failure")
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat", body: try JSONEncoder().encode(snapshot))
            await model.refreshConversation(chat)
            XCTAssertFalse(model.projects.unavailable.contains(chat.id), "Both endpoints recovered; the chat opens again")
        }
    }

    // Contract: a file picked for a chat whose saved draft could not be read
    // is refused with a recovery step, and the unreadable bytes stay intact.
    // Regression: the import replaced them with a draft holding only the file.
    @MainActor func testFileImportOverUnreadableDraftKeepsOriginal() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let saved = Self.cameraSavedConnection()
        let chat = try Self.cameraChat(id: "unreadable-import")
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        let original = Data("{not a draft".utf8)
        try store.saveIntent(original, conversation: chat.id)
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat)
        XCTAssertFalse(model.canAttach(chat), "Attach controls are off while the draft is unreadable")
        let note = root.appendingPathComponent("picked.txt")
        try Data("Picked file".utf8).write(to: note)
        await model.stage(note, chat: chat, mime: "text/plain")
        XCTAssertEqual(try store.loadIntent(conversation: chat.id), original)
        XCTAssertEqual(model.controlErrors[chat.id], "Reopen this chat to load its saved draft before attaching a file.")
        XCTAssertNotNil(model.composerErrors[chat.id])
        XCTAssertEqual(model.composers[chat.id]?.attachmentCount, 0)
    }

    private func recoveryQuestion() throws -> AsyncQuestion {
        try JSONDecoder().decode(AsyncQuestion.self, from: Data(#"{"id":"q","conversationId":"recovery-chat","turnId":"turn","itemId":"item","questions":[{"title":"Question?"}],"state":"pending","expiresAtMs":9999999999999}"#.utf8))
    }

    func testInlineMediaCopiesFromEarlierLaunchesAreRemoved() throws {
        let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        let root = temporary.appendingPathComponent("inline-media", isDirectory: true)
        let earlier = root.appendingPathComponent("earlier-launch/a.mp4"), current = root.appendingPathComponent("this-launch/b.mp4")
        let loose = temporary.appendingPathComponent("inline-legacy.mp4"), unrelated = temporary.appendingPathComponent("export.txt")
        for url in [earlier, current, loose, unrelated] {
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data(count: 1_024).write(to: url)
        }
        InlineMediaFiles.removeEarlierLaunches(root: root, temporary: temporary, keeping: "this-launch")
        XCTAssertFalse(FileManager.default.fileExists(atPath: earlier.deletingLastPathComponent().path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: loose.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: current.path), "This launch's copies stay")
        XCTAssertTrue(FileManager.default.fileExists(atPath: unrelated.path))
    }

    @MainActor func testLargeImagePreviewIsDownsampledAndMalformedDataIsRejected() async throws {
        let data=try autoreleasepool {
            let format=UIGraphicsImageRendererFormat(); format.scale=1
            return try XCTUnwrap(UIGraphicsImageRenderer(size:CGSize(width:4000,height:3000),format:format).image { context in
                UIColor.systemBlue.setFill(); context.fill(CGRect(x:0,y:0,width:4000,height:3000))
            }.jpegData(compressionQuality: 0.5))
        }
        let decoded=await Task.detached { ToolPreviewImage.decode(data) }.value
        let image=try XCTUnwrap(decoded)
        XCTAssertEqual(image.size.width,1080); XCTAssertEqual(image.size.height,810)
        XCTAssertNil(ToolPreviewImage.decode(Data("invalid".utf8)))
    }

    @MainActor func testWorkspaceEndpointEncodesConversationSegmentExactlyOnce() {
        let conversationID = "550e8400-e29b-41d4-a716-446655440000/child"
        let endpoint = ConnectionModel.workspaceEndpoint(conversationID: conversationID, operation: "git/diff")
        XCTAssertEqual(
            endpoint,
            "/api/v1/conversations/550e8400%2De29b%2D41d4%2Da716%2D446655440000%2Fchild/workspace/git/diff"
        )
        XCTAssertFalse(endpoint?.contains("%252D") == true)
        XCTAssertFalse(endpoint?.contains("%252F") == true)
    }

    @MainActor func testCameraIgnoresDelayedSessionNotificationsAfterStop() {
        let camera = CameraCaptureController(fixture: .ready(data: Data(), preview: UIImage()))
        camera.start { _ in XCTFail("A stopped camera must not deliver a photo") }
        XCTAssertEqual(camera.state, .ready)
        camera.stop()
        camera.runtimeError()
        camera.retry()
        XCTAssertEqual(camera.state, .checking)
    }

    @MainActor func testSketchRejectsWrongMIMEChangedDestinationAndAttachmentLimit() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("wonder-sketch-reject-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: root) }
        let saved = Self.cameraSavedConnection(), chat = try Self.cameraChat(id: "sketch-reject")
        var initial = ComposerIntent(); initial.draft = "Retain me"
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat, initialIntent: initial)
        let png = UIImage(data: Self.cameraImageData())!.pngData()!
        guard case .failed = await model.stageSketch(Self.cameraImageData(), chat: chat, scope: model.assignmentScope) else {
            return XCTFail("Sketch accepted JPEG bytes as PNG")
        }
        guard case .cancelled = await model.stageSketch(png, chat: chat, scope: "other:pairing") else { return XCTFail("Wrong pairing") }
        model.selectedChat = try Self.cameraChat(id: "another-chat")
        guard case .cancelled = await model.stageSketch(png, chat: chat, scope: model.assignmentScope) else { return XCTFail("Wrong chat") }
        XCTAssertEqual(model.composers[chat.id]?.attachmentCount, 0)
        XCTAssertEqual(model.composers[chat.id]?.draft, "Retain me")
        model.selectedChat = chat
        initial.stagedFiles = try (0..<4).map { try StagedFile(id: "existing-\($0)", name: "file.txt", mimeType: "text/plain", data: Data([65])) }
        let full = ConnectionModel(cameraFixtureStoreRoot: root.appendingPathComponent("full"), saved: saved, chat: chat, initialIntent: initial)
        XCTAssertFalse(full.canAttach(chat))
        XCTAssertEqual(full.sketchUnavailableReason(chat), "Remove an attachment to add a sketch. You can attach up to four files.")
        guard case .failed(let reason) = await full.stageSketch(png, chat: chat, scope: full.assignmentScope) else { return XCTFail("Accepted fifth attachment or misreported a destination change") }
        XCTAssertEqual(reason, full.sketchUnavailableReason(chat))
        XCTAssertEqual(full.composers[chat.id]?.attachmentIDs, initial.attachmentIDs)
    }

    @MainActor func testImagePasteRejectsInvalidOversizedAndExcessImagesAtomically() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let chat = try Self.cameraChat(id: "paste-limits")
        var initial = ComposerIntent(); initial.draft = "Preserved"
        initial.stagedFiles = [try StagedFile(name: "notes.txt", mimeType: "text/plain", data: Data("Notes".utf8))]
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: Self.cameraSavedConnection(), chat: chat, initialIntent: initial)
        let valid = NSItemProvider(item: Self.cameraImageData() as NSData, typeIdentifier: UTType.jpeg.identifier)
        for providers in [
            [valid, NSItemProvider(item: Data("not an image".utf8) as NSData, typeIdentifier: UTType.png.identifier)],
            [NSItemProvider(item: Data(count: 8 * 1024 * 1024 + 1) as NSData, typeIdentifier: UTType.png.identifier)],
            [valid, valid, valid, valid]
        ] {
            await model.stagePastedImages(providers, chat: chat, scope: model.assignmentScope)
            XCTAssertEqual(model.composers[chat.id]?.attachmentIDs, initial.attachmentIDs)
            XCTAssertEqual(model.composers[chat.id]?.draft, initial.draft)
            XCTAssertNotNil(model.controlErrors[chat.id])
            XCTAssertFalse(model.loadingPhotos.contains(chat.id))
        }
    }

    @MainActor func testImagePasteIgnoresNavigationAndCancellationDuringProviderLoad() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let chat = try Self.cameraChat(id: "paste-delayed")
        let other = try Self.cameraChat(id: "other")
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: Self.cameraSavedConnection(), chat: chat)
        let data = Self.cameraImageData()
        for cancel in [false, true] {
            model.selectedChat = chat
            let started = expectation(description: "Provider began")
            let delivery = DelayedPasteDelivery()
            let provider = NSItemProvider()
            provider.registerDataRepresentation(forTypeIdentifier: UTType.jpeg.identifier, visibility: .all) { completion in
                delivery.set(completion)
                started.fulfill()
                return nil
            }
            let task = Task { await model.stagePastedImages([provider], chat: chat, scope: model.assignmentScope) }
            await fulfillment(of: [started], timeout: 5)
            if cancel { task.cancel() } else { model.selectedChat = other }
            delivery.complete(data)
            await task.value
            XCTAssertEqual(model.composers[chat.id]?.attachmentCount, 0)
            XCTAssertNil(model.controlErrors[chat.id])
            XCTAssertFalse(model.loadingPhotos.contains(chat.id))
        }
    }

    // Contract: with maximum-size attachments staged, a draft save writes only
    // small references (the bodies are never re-encoded), and a picked file is
    // read and saved by the persistence worker off the main actor, appearing
    // in the draft only once durable, with text typed meanwhile kept in order.
    @MainActor func testLargeAttachmentDraftPersistsOffTheMainActorWithOrderedCommits() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let saved = Self.cameraSavedConnection()
        let chat = try Self.cameraChat(id: "large-attachment-draft")
        let bodies: [Data] = (0..<3).map { index in
            var body = Data(repeating: 65, count: 8 * 1024 * 1024)
            body[body.count - 1] = 66 + UInt8(index)
            return body
        }
        var initial = ComposerIntent(); initial.draft = "Large attachments"
        initial.stagedFiles = try bodies.enumerated().map {
            try StagedFile(id: "large-\($0.offset)", name: "large-\($0.offset).txt", mimeType: "text/plain", data: $0.element)
        }
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat, initialIntent: initial)
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        XCTAssertLessThan(try XCTUnwrap(store.loadIntent(conversation: chat.id)).count, 8 * 1024)
        let directory = store.stagedBodies(conversation: chat.id).directory
        func bodyURL(_ data: Data) -> URL { directory.appendingPathComponent(ConversationFile.digest(data) + ".body") }
        func identity(_ data: Data) throws -> NSNumber? {
            try FileManager.default.attributesOfItem(atPath: bodyURL(data).path)[.systemFileNumber] as? NSNumber
        }
        let kept = try identity(bodies[1])
        XCTAssertNotNil(kept)

        let removalStart = Date()
        model.removeStaged("large-0", chat: chat.id)
        print("ATTACH_DRAFT staged_bytes=\(2 * bodies[1].count) removal_main_ms=\(Date().timeIntervalSince(removalStart) * 1000)")
        XCTAssertNil(model.controlErrors[chat.id])
        XCTAssertEqual(try identity(bodies[1]), kept, "A save after removal must not rewrite the other bodies")
        XCTAssertFalse(FileManager.default.fileExists(atPath: bodyURL(bodies[0]).path))
        XCTAssertLessThan(try XCTUnwrap(store.loadIntent(conversation: chat.id)).count, 8 * 1024)

        let picked = FileManager.default.temporaryDirectory.appendingPathComponent("picked-\(UUID().uuidString).txt")
        let pickedBody = Data(repeating: 67, count: 8 * 1024 * 1024)
        try pickedBody.write(to: picked)
        defer { try? FileManager.default.removeItem(at: picked) }
        let (gate, hold) = await holdPhotoPersistence()
        defer { gate.signal() }
        let preparing = expectation(description: "Picked file reached the persistence worker")
        model.attachmentPersistenceWillPrepare = { preparing.fulfill() }
        let stage = Task { await model.stage(picked, chat: chat, mime: "text/plain") }
        await fulfillment(of: [preparing], timeout: 10)
        XCTAssertTrue(model.loadingPhotos.contains(chat.id))
        XCTAssertEqual(model.composers[chat.id]?.attachmentIDs, ["large-1", "large-2"], "Not shown before it is durable")
        var heartbeat = 0
        await Task { @MainActor in heartbeat += 1 }.value
        XCTAssertEqual(heartbeat, 1)
        model.editDraft("Typed while the file saved", chat: chat.id)
        gate.signal()
        await hold.value
        await stage.value
        XCTAssertNil(model.controlErrors[chat.id])
        XCTAssertFalse(model.loadingPhotos.contains(chat.id))
        XCTAssertEqual(model.composers[chat.id]?.attachmentCount, 3)
        XCTAssertEqual(try identity(bodies[1]), kept)
        let restarted = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat)
        XCTAssertEqual(restarted.composers[chat.id]?.draft, "Typed while the file saved")
        XCTAssertEqual(restarted.composers[chat.id]?.stagedFiles?.map(\.data), [bodies[1], bodies[2], pickedBody])
        XCTAssertEqual(restarted.composers[chat.id]?.attachmentIDs, model.composers[chat.id]?.attachmentIDs)
    }

    // Contract: staged success follows durable commit, and edits/removal made
    // during persistence are rebased instead of being overwritten by its snapshot.
    @MainActor func testPhotoPersistenceServicesHeartbeatAndPreservesConcurrentDraftChanges() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let saved = Self.cameraSavedConnection()
        let chat = try Self.cameraChat(id: "photo-persistence-edit")
        var initial = ComposerIntent(); initial.draft = "Original draft"
        initial.stagedFiles = [try StagedFile(id: "remove-me", name: "old.txt", mimeType: "text/plain", data: Data("Keep until removed".utf8))]
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat, initialIntent: initial)
        model.snapshots[chat.id] = ConversationSnapshot(conversationId: chat.id, hostEpoch: "fixture", lastSequence: 1,
            messages: [], assistantMessages: [], thread: ThreadProjection(nextCursor: nil, hydrated: true), messageRecords: nil)
        XCTAssertTrue(model.canSend(chat))
        let (gate, hold) = await holdPhotoPersistence()
        defer { gate.signal() }
        let preparing = expectation(description: "Photo reached persistence")
        var preparations = 0
        model.attachmentPersistenceWillPrepare = {
            preparations += 1
            if preparations == 1 { preparing.fulfill() }
        }
        let stage = Task { await model.stageCameraPhoto(Self.cameraImageData(), chat: chat, scope: model.assignmentScope) }
        await fulfillment(of: [preparing], timeout: 5)
        guard model.loadingPhotos.contains(chat.id) else {
            gate.signal()
            await hold.value
            _ = await stage.value
            return XCTFail("Photo bypassed the held persistence worker and published before the main actor could resume")
        }
        XCTAssertTrue(model.loadingPhotos.contains(chat.id))
        XCTAssertEqual(model.composers[chat.id]?.attachmentIDs, initial.attachmentIDs)
        XCTAssertFalse(model.canSend(chat))
        await model.send(chat) // Loading must refuse Send before any network work.
        XCTAssertNil(model.composers[chat.id]?.pending)
        var heartbeat = 0
        await Task { @MainActor in heartbeat += 1 }.value
        XCTAssertEqual(heartbeat, 1)
        model.editDraft("Typed while persistence waited", chat: chat.id)
        model.removeStaged("remove-me", chat: chat.id)
        model.editDraft("Latest text after removing the file", chat: chat.id)
        gate.signal()
        await hold.value
        guard case .attached = await stage.value else { return XCTFail("Photo did not commit after rebasing") }
        XCTAssertEqual(preparations, 2)
        XCTAssertFalse(model.loadingPhotos.contains(chat.id))
        XCTAssertNil(model.controlErrors[chat.id])
        let restarted = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat)
        XCTAssertEqual(restarted.composers[chat.id]?.draft, "Latest text after removing the file")
        XCTAssertEqual(restarted.composers[chat.id]?.attachmentCount, 1)
        XCTAssertFalse(restarted.composers[chat.id]?.attachmentIDs.contains("remove-me") == true)
        XCTAssertEqual(restarted.composers[chat.id]?.attachmentIDs, model.composers[chat.id]?.attachmentIDs)
        XCTAssertNil(restarted.composers[chat.id]?.pending)
        XCTAssertTrue(model.canSend(chat))
    }

    @MainActor func testPhotoPersistenceRejectsCancellationNavigationAndPairingReplacement() async throws {
        // Background continuation is owned by testCameraPhotoKeepsAttachingWhenTheAppGoesInactive.

        for change in ["cancel", "navigate-back", "pairing"] {
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let saved = Self.cameraSavedConnection()
            let chat = try Self.cameraChat(id: "photo-persistence-\(change)")
            var initial = ComposerIntent(); initial.draft = "Prior durable draft"
            initial.stagedFiles = [try StagedFile(name: "old.txt", mimeType: "text/plain", data: Data("Prior file bytes".utf8))]
            let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat, initialIntent: initial)
            let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
            let before = try store.loadIntent(conversation: chat.id)
            let (gate, hold) = await holdPhotoPersistence()
            defer { gate.signal() }
            let preparing = expectation(description: "Photo reached persistence for \(change)")
            model.attachmentPersistenceWillPrepare = { preparing.fulfill() }
            let stage = Task { await model.stageCameraPhoto(Self.cameraImageData(), chat: chat, scope: model.assignmentScope) }
            await fulfillment(of: [preparing], timeout: 10)
            switch change {
            case "cancel":
                model.editDraft("Typed before cancellation", chat: chat.id)
                stage.cancel()
            case "navigate-back":
                model.selectedChat = try Self.cameraChat(id: "elsewhere")
                model.selectedChat = chat
            default: model.connection = Self.cameraSavedConnection(hostID: "replacement-host")
            }
            gate.signal()
            await hold.value
            guard case .cancelled = await stage.value else { return XCTFail("Stale photo was committed for \(change)") }
            XCTAssertFalse(model.loadingPhotos.contains(chat.id))
            XCTAssertNil(model.controlErrors[chat.id])
            XCTAssertEqual(try store.loadIntent(conversation: chat.id), before)
            XCTAssertEqual(try store.loadComposer(conversation: chat.id).draft,
                           change == "cancel" ? "Typed before cancellation" : initial.draft)
            XCTAssertEqual(try store.loadComposer(conversation: chat.id).stagedFiles?.first?.data, initial.stagedFiles?.first?.data)
        }
    }

    @MainActor func testPhotoPersistenceWriteFailureKeepsDraftFilesAndAllowsRetry() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let saved = Self.cameraSavedConnection()
        let chat = try Self.cameraChat(id: "photo-persistence-failure")
        var initial = ComposerIntent(); initial.draft = "Still here"
        initial.stagedFiles = [try StagedFile(name: "old.txt", mimeType: "text/plain", data: Data("Original bytes".utf8))]
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat, initialIntent: initial)
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        let before = try store.loadIntent(conversation: chat.id)
        let (gate, hold) = await holdPhotoPersistence()
        defer { gate.signal() }
        let preparing = expectation(description: "Photo reached persistence")
        model.attachmentPersistenceWillPrepare = { preparing.fulfill() }
        let stage = Task { await model.stageCameraPhoto(Self.cameraImageData(), chat: chat, scope: model.assignmentScope) }
        await fulfillment(of: [preparing], timeout: 5)
        try FileManager.default.setAttributes([.posixPermissions: 0o500], ofItemAtPath: store.directory.path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: store.directory.path) }
        gate.signal()
        await hold.value
        guard case .failed = await stage.value else { return XCTFail("Storage failure reported successful staging") }
        XCTAssertEqual(try store.loadIntent(conversation: chat.id), before)
        XCTAssertEqual(model.composers[chat.id]?.draft, initial.draft)
        XCTAssertEqual(model.composers[chat.id]?.stagedFiles?.first?.data, initial.stagedFiles?.first?.data)
        XCTAssertTrue(model.controlErrors[chat.id]?.contains("storage") == true)
        XCTAssertFalse(model.loadingPhotos.contains(chat.id))
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: store.directory.path)
        model.attachmentPersistenceWillPrepare = nil
        guard case .attached = await model.stageCameraPhoto(Self.cameraImageData(), chat: chat, scope: model.assignmentScope)
        else { return XCTFail("Storage recovery could not retry") }
        let restarted = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat)
        XCTAssertEqual(restarted.composers[chat.id]?.draft, initial.draft)
        XCTAssertEqual(restarted.composers[chat.id]?.attachmentCount, 2)
        XCTAssertEqual(restarted.composers[chat.id]?.stagedFiles?.first?.data, initial.stagedFiles?.first?.data)
        XCTAssertNil(model.controlErrors[chat.id])
    }

    @MainActor func testPhotoPersistenceRebasesTypingWithoutRepeatingLargePreparation() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let saved = Self.cameraSavedConnection()
        let chat = try Self.cameraChat(id: "photo-persistence-typing")
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat)
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        let originalRevision = try store.intentRevision(conversation: chat.id)
        let (gate, hold) = await holdPhotoPersistence()
        defer { gate.signal() }
        let preparing = expectation(description: "Photo reached persistence")
        var preparations = 0
        model.attachmentPersistenceWillPrepare = {
            preparations += 1
            if preparations == 1 { preparing.fulfill() }
        }
        let stage = Task { await model.stageCameraPhoto(Self.cameraImageData(), chat: chat, scope: model.assignmentScope) }
        await fulfillment(of: [preparing], timeout: 5)
        for edit in 0..<80 { model.editDraft("New edit \(edit)", chat: chat.id) }
        XCTAssertEqual(try store.intentRevision(conversation: chat.id), originalRevision,
                       "Typing during preparation must not replace the full intent")
        XCTAssertEqual(try store.loadComposer(conversation: chat.id).draft, "New edit 79")
        XCTAssertEqual(try store.loadComposer(conversation: chat.id).attachmentCount, 0)
        gate.signal()
        await hold.value
        guard case .attached = await stage.value else { return XCTFail("Typing prevented photo persistence") }
        XCTAssertEqual(preparations, 1, "Text does not re-encode or rewrite attachment bodies")
        XCTAssertNil(model.controlErrors[chat.id])
        let restarted = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved, chat: chat)
        XCTAssertEqual(restarted.composers[chat.id]?.draft, "New edit 79")
        XCTAssertEqual(restarted.composers[chat.id]?.attachmentCount, 1)
        XCTAssertEqual(restarted.composers[chat.id]?.attachmentIDs, model.composers[chat.id]?.attachmentIDs)
    }

    @MainActor private func holdPhotoPersistence() async -> (DispatchSemaphore, Task<Void, Never>) {
        let gate = DispatchSemaphore(value: 0)
        let started = expectation(description: "Persistence worker held off main")
        let hold = Task { await ComposerPersistenceWorker.shared.holdPhotoPersistenceForTest(gate, started: started) }
        await fulfillment(of: [started], timeout: 5)
        return (gate, hold)
    }

    @MainActor private static func cameraImageData() -> Data {
        let format = UIGraphicsImageRendererFormat(); format.scale = 1
        return UIGraphicsImageRenderer(size: CGSize(width: 720, height: 960), format: format).jpegData(withCompressionQuality: 0.82) { context in
            UIColor.systemOrange.setFill(); context.fill(CGRect(x: 0, y: 0, width: 720, height: 960))
            UIColor.white.setFill(); context.fill(CGRect(x: 120, y: 170, width: 480, height: 620))
        }
    }

    private static func cameraSavedConnection(hostID: String = "camera-unit-host", expiresAtMs: UInt64? = nil) -> SavedConnection {
        var credential: [String: Any] = [
            "sessionToken": "camera-unit-session", "deviceId": "camera-unit-device", "csrfToken": "camera-unit-csrf",
            "hostInstallationId": hostID
        ]
        credential["expiresAtMs"] = expiresAtMs
        let value: [String: Any] = ["origin": "https://camera-unit.invalid", "credential": credential]
        return try! JSONDecoder().decode(SavedConnection.self, from: JSONSerialization.data(withJSONObject: value))
    }

    private static func cameraChat(id: String, archived: Bool = false) throws -> ChatSummary {
        let value: [String: Any] = ["conversationId": id, "title": id,
                                     "messageCount": 0, "hasUnread": false, "isArchived": archived, "isPinned": false]
        return try JSONDecoder().decode(ChatSummary.self, from: JSONSerialization.data(withJSONObject: value))
    }

    func testJournalRetriesUntilAcknowledgedAndExportsSentBatches() async throws {
        let root=FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let journal=DiagnosticJournal(root:root); let host=UUID().uuidString
        journal.selectHost(host)
        journal.record(DiagnosticEvent(operation:"activity.expand",durationMs:10))
        let first=await journal.pending(host:host)
        XCTAssertEqual(first.count,1)
        let retry=await journal.pending(host:host)
        XCTAssertEqual(first.first?.1,retry.first?.1)
        journal.acknowledge(first[0].0)
        let pending=await journal.pending(host:host); XCTAssertTrue(pending.isEmpty)
        let exported=try await journal.export()
        let values=try JSONSerialization.jsonObject(with:Data(contentsOf:exported)) as? [[String:Any]]
        XCTAssertEqual(values?.count,1)
        XCTAssertFalse(String(data:try Data(contentsOf:exported),encoding:.utf8)!.contains(host))
    }
    func testRetentionAndStorageBudget() async throws {
        let root=FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let host=UUID().uuidString
        let journal=DiagnosticJournal(root:root,maximumBytes:1024,retention:10)
        journal.selectHost(host)
        journal.record(DiagnosticEvent(operation:"history.load",durationMs:1))
        let initial=await journal.pending(host:host)
        XCTAssertFalse(initial.isEmpty)
        try FileManager.default.setAttributes([.modificationDate:Date.distantPast],ofItemAtPath:initial[0].0.path)
        let expired=await journal.pending(host:host)
        XCTAssertTrue(expired.isEmpty)
        for _ in 0..<3 {
            journal.record(DiagnosticEvent(operation:"activity.expand",durationMs:1))
            _=await journal.pending(host:host)
        }
        let retained=await journal.pending(host:host)
        XCTAssertLessThanOrEqual(retained.reduce(0) { $0+$1.1.count },1024)
    }

    /// The real host event payload updates one computer notice without an
    /// attached library, HTTP reader or foreground timer.
    @MainActor func testComputerDialogHostEventsDriveCountAndIgnoreUnrelatedOrInvalidFrames() {
        let push = PushNotifications(read: { _ in nil }, write: { _, _ in }, registerForPush: {})
        func event(_ count: String, category: String = "mac_dialogs") -> ReplayEvent {
            ReplayEvent(eventId: UUID().uuidString, hostEpoch: "epoch", sequence: 1, occurredAt: "now",
                conversationId: nil, event: EventBody(type: "activity", category: category, state: "changed",
                    detail: "{\"count\":\(count)}"), turnId: nil, itemId: nil)
        }
        for count in [3, 2, 1, 0] {
            push.receiveMacDialogEvents([event(String(count))], host: "host", name: "My Mac")
            XCTAssertEqual(push.macDialogs["host"]?.count ?? 0, count)
            push.openMacDialogs(host: "host")
            if count > 0 {
                XCTAssertEqual(push.destination?.host, "host")
                XCTAssertEqual(push.destination?.conversation, "wonder-host-view")
                XCTAssertEqual(push.destination?.macDialog, true, "Dialog attention routes to the host viewer")
            }
            XCTAssertEqual(push.macDialogs["host"]?.count ?? 0, count, "Opening never handles a dialog")
        }
        push.receiveMacDialogEvents([event("3", category: "native_status"), event("-1"), event("257"), event("null")],
                                    host: "host", name: "My Mac")
        XCTAssertTrue(push.macDialogs.isEmpty)
        push.receiveMacDialogEvents([event("1")], host: "other", name: "Other Mac")
        XCTAssertEqual(push.macDialogs["other"]?.count, 1)
        XCTAssertNil(push.macDialogs["host"])
    }

    @MainActor func testMacDialogChipRequiresAnExactWindowSource() throws {
        for id in ["app:UserNotificationCenter#31507", "window:31507"] {
            XCTAssertTrue(ComputerSessionModel.isExactDialogSource(ComputerSource(id: id, name: "Mac dialog", kind: "window")))
        }
        for source in [ComputerSource(id: "display:1", kind: "display"),
                       ComputerSource(id: "app:UserNotificationCenter", kind: "window"),
                       ComputerSource(id: "window:0", kind: "window"),
                       ComputerSource(id: "window:31507", kind: "display")] {
            XCTAssertFalse(ComputerSessionModel.isExactDialogSource(source))
        }
        let target = try XCTUnwrap(ComputerSessionModel.dialogWindowTarget(ComputerSource(id: "app:UserNotificationCenter#31507", name: "Mac dialog", kind: "window")))
        XCTAssertEqual(target.windowID, 31_507)
        XCTAssertEqual(target.app, "UserNotificationCenter")
    }

    @MainActor func testMacDialogChipRejectsAndEndsAWholeScreenSession() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let connection = recoveryModel(root: root), chat = DiagnosticSubagentFixture.parentChat()
        let saved = try XCTUnwrap(connection.connection)
        let old = ComputerSession(id: "old-screen", clientRequestId: "request", ownerDeviceId: saved.credential.deviceId,
            hostInstallationId: saved.credential.hostInstallationId, conversationId: chat.id, generation: 1, state: .preparing,
            source: ComputerSource(id: "display:1", kind: "display"), geometryRevision: 0, failureReason: nil,
            createdAt: "now", updatedAt: "now", lastStateAt: "now", endedAt: nil,
            capability: ComputerCapability(available: true, action: "retry", reason: "Fixture"))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/computer/sessions", method: "POST", body: try JSONEncoder().encode(old))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/computer/sessions/old-screen", method: "DELETE")
        let computer = ComputerSessionModel(model: connection, chat: chat)
        await computer.showMacDialog()
        XCTAssertNil(computer.session)
        XCTAssertFalse(computer.dialogGone)
        XCTAssertEqual(computer.failure, "This dialog's window couldn't be opened. Open it on your computer.")
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/computer/sessions/old-screen", includingEmpty: true).count, 1)
        await computer.close()
    }

    @MainActor func testComputerZoomResetsOnPaneWidthChangeAndSnapsToFit() {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let computer = ComputerSessionModel(model: recoveryModel(root: root), chat: DiagnosticSubagentFixture.parentChat())
        computer.viewportResized(CGSize(width: 400, height: 600))
        XCTAssertEqual(computer.zoomScale, 1)
        computer.magnify(2)
        XCTAssertEqual(computer.zoomScale, 2)
        // The keyboard changes the height only: the zoom stays.
        computer.viewportResized(CGSize(width: 400, height: 300))
        XCTAssertEqual(computer.zoomScale, 2)
        // A pane resize or rotation changes the width: back to Fit.
        computer.viewportResized(CGSize(width: 700, height: 300))
        XCTAssertEqual(computer.zoomScale, 1)
        // Pinching back to nearly the whole picture lands on Fit.
        computer.magnify(1.03)
        XCTAssertEqual(computer.zoomScale, 1)
        UserDefaults.standard.set(true, forKey: "computer.fillsViewport")
        let reopened = ComputerSessionModel(model: recoveryModel(root: root), chat: DiagnosticSubagentFixture.parentChat())
        reopened.viewportResized(CGSize(width: 400, height: 600))
        XCTAssertEqual(reopened.zoomScale, 1, "a saved Fill from an older version no longer zooms the picture")
        XCTAssertNil(UserDefaults.standard.object(forKey: "computer.fillsViewport"))
    }

    func testComputerAppSwitcherHoldsCommandUntilSelectionOrCancellation() {
        var switcher = ComputerAppSwitcher()
        let open: [ComputerInputAction] = [.key(key: "command", phase: "down", modifiers: 8),
                                           .key(key: "tab", phase: "press", modifiers: 8)]
        let release: [ComputerInputAction] = [.key(key: "command", phase: "up", modifiers: 0)]
        XCTAssertEqual(switcher.toggle(), open)
        XCTAssertTrue(switcher.isPresented)
        XCTAssertEqual(switcher.key("tab"), [.key(key: "tab", phase: "press", modifiers: 8)])
        XCTAssertTrue(switcher.isPresented)
        XCTAssertEqual(switcher.key("left"), [.key(key: "left", phase: "press", modifiers: 8)])
        XCTAssertEqual(switcher.toggle(), release)
        XCTAssertFalse(switcher.isPresented)
        XCTAssertTrue(switcher.finish().isEmpty)
        XCTAssertEqual(switcher.toggle(), open)
        XCTAssertEqual(switcher.key("escape"), [.key(key: "escape", phase: "press", modifiers: 8)] + release)
        XCTAssertFalse(switcher.isPresented)
        XCTAssertEqual(switcher.toggle(), open)
        XCTAssertEqual(switcher.key("delete"), [.key(key: "escape", phase: "press", modifiers: 8)] + release
                       + [.key(key: "delete", phase: "press", modifiers: 0)])
        XCTAssertFalse(switcher.isPresented)
        XCTAssertEqual(switcher.toggle(), open)
        XCTAssertEqual(switcher.key("return"), release)
        XCTAssertFalse(switcher.isPresented)
    }

    @MainActor private final class InlinePaneActions: ObservableObject {
        @Published var expanded = false
        var closes = 0
    }

    private struct InlinePaneHarness<Content: View>: View {
        @ObservedObject var actions: InlinePaneActions
        let content: Content
        var body: some View {
            content.environment(\.companionPane, CompanionPaneChrome(
                isExpanded: actions.expanded, showsClose: false,
                toggleExpanded: { actions.expanded.toggle() }, close: { actions.closes += 1 },
                onDisplayWithFold: true))
        }
    }

    @MainActor func testComputerKeyboardCommitsCompositionWithoutDuplicatingMarkedText() {
        var input = ComputerKeyboardComposition()
        XCTAssertEqual(input.update("ni", hasMarkedText: true), [])
        XCTAssertEqual(input.update("你", hasMarkedText: true), [])
        XCTAssertEqual(input.update("你", hasMarkedText: false), [.text("你")])
        XCTAssertEqual(input.update("你", hasMarkedText: false), [])
        XCTAssertEqual(input.update("你好", hasMarkedText: false), [.text("好")])
        XCTAssertEqual(input.update("你", hasMarkedText: false), [.key(key: "delete", phase: "press", modifiers: 0)])
        input.reset()
        XCTAssertEqual(input.update("👨‍👩‍👧‍👦", hasMarkedText: false), [.text("👨‍👩‍👧‍👦")])
        XCTAssertEqual(input.update("", hasMarkedText: false), [.key(key: "delete", phase: "press", modifiers: 0)])
    }

    @MainActor func testComputerInputBufferCoalescesMotionWithoutCrossingClicksOrDragBoundaries() {
        var buffer = ComputerInputBuffer()
        XCTAssertTrue(buffer.append([.pointer(x: 0.1, y: 0.2, phase: "move", button: nil)]))
        XCTAssertTrue(buffer.append([.pointer(x: 0.2, y: 0.3, phase: "move", button: nil)]))
        XCTAssertTrue(buffer.append([.pointer(x: 0.2, y: 0.3, phase: "down", button: "left")]))
        XCTAssertTrue(buffer.append([.pointer(x: 0.3, y: 0.4, phase: "move", button: "left")]))
        XCTAssertTrue(buffer.append([.pointer(x: 0.4, y: 0.5, phase: "move", button: "left")]))
        XCTAssertTrue(buffer.append([.pointer(x: 0.4, y: 0.5, phase: "up", button: "left")]))
        XCTAssertEqual(buffer.popFirst(), [.pointer(x: 0.2, y: 0.3, phase: "move", button: nil)])
        XCTAssertEqual(buffer.popFirst(), [.pointer(x: 0.2, y: 0.3, phase: "down", button: "left")])
        XCTAssertEqual(buffer.popFirst(), [.pointer(x: 0.4, y: 0.5, phase: "move", button: "left")])
        XCTAssertEqual(buffer.popFirst(), [.pointer(x: 0.4, y: 0.5, phase: "up", button: "left")])
        XCTAssertNil(buffer.popFirst())
        // A simulator swipe keeps its finger's newest unsent position, never its lift.
        XCTAssertTrue(buffer.append([.touch(x: 0.5, y: 0.8, phase: "down", delayMs: nil)]))
        XCTAssertTrue(buffer.append([.touch(x: 0.5, y: 0.6, phase: "move", delayMs: nil)]))
        XCTAssertTrue(buffer.append([.touch(x: 0.5, y: 0.4, phase: "move", delayMs: nil)]))
        XCTAssertTrue(buffer.append([.touch(x: 0.5, y: 0.4, phase: "up", delayMs: nil)]))
        XCTAssertTrue(buffer.append([.touch(x: 0.5, y: 0.4, phase: "move", delayMs: nil)]))
        XCTAssertEqual(buffer.popFirst(), [.touch(x: 0.5, y: 0.8, phase: "down", delayMs: nil)])
        XCTAssertEqual(buffer.popFirst(), [.touch(x: 0.5, y: 0.4, phase: "move", delayMs: nil)])
        XCTAssertEqual(buffer.popFirst(), [.touch(x: 0.5, y: 0.4, phase: "up", delayMs: nil)])
        XCTAssertEqual(buffer.popFirst(), [.touch(x: 0.5, y: 0.4, phase: "move", delayMs: nil)])
        XCTAssertNil(buffer.popFirst())
        XCTAssertTrue(buffer.append([.text("hello")]))
        XCTAssertTrue(buffer.append([.text(" world")]))
        XCTAssertTrue(buffer.append([.key(key: "return", phase: "press", modifiers: 0)]))
        XCTAssertTrue(buffer.append([.text("next line")]))
        XCTAssertEqual(buffer.popFirst(), [.text("hello world")])
        XCTAssertEqual(buffer.popFirst(), [.key(key: "return", phase: "press", modifiers: 0)])
        XCTAssertEqual(buffer.popFirst(), [.text("next line")])
    }

    @MainActor func testComputerInputBufferMergesKeyRepeatAndReleasesAHeldKeyPromptly() {
        let down = ComputerInputAction.key(key: "delete", phase: "down", modifiers: 0)
        let up = ComputerInputAction.key(key: "delete", phase: "up", modifiers: 0)
        var buffer = ComputerInputBuffer()
        // A held key's 30 ms repeat while a request is in flight: 5 s is 160 ticks.
        XCTAssertTrue(buffer.append([down]))
        XCTAssertEqual(buffer.popFirst(), [down])
        for _ in 0..<160 { XCTAssertTrue(buffer.append([down])) }
        XCTAssertEqual(buffer.batches.count, 1)
        // Lifting drops the unsent repeat: the up is next, one request behind.
        XCTAssertTrue(buffer.append([up]))
        XCTAssertEqual(buffer.batches, [[up]])
        XCTAssertLessThanOrEqual(buffer.maximumDepth, 2)
        // Before the first down has left, nothing is dropped: the key still goes down, then up.
        buffer.removeAll()
        XCTAssertTrue(buffer.append([down]))
        XCTAssertTrue(buffer.append([down]))
        XCTAssertTrue(buffer.append([up]))
        XCTAssertEqual(buffer.popFirst(), [down])
        XCTAssertEqual(buffer.popFirst(), [up])
        XCTAssertNil(buffer.popFirst())
        // A 30 s hold of Delete over a slow link (250 ms a request, 30 ms repeat): the queue stays at one.
        var held = ComputerInputBuffer()
        var sent = 0
        XCTAssertTrue(held.append([down]))
        for tick in 0..<1_000 {
            if tick % 8 == 0, held.popFirst() != nil { sent += 1 }
            XCTAssertTrue(held.append([down]))
        }
        XCTAssertTrue(held.append([up]))
        XCTAssertLessThanOrEqual(held.maximumDepth, 2)
        XCTAssertLessThanOrEqual(held.batches.count, 1)
        XCTAssertLessThan(sent, 140)
        // A latched modifier's up shares the key-up's batch: the unsent repeat still goes.
        let shiftUp = ComputerInputAction.key(key: "shift", phase: "up", modifiers: 0)
        var latched = ComputerInputBuffer()
        XCTAssertTrue(latched.append([down]))
        XCTAssertEqual(latched.popFirst(), [down])
        for _ in 0..<40 { XCTAssertTrue(latched.append([down])) }
        XCTAssertTrue(latched.append([up, shiftUp]))
        XCTAssertEqual(latched.batches, [[up, shiftUp]])
        XCTAssertLessThanOrEqual(latched.maximumDepth, 2)
        // Another key's repeat is never merged into this one.
        let left = ComputerInputAction.key(key: "left", phase: "down", modifiers: 0)
        XCTAssertTrue(buffer.append([down]))
        XCTAssertTrue(buffer.append([left]))
        XCTAssertEqual(buffer.batches.count, 2)
    }

    @MainActor func testComputerInputBufferRefusesOverflowAndClearsAllPendingWork() {
        var buffer = ComputerInputBuffer()
        for _ in 0..<64 { XCTAssertTrue(buffer.append([.key(key: "delete", phase: "press", modifiers: 0)])) }
        XCTAssertFalse(buffer.append([.text("must not disappear silently")]))
        XCTAssertEqual(buffer.batches.count, 64)
        buffer.removeAll()
        XCTAssertNil(buffer.popFirst())
        XCTAssertTrue(buffer.append([.text("new lease")]))
        XCTAssertEqual(buffer.popFirst(), [.text("new lease")])
    }

    @MainActor func testComputerClipboardSignalingEnvelopeIsOptionalAndKeepsSessionBinding() throws {
        let request = ComputerClipboardPollRequest(
            signaling: ComputerSignalingPollRequest(binding: ComputerSignalingBinding(
                generation: 7, conversationId: "chat", hostInstallationId: "host"), cursor: 2),
            clipboard: ComputerClipboardSubscription(id: "device", cursor: 4))
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(request)) as? [String: Any])
        XCTAssertEqual(json["generation"] as? Int, 7)
        XCTAssertEqual(json["conversationId"] as? String, "chat")
        XCTAssertEqual(json["hostInstallationId"] as? String, "host")
        XCTAssertEqual(json["cursor"] as? Int, 2)
        XCTAssertEqual(json["clipboardSubscriptionId"] as? String, "device")
        XCTAssertEqual(json["clipboardCursor"] as? Int, 4)
        let base = #"{"sessionId":"session","generation":7,"state":"live","captureState":"capturing","peerState":"connected","candidates":[],"cursor":2,"nextCursor":2,"iceServers":[]}"#
        let withoutClipboard = try JSONDecoder().decode(ComputerClipboardPollResponse.self, from: Data(base.utf8))
        XCTAssertNil(withoutClipboard.clipboard)
        var extended = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(base.utf8)) as? [String: Any])
        extended["clipboard"] = ["subscriptionId": "device", "sequence": 5, "status": "watching", "text": "private fixture"]
        let response = try JSONDecoder().decode(ComputerClipboardPollResponse.self,
            from: JSONSerialization.data(withJSONObject: extended))
        XCTAssertEqual(response.signaling.sessionId, "session")
        XCTAssertEqual(response.signaling.generation, 7)
        XCTAssertEqual(response.clipboard?.subscriptionId, "device")
        XCTAssertEqual(response.clipboard?.text, "private fixture")
        // A reply without the field means the Mac cannot share; a helper timeout is transient.
        let message = { ComputerClipboardUpdate.message(for: $0, subscriptionID: "device", hostName: "Mac") }
        XCTAssertEqual(message(withoutClipboard.clipboard), "Clipboard sharing is unavailable on Mac.")
        extended["clipboard"] = ["subscriptionId": "device", "sequence": 5, "status": "unavailable"]
        let transient = try JSONDecoder().decode(ComputerClipboardPollResponse.self,
            from: JSONSerialization.data(withJSONObject: extended))
        XCTAssertFalse(try XCTUnwrap(message(transient.clipboard)).contains("Update"))
        XCTAssertNil(transient.clipboard?.text)
        extended["clipboard"] = ["subscriptionId": "older", "sequence": 5, "status": "unavailable"]
        XCTAssertNil(message(try JSONDecoder().decode(ComputerClipboardPollResponse.self,
            from: JSONSerialization.data(withJSONObject: extended)).clipboard))
    }

    @MainActor func testComputerClipboardWritesBoundedTextToPrivatePhonePasteboard() throws {
        let name = UIPasteboard.Name("computer-clipboard-" + UUID().uuidString)
        let board = try XCTUnwrap(UIPasteboard(name: name, create: true))
        defer { UIPasteboard.remove(withName: name) }
        ComputerPhoneClipboard.write("private fixture", to: board)
        XCTAssertEqual(board.string, "private fixture")
        ComputerPhoneClipboard.write(String(repeating: "é", count: 131_073), to: board)
        XCTAssertEqual(board.string, "private fixture")
        ComputerPhoneClipboard.write("", to: board)
        XCTAssertEqual(board.string, "private fixture")
    }

    /// WebRTC and the receiver's detached poll tasks report state, quality and
    /// the host's report from threads that are not the main actor. The model's
    /// handlers must accept that: a handler that Swift 6 treats as main-actor
    /// isolated traps on the first call (build 215's first frame did exactly this).
    @MainActor func testReceiverCallbacksRunSafelyOffTheMainThread() async throws {
        let chat = try JSONDecoder().decode(ChatSummary.self, from: Data(#"{"conversationId":"offmain-fixture","title":"Off main","messageCount":0,"hasUnread":false,"isArchived":false,"isPinned":false}"#.utf8))
        let connection = ConnectionModel(saved: nil, persistConnection: { _ in })
        let computer = ComputerSessionModel(model: connection, chat: chat)
        let receiver = computer.receiver
        await Task.detached {
            XCTAssertFalse(Thread.isMainThread)
            receiver.onStateChange?(.preparing, 1)
            receiver.onQualityChange?(.good)
            receiver.onHostReportChange?(ComputerHostReport(windowHidden: true), 1)
        }.value
        for _ in 0..<200 where computer.receiverState != .preparing { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertEqual(computer.receiverState, .preparing)
        await computer.close()
    }

    @MainActor func testClosedComputerSessionCannotRestartOrPresentInput() async throws {
        let chat = try JSONDecoder().decode(ChatSummary.self, from: Data(#"{"conversationId":"closed-fixture","title":"Closed fixture","messageCount":0,"hasUnread":false,"isArchived":false,"isPinned":false}"#.utf8))
        let connection = ConnectionModel(saved: nil, persistConnection: { _ in })
        let computer = ComputerSessionModel(model: connection, chat: chat)
        await computer.close()
        await computer.start()
        await computer.retry()
        computer.toggleKeyboard()
        XCTAssertTrue(computer.isClosed)
        XCTAssertEqual(computer.receiverState, .closed)
        XCTAssertFalse(computer.isLoading)
        XCTAssertNil(computer.session)
        XCTAssertFalse(computer.keyboardPresented)
        XCTAssertFalse(computer.controlAvailable)
    }

    @MainActor func testComputerControlLeaseReconciliationNeverRollsSequenceBackward() {
        func lease(id: String = "lease", sequence: UInt64) -> ComputerControlLease {
            ComputerControlLease(
                id: id, sessionId: "session", ownerDeviceId: "phone", hostInstallationId: "mac",
                conversationId: "chat", generation: 1, sourceId: "display:1", geometryRevision: 1,
                status: "active", lastSequence: sequence, acquiredAt: "a", updatedAt: "u",
                expiresAt: "e", releasedAt: nil
            )
        }

        let reconciled = ComputerSessionModel.reconciledLease(
            current: lease(sequence: 7),
            refreshed: lease(sequence: 6)
        )
        XCTAssertEqual(reconciled?.lastSequence, 7)
        XCTAssertNil(ComputerSessionModel.reconciledLease(
            current: lease(sequence: 7),
            refreshed: lease(id: "other", sequence: 8)
        ))
    }

    /// The Apps key opens the launcher; it searches names and opens an app only
    /// while the person controls the computer.
    @MainActor func testAppLauncherSearchesAndOpensOnlyWithControl() async throws {
        let computer = try await controlledFixtureComputer()
        let apps = try await computer.installedApps()
        XCTAssertEqual(ComputerInstalledApp.matching("CALC", in: apps).map(\.name), ["Calculator"])
        XCTAssertEqual(ComputerInstalledApp.matching("  ", in: apps), apps)
        XCTAssertTrue(ComputerInstalledApp.matching("zzz", in: apps).isEmpty)
        XCTAssertEqual(ComputerInstalledApp.matching("code", in: apps).map(\.id), ["com.microsoft.VSCode"])

        let appsKey = try XCTUnwrap(computer.platformControls.rows.flatMap { $0 }.first { $0.id == "applications" })
        computer.keyboardPresented = true
        computer.performControl(appsKey)
        XCTAssertTrue(computer.appLauncherPresented)
        XCTAssertFalse(computer.keyboardPresented, "the launcher replaces the keyboard")

        let calculator = try XCTUnwrap(apps.first { $0.id == "com.apple.calculator" })
        let opened = await computer.openApp(calculator)
        XCTAssertNil(opened)
        XCTAssertEqual(computer.openedAppIDs, ["com.apple.calculator"])
        await computer.done()
        let refused = await computer.openApp(calculator)
        XCTAssertEqual(refused, "Take control to open apps on \(computer.hostShortName).")
        XCTAssertEqual(computer.openedAppIDs, ["com.apple.calculator"])
        await computer.close()
    }

    @MainActor private func controlledFixtureComputer(
        connection: ConnectionModel = ConnectionModel(saved: nil, persistConnection: { _ in })
    ) async throws -> ComputerSessionModel {
        let chat = DiagnosticSubagentFixture.parentChat()
        let session = ComputerSession(id: "fixture-computer-session", clientRequestId: "fixture-request",
            ownerDeviceId: "fixture-device", hostInstallationId: "fixture-host", conversationId: chat.id,
            generation: 1, state: .live,
            source: ComputerSource(id: "display:1", kind: "display", width: 1920, height: 1080, scale: 1),
            geometryRevision: 1, failureReason: nil, createdAt: "a", updatedAt: "u", lastStateAt: "a", endedAt: nil,
            capability: ComputerCapability(available: true, action: "none", reason: "Available"),
            control: ComputerControlCapability(available: true, action: "none", reason: "Available"), supportsControlSourceRebind: true)
        let suite = "computer-control-" + UUID().uuidString
        let preferences = try XCTUnwrap(UserDefaults(suiteName: suite))
        let computer = ComputerSessionModel(model: connection,
                                            chat: chat, inputPreferences: preferences, fixtureSession: session)
        preferences.removePersistentDomain(forName: suite)
        await computer.start()
        computer.takeControl()
        for _ in 0..<300 where !computer.isControlActive { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(computer.isControlActive)
        await computer.awaitInputIdle()
        return computer
    }
}

/// NSItemProvider calls its loader on an arbitrary queue. Synchronize the
/// test-controlled delivery instead of sharing a mutable closure across actors.
private final class DelayedPasteDelivery: @unchecked Sendable {
    private let lock = NSLock()
    private var completion: ((Data?, Error?) -> Void)?
    func set(_ value: @escaping (Data?, Error?) -> Void) {
        lock.lock(); defer { lock.unlock() }
        completion = value
    }
    func complete(_ data: Data) {
        lock.lock()
        let value = completion; completion = nil
        lock.unlock()
        value?(data, nil)
    }
}

private actor ImageLoadProbe {
    let data: Data
    let delay: Duration
    private(set) var calls = 0
    private(set) var maximumActive = 0
    private var active = 0
    init(data: Data, delay: Duration = .milliseconds(20)) { self.data = data; self.delay = delay }
    func load() async throws -> Data {
        calls += 1; active += 1; maximumActive = max(maximumActive, active)
        defer { active -= 1 }
        try await Task.sleep(for: delay)
        return data
    }
}

extension WonderDiagnosticsTests {

    @MainActor func testProjectAccessRapidSelectionsSerializeAndCoalesceToLatestChoice() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root); MessageRecoveryURLProtocol.releaseHeld() }
        let (model, chat, target) = try await approvalFixture(root: root)
        let path = "/api/v1/project-conversations/\(target)"
        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode(accessDetail(.fullAccess)))
        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode(accessDetail(.workspace)))
        MessageRecoveryURLProtocol.hold(path: path)
        model.setProjectAccess(ProjectAccess(accessMode: .fullAccess), conversationID: target)
        let saving = try XCTUnwrap(model.projectAccessTasks[target])
        try await waitForRecoveryRequests(path: path)

        model.setProjectAccess(ProjectAccess(accessMode: .readOnly), conversationID: target)
        model.setProjectAccess(ProjectAccess(accessMode: .fullAccess), conversationID: target)
        model.setProjectAccess(ProjectAccess(accessMode: .workspace), conversationID: target)
        XCTAssertEqual(model.projectAccessChanges[target]?.desired.accessMode, .workspace)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 1)
        MessageRecoveryURLProtocol.releaseHeld()
        await saving.value

        let writes = try MessageRecoveryURLProtocol.bodies(path: path).map {
            try JSONDecoder().decode([String: String].self, from: $0)
        }
        XCTAssertEqual(writes.map { $0["accessMode"] }, ["full_access", "workspace"])
        XCTAssertEqual(model.projects.details[chat.id]?.accessMode.rawValue, ProjectAccessMode.workspace.rawValue)
        XCTAssertNil(model.projectAccessChanges[target])
        XCTAssertTrue(model.canSend(chat))
    }

    @MainActor func testProjectAccessFailureReadsBackConfirmedStateAndKeepsRetryIntent() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat, target) = try await approvalFixture(root: root)
        let path = "/api/v1/project-conversations/\(target)"
        MessageRecoveryURLProtocol.enqueue(path: path, status: 503)
        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode(accessDetail(.readOnly)))
        model.setProjectAccess(ProjectAccess(accessMode: .fullAccess), conversationID: target)
        await model.projectAccessTasks[target]?.value

        let failure = try XCTUnwrap(model.projectAccessChanges[target])
        XCTAssertFalse(failure.saving)
        XCTAssertTrue(failure.reconciled)
        XCTAssertNotNil(failure.failure)
        XCTAssertEqual(failure.desired.accessMode, .fullAccess)
        XCTAssertEqual(model.projects.details[chat.id]?.accessMode.rawValue, ProjectAccessMode.readOnly.rawValue)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count, 2)
        XCTAssertTrue(model.canSend(chat), "Successful readback restores a known execution policy")

        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode(accessDetail(.fullAccess)))
        model.setProjectAccess(failure.desired, conversationID: target)
        await model.projectAccessTasks[target]?.value
        XCTAssertEqual(model.projects.details[chat.id]?.accessMode.rawValue, ProjectAccessMode.fullAccess.rawValue)
        XCTAssertNil(model.projectAccessChanges[target])
    }

    @MainActor func testProjectAccessTimeoutWithConfirmedDesiredReadbackCompletesWithoutError() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat, target) = try await approvalFixture(root: root)
        let path = "/api/v1/project-conversations/\(target)"
        MessageRecoveryURLProtocol.fail(path: path, error: .timedOut)
        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode(accessDetail(.fullAccess)))

        model.setProjectAccess(ProjectAccess(accessMode: .fullAccess), conversationID: target)
        await model.projectAccessTasks[target]?.value

        XCTAssertNil(model.projectAccessChanges[target], "A lost write response is successful when the server confirms the intended mode")
        XCTAssertEqual(model.projects.details[chat.id]?.accessMode.rawValue, ProjectAccessMode.fullAccess.rawValue)
        XCTAssertTrue(model.canSend(chat))
        XCTAssertTrue(model.canGuide(chat))
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 1, "Authoritative readback must avoid a redundant write retry")
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count, 2)
    }

    @MainActor func testProjectAccessAmbiguousFailureBlocksSendAndGuideUntilRetryConfirmsState() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat, target) = try await approvalFixture(root: root)
        let path = "/api/v1/project-conversations/\(target)"
        MessageRecoveryURLProtocol.fail(path: path, error: .timedOut)
        MessageRecoveryURLProtocol.fail(path: path, error: .notConnectedToInternet)
        model.setProjectAccess(ProjectAccess(accessMode: .fullAccess), conversationID: target)
        await model.projectAccessTasks[target]?.value

        let failure = try XCTUnwrap(model.projectAccessChanges[target])
        XCTAssertFalse(failure.saving)
        XCTAssertFalse(failure.reconciled)
        XCTAssertNotNil(failure.failure)
        XCTAssertFalse(model.canSend(chat))
        XCTAssertFalse(model.canGuide(chat))
        await model.send(chat)
        await model.guide(chat)
        XCTAssertNil(model.composers[chat.id]?.pending)
        XCTAssertEqual(model.composers[chat.id]?.draft, "Keep this draft")

        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode(accessDetail(.fullAccess)))
        model.setProjectAccess(failure.desired, conversationID: target)
        await model.projectAccessTasks[target]?.value
        XCTAssertNil(model.projectAccessChanges[target])
        XCTAssertTrue(model.canSend(chat))
        XCTAssertTrue(model.canGuide(chat))
    }

    @MainActor func testProjectAccessChangeDuringDelayedUploadPreventsSendFromBeginning() async throws {
        try await assertApprovalChangeDuringDelayedUploadPreventsExecution(guide: false)
    }

    @MainActor private func assertApprovalChangeDuringDelayedUploadPreventsExecution(guide: Bool) async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root); MessageRecoveryURLProtocol.releaseHeld() }
        let (model, chat, target) = try await approvalFixture(root: root)
        let file = try StagedFile(id: "approval-file", name: "Notes.txt", mimeType: "text/plain", data: Data("Retain this attachment".utf8))
        var draft = try XCTUnwrap(model.composers[chat.id])
        draft.stagedFiles = [file]
        model.composers[chat.id] = draft
        let uploadPath = "/api/v1/conversations/\(chat.id)/files"
        let uploaded: [String: Any] = ["id": "uploaded-approval-file", "name": file.name, "mimeType": file.mimeType,
            "byteSize": file.data.count, "sha256": ConversationFile.digest(file.data), "state": "available", "updatedAt": "2026-09-20T00:00:00Z"]
        MessageRecoveryURLProtocol.enqueue(path: uploadPath, body: try JSONSerialization.data(withJSONObject: uploaded))
        MessageRecoveryURLProtocol.hold(path: uploadPath)
        XCTAssertTrue(guide ? model.canGuide(chat) : model.canSend(chat))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/\(chat.id)", body: try JSONEncoder().encode(accessDetail(.readOnly)))
        let execution = Task { @MainActor in
            if guide { await model.guide(chat) } else { await model.send(chat) }
        }
        try await waitForRecoveryRequests(path: uploadPath)
        XCTAssertTrue(model.uploading.contains(chat.id))

        // The selection happens after Send/Guide passed its initial guard,
        // while attachment upload is suspended. An unknown save result must
        // remain a fence when that earlier action resumes.
        let settingsPath = "/api/v1/project-conversations/\(target)"
        MessageRecoveryURLProtocol.fail(path: settingsPath, error: .timedOut)
        MessageRecoveryURLProtocol.fail(path: settingsPath, error: .notConnectedToInternet)
        model.setProjectAccess(ProjectAccess(accessMode: .fullAccess), conversationID: target)
        await model.projectAccessTasks[target]?.value
        XCTAssertEqual(model.projectAccessChanges[target]?.reconciled, false)
        XCTAssertTrue(model.approvalSettingsBlockSending(chat.id))
        MessageRecoveryURLProtocol.releaseHeld()
        await execution.value

        XCTAssertNil(model.composers[chat.id]?.pending)
        XCTAssertEqual(model.composers[chat.id]?.draft, "Keep this draft")
        XCTAssertEqual(model.composers[chat.id]?.stagedFiles?.first?.uploaded?.id, "uploaded-approval-file")
        XCTAssertEqual(model.composers[chat.id]?.stagedFiles?.first?.data, file.data)
        XCTAssertFalse(model.uploading.contains(chat.id))
        XCTAssertFalse(model.preparingSends.contains(chat.id))
        XCTAssertNil(model.controlErrors[chat.id])
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/\(chat.id)/messages").isEmpty)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/\(chat.id)/turns/active-turn/steer").isEmpty)
    }

    /// An uploaded sketch removed while offline is forgotten when the Mac is
    /// back, alongside the message that waited in the outbox.
    @MainActor func testUploadRemovedOfflineIsForgottenWhenTheOutboxDrains() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        model.macConnected = false
        var draft = ComposerIntent()
        draft.stagedFiles = [try uploadedSketch("offline")]
        model.composers[chat.id] = draft
        let sketchPath = "/api/v1/conversations/\(chat.id)/files/uploaded-offline"
        MessageRecoveryURLProtocol.fail(path: sketchPath, error: .notConnectedToInternet)
        model.removeStaged("offline", chat: chat.id)
        try await waitForRecoveryRequests(path: sketchPath)
        model.editDraft("Sent after the removal", chat: chat.id)
        await model.send(chat)
        let waiting = try XCTUnwrap(model.composers[chat.id]?.outbox)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(model.composers[chat.id]?.removedUploadIds, ["uploaded-offline"])

        MessageRecoveryURLProtocol.enqueue(path: sketchPath, method: "DELETE", status: 204, body: Data())
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat/history/activity", body: Self.outboxActivity)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat",
                                           body: try outboxSnapshot(waiting.map { ($0.id, $0.body) }, sequence: 2))
        model.macConnected = true
        try await waitForOutbox { !model.isDrainingOutbox && model.composers[chat.id]?.outbox == nil }
        XCTAssertEqual(try sentRequests().map(\.body), ["Sent after the removal"])
        try await waitForRecoveryRequests(path: sketchPath, count: 2)
        try await waitForOutbox { model.composers[chat.id]?.removedUploadIds == nil }
    }

    private func uploadedSketch(_ name: String) throws -> StagedFile {
        let bytes = Data([137, 80, 78, 71, 13, 10, 26, 10])
        let uploaded = try JSONDecoder().decode(ConversationFile.self, from: JSONSerialization.data(withJSONObject: [
            "id": "uploaded-" + name, "name": name + ".png", "mimeType": "image/png", "byteSize": bytes.count,
            "sha256": ConversationFile.digest(bytes), "state": "available", "updatedAt": "now"]))
        return try StagedFile(id: name, name: name + ".png", mimeType: "image/png", data: bytes, uploaded: uploaded)
    }

    @MainActor func testProjectAccessHostChangeCancelsPendingSaveAndIgnoresOldResponse() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root); MessageRecoveryURLProtocol.releaseHeld() }
        let (model, chat, target) = try await approvalFixture(root: root)
        let path = "/api/v1/project-conversations/\(target)"
        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode(accessDetail(.fullAccess)))
        MessageRecoveryURLProtocol.hold(path: path)
        model.setProjectAccess(ProjectAccess(accessMode: .fullAccess), conversationID: target)
        let saving = try XCTUnwrap(model.projectAccessTasks[target])
        try await waitForRecoveryRequests(path: path)

        var fields = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(Self.cameraSavedConnection())) as? [String: Any])
        var credential = try XCTUnwrap(fields["credential"] as? [String: Any])
        credential["hostInstallationId"] = "another-unit-host"
        fields["credential"] = credential
        model.connection = try JSONDecoder().decode(SavedConnection.self, from: JSONSerialization.data(withJSONObject: fields))
        XCTAssertTrue(model.projectAccessChanges.isEmpty)
        XCTAssertTrue(model.projectAccessTasks.isEmpty)
        MessageRecoveryURLProtocol.releaseHeld()
        await saving.value
        XCTAssertEqual(model.projects.details[chat.id]?.accessMode.rawValue, ProjectAccessMode.readOnly.rawValue)
        XCTAssertTrue(model.projectAccessChanges.isEmpty)
    }

    @MainActor func testProjectQueueOlderRefreshCannotReplaceConfirmedRevision() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root); MessageRecoveryURLProtocol.releaseHeld() }
        let (model, chat, _) = try await approvalFixture(root: root)
        let path = "/api/v1/conversations/\(chat.id)/queue"
        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode([approvalQueuedMessage(revision: 1)]))
        MessageRecoveryURLProtocol.hold(path: path)
        let refresh = Task { @MainActor in try await model.loadQueue(chat) }
        try await waitForRecoveryRequests(path: path)
        model.queues[chat.id] = [try approvalQueuedMessage(revision: 3)]
        MessageRecoveryURLProtocol.releaseHeld()
        try await refresh.value
        XCTAssertEqual(model.queues[chat.id]?.first?.revision, 3)
        XCTAssertEqual(model.queues[chat.id]?.first?.body, "Queued draft")
    }

    @MainActor private func approvalFixture(root: URL) async throws -> (ConnectionModel, ChatSummary, String) {
        let model = recoveryModel(root: root)
        let chat = try Self.cameraChat(id: "approval-chat")
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/\(chat.id)", body: try JSONEncoder().encode(accessDetail(.readOnly)))
        _ = try await model.projects.loadDetail(chat.id)
        MessageRecoveryURLProtocol.reset()
        model.presentConversation(chat)
        model.snapshots[chat.id] = try JSONDecoder().decode(ConversationSnapshot.self, from: Data(#"{"conversationId":"approval-chat","hostEpoch":"epoch","lastSequence":1,"messages":[],"assistantMessages":[],"thread":{"hydrated":true,"turns":[{"id":"active-turn","status":"inProgress","items":[]}]}}"#.utf8))
        model.editDraft("Keep this draft", chat: chat.id)
        return (model, chat, chat.id)
    }

    private func accessDetail(_ mode: ProjectAccessMode, family: AgentFamily = .codex) throws -> ProjectConversationDetail {
        let fields: [String: Any] = ["conversationId": "approval-chat", "projectId": "project", "projectName": "Project",
            "title": "Access contract", "family": family.rawValue, "model": "ocean", "effort": "medium",
            "accessMode": mode.rawValue, "workingFolder": "/fixture", "workingFolderName": "fixture",
            "isPinned": false, "hasUnread": false, "hasNativeSession": false, "folderInProject": true]
        return try JSONDecoder().decode(ProjectConversationDetail.self, from: JSONSerialization.data(withJSONObject: fields))
    }

    private func approvalQueuedMessage(revision: Int) throws -> QueuedMessage {
        let fields: [String: Any] = ["id": "queued", "clientMessageId": "queued-client", "body": "Queued draft", "revision": revision,
            "attachmentIds": []]
        return try JSONDecoder().decode(QueuedMessage.self, from: JSONSerialization.data(withJSONObject: fields))
    }

    @MainActor private func waitForRecoveryRequests(path: String, count: Int = 1) async throws {
        for _ in 0..<100 {
            if MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count >= count { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("The expected synthetic request did not arrive: \(path)")
    }

    @MainActor func testConnectionRenewalRecoversFromNetworkFailureWithoutPairingAgain() async throws {
        let path = "/api/v1/pairing/session/refresh-challenge"
        for (failure, message) in [(URLError.Code.notConnectedToInternet, "This device is offline"),
                                   (.timedOut, "Cupola is running"), (.cannotFindHost, "Cupola is running"), (.cannotConnectToHost, "Cupola is running")] {
            MessageRecoveryURLProtocol.reset()
            var saved: [SavedConnection] = []
            let model = renewalModel { if let value = $0 { saved.append(value) } }
            MessageRecoveryURLProtocol.fail(path: path, error: failure)
            await model.check(renew: true, userInitiated: true)
            XCTAssertEqual(model.macConnected, false)
            XCTAssertTrue(model.status.contains(message))
            XCTAssertFalse(model.accessEnded)
            XCTAssertEqual(model.connection?.requiresPairing, false)
            XCTAssertEqual(model.connection?.origin, Self.cameraSavedConnection().origin)
            XCTAssertEqual(model.connection?.credential.deviceId, Self.cameraSavedConnection().credential.deviceId)
            XCTAssertFalse(model.busy)
            XCTAssertFalse(model.checkingConnection)
            XCTAssertTrue(saved.isEmpty)

            try enqueueRenewal()
            await model.check(renew: true, userInitiated: true)
            XCTAssertEqual(model.macConnected, true)
            XCTAssertEqual(model.status, "Connected to your computer.")
            XCTAssertFalse(model.accessEnded)
            XCTAssertNil(model.error)
            XCTAssertEqual(saved.count, 1)
            XCTAssertEqual(saved.first?.credential.deviceId, Self.cameraSavedConnection().credential.deviceId)
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path).count, 2)
        }
    }

    @MainActor func testConnectionRenewalDiscardsBackgroundResponseAndRetriesOnResume() async throws {
        MessageRecoveryURLProtocol.reset()
        let path = "/api/v1/pairing/session/refresh-challenge"
        var saves = 0
        let model = renewalModel { _ in saves += 1 }
        try enqueueRenewal()
        MessageRecoveryURLProtocol.hold(path: path)
        defer { MessageRecoveryURLProtocol.releaseHeld() }
        let check = Task { await model.check(renew: true) }
        try await waitForRecoveryRequests(path: path)
        model.setForeground(false)
        MessageRecoveryURLProtocol.releaseHeld()
        await check.value
        XCTAssertNil(model.macConnected)
        XCTAssertEqual(saves, 0, "A backgrounded renewal must not publish or save a stale response")
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/pairing/session").isEmpty)
        try enqueueRenewal()
        await model.check(renew: true)
        XCTAssertEqual(model.macConnected, true)
        XCTAssertEqual(saves, 1)
    }

    @MainActor func testConnectionRenewalRejectsWrongHostWithoutReplacingCredentials() async throws {
        MessageRecoveryURLProtocol.reset()
        let model = renewalModel { _ in XCTFail("A different host must not replace the saved connection") }
        try enqueueRenewal(host: "different-host")
        await model.check(renew: true)
        XCTAssertEqual(model.macConnected, false)
        XCTAssertEqual(model.connection?.credential.hostInstallationId, Self.cameraSavedConnection().credential.hostInstallationId)
        XCTAssertFalse(model.accessEnded)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/pairing/session").isEmpty)
    }

    @MainActor func testCancelledConnectionCheckDoesNotRenewAfterCachePreparation() async throws {
        MessageRecoveryURLProtocol.reset()
        let model = renewalModel { _ in XCTFail("A cancelled check must not replace credentials") }
        let check = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            await model.check(renew: true)
        }
        await check.value
        XCTAssertFalse(model.checkingConnection)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/pairing/session/refresh-challenge", includingEmpty: true).isEmpty)
    }

    // A saved Project can open before the foreground connection check starts.
    // Its history refresh must join renewal instead of leaving Send blocked by a stale failure.
    @MainActor func testColdProjectHistoryWaitsForRenewalBeforeRefreshing() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        // The session restored from disk has run out, so the check renews it.
        let model = renewalModel(root: root, expiresAtMs: UInt64(Date().timeIntervalSince1970 * 1000) - 1) { _ in }
        model.projects.forgetCache()
        defer { model.projects.forgetCache() }
        let detail = try JSONDecoder().decode(ProjectConversationDetail.self, from: projectDetail(pinned: false))
        model.projects.installPreviewFilesConversation(detail)
        let chat = model.projectChat(detail)
        model.macConnected = nil
        try enqueueRenewal()
        let challengePath = "/api/v1/pairing/session/refresh-challenge"
        let historyPath = "/api/v1/conversations/\(chat.id)/history/refresh"
        MessageRecoveryURLProtocol.hold(path: challengePath)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/\(chat.id)", body: projectDetail(pinned: false))
        MessageRecoveryURLProtocol.enqueue(path: historyPath, method: "POST", status: 202,
                                           body: Data(#"{"state":"refreshing"}"#.utf8))
        MessageRecoveryURLProtocol.enqueue(path: historyPath, method: "GET", body: Data(#"{"state":"completed"}"#.utf8))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/\(chat.id)",
                                           body: Data(#"{"conversationId":"project-chat","hostEpoch":"epoch","lastSequence":1,"messages":[],"assistantMessages":[],"thread":{"hydrated":true,"turns":[{"id":"imported","status":"completed","items":[{"id":"reply","type":"agentMessage","state":"completed","text":"Imported reply","createdAt":"2026-10-10T00:00:00Z"}]}]}}"#.utf8))
        let refresh = Task { await model.reloadNativeHistory(chat) }
        try await waitForRecoveryRequests(path: challengePath)
        XCTAssertTrue(model.nativeHistoryRefreshing.contains(chat.id))
        XCTAssertFalse(model.nativeHistoryFailures.contains(chat.id))
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: historyPath).isEmpty)
        MessageRecoveryURLProtocol.releaseHeld()
        await refresh.value
        XCTAssertEqual(model.macConnected, true)
        XCTAssertFalse(model.nativeHistoryFailures.contains(chat.id))
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: historyPath).count, 1)
        XCTAssertEqual(model.snapshots[chat.id]?.conversationId, chat.id)
        XCTAssertEqual(model.snapshots[chat.id]?.rows(author: "Claude").last?.text, "Imported reply")
    }

    // Session expiry (B-019, B-128, B-355). The computer ends each session an
    // hour after it starts and accepts renewal by the device key. Meeting an
    // ended session is not revoked access; only a refused renewal or the
    // computer no longer listing this device is.

    func testSessionRenewalIsDueAheadOfExpiryWithinTheLiveCadence() {
        let now: UInt64 = 1_800_000_000_000
        XCTAssertFalse(SessionRenewal.due(expiresAtMs: nil, nowMs: now), "a session that never expires is not renewed")
        XCTAssertFalse(SessionRenewal.due(expiresAtMs: now + 10 * 60_000, nowMs: now))
        // The old 60 s window fell inside the two-minute refresh cadence.
        XCTAssertTrue(SessionRenewal.due(expiresAtMs: now + 4 * 60_000, nowMs: now))
        XCTAssertTrue(SessionRenewal.due(expiresAtMs: now - 1, nowMs: now), "an ended session is renewed")
        XCTAssertNil(SessionRenewal.delay(expiresAtMs: nil, nowMs: now))
        XCTAssertEqual(SessionRenewal.delay(expiresAtMs: now + 60 * 60_000, nowMs: now), .seconds(55 * 60))
        XCTAssertEqual(SessionRenewal.delay(expiresAtMs: now + 60_000, nowMs: now), .zero)
        XCTAssertEqual(SessionRenewal.delay(expiresAtMs: UInt64.max, nowMs: now), .seconds(3600), "the timer looks again within an hour")
    }

    @MainActor func testRequestAtSessionExpiryRenewsInsteadOfEndingAccess() async throws {
        let challengePath = "/api/v1/pairing/session/refresh-challenge"
        for (renewal, endsAccess) in [("accepted", false), ("refused", true), ("unreachable", false)] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            // No expiry known ahead (or a clock off by minutes): the request finds out.
            let model = renewalModel(root: root) { _ in }
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/devices", status: 401)
            switch renewal {
            case "accepted": try enqueueRenewal(token: "renewed-session")
            case "refused": MessageRecoveryURLProtocol.enqueue(path: challengePath, status: 401)
            default: MessageRecoveryURLProtocol.fail(path: challengePath, error: .timedOut)
            }
            await model.check()
            XCTAssertEqual(model.accessEnded, endsAccess, renewal)
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: challengePath, includingEmpty: true).count, 1, "renewed once: \(renewal)")
            switch renewal {
            case "accepted":
                XCTAssertEqual(model.macConnected, true)
                XCTAssertNil(model.connectionLoss)
                XCTAssertEqual(model.connection?.credential.sessionToken, "renewed-session")
                XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/devices", includingEmpty: true).count, 2,
                               "the refused read runs again on the renewed session")
            case "refused":
                XCTAssertEqual(model.connectionLoss, .accessEnded)
            default:
                XCTAssertEqual(model.macConnected, false)
                XCTAssertNotEqual(model.connectionLoss, .accessEnded, "an unreachable computer did not end access")
                XCTAssertEqual(model.connection?.requiresPairing, false)
            }
        }
    }

    @MainActor func testRefusedReadAtExpiryRenewsAndReadsAgain() async throws {
        let snapshot = #"{"conversationId":"expiry-chat","hostEpoch":"epoch","lastSequence":1,"messages":[],"assistantMessages":[],"thread":{"hydrated":true,"nextCursor":"older"}}"#
        let older = #"{"conversationId":"expiry-chat","hostEpoch":"epoch","lastSequence":1,"messages":[],"assistantMessages":[],"thread":{"hydrated":true}}"#
        let conversation = "/api/v1/conversations/expiry-chat"
        let history = conversation + "/history"
        let challenge = "/api/v1/pairing/session/refresh-challenge"
        // The open chat's read, then a page of earlier messages.
        for read in ["conversation", "older"] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let model = renewalModel(root: root) { _ in }
            let chat = try Self.expiryProjectChat(model)
            if read == "conversation" {
                MessageRecoveryURLProtocol.enqueue(path: conversation, status: 401)
                try enqueueRenewal(token: "renewed-session")
                MessageRecoveryURLProtocol.enqueue(path: conversation, body: Data(snapshot.utf8))
                await model.refreshConversation(chat)
                XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: conversation, includingEmpty: true).count, 2,
                               "the refused read runs again on the renewed session")
                XCTAssertEqual(model.snapshots[chat.id]?.thread.nextCursor, "older", "the chat shows what the second read returned")
                XCTAssertNil(model.conversationLoadFailures[chat.id])
            } else {
                MessageRecoveryURLProtocol.enqueue(path: conversation, body: Data(snapshot.utf8))
                await model.refreshConversation(chat)
                XCTAssertEqual(model.snapshots[chat.id]?.thread.nextCursor, "older")
                MessageRecoveryURLProtocol.enqueue(path: history, status: 401)
                try enqueueRenewal(token: "renewed-session")
                MessageRecoveryURLProtocol.enqueue(path: history, body: Data(older.utf8))
                let failure = await model.loadOlder(chat)
                XCTAssertNil(failure, "earlier messages load after renewal")
                XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: history, includingEmpty: true).count, 2)
                XCTAssertNil(model.snapshots[chat.id]?.thread.nextCursor, "the earlier page was merged")
            }
            XCTAssertFalse(model.accessEnded, "an ended session is not revoked access: \(read)")
            XCTAssertNotEqual(model.connectionLoss, .accessEnded)
            XCTAssertEqual(model.macConnected, true)
            XCTAssertEqual(model.connection?.credential.sessionToken, "renewed-session")
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: challenge, includingEmpty: true).count, 1, read)
        }
    }

    @MainActor private static func expiryProjectChat(_ model: ConnectionModel) throws -> ChatSummary {
        let detail = try JSONDecoder().decode(ProjectConversationDetail.self, from: Data(#"{"conversationId":"expiry-chat","projectId":"project","projectName":"Todo app","title":"Review tasks","family":"codex","model":"fixture-model","accessMode":"workspace","workingFolder":"/fixture","workingFolderName":"Todo app","isPinned":false,"hasUnread":false,"hasNativeSession":true,"folderInProject":true}"#.utf8))
        model.projects.installPreviewFilesConversation(detail)
        let chat = model.projectChat(detail)
        model.chats = [chat]
        return chat
    }

    // A read refused again on the session just renewed for it is not
    // recovered again: no renew, refuse, renew loop, and access stays.
    @MainActor func testReadRefusedAfterRecoveryDoesNotRenewAgain() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }
        let chat = try Self.expiryProjectChat(model)
        try enqueueRenewal(token: "renewed-session")
        for _ in 0..<3 { MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/expiry-chat", status: 401) }
        await model.refreshConversation(chat)
        await model.refreshConversation(chat)
        for _ in 0..<50 { await Task.yield() }
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/pairing/session/refresh-challenge", includingEmpty: true).count, 1)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/expiry-chat", includingEmpty: true).count, 3,
                       "one retry after the renewal, then no more")
        XCTAssertFalse(model.accessEnded)
    }

    // An HTTP timeout must not strand an otherwise live replay and its outbox.
    // No lifecycle reset or explicit Check connection should be needed.
    @MainActor func testLiveFrameReconcilesFailedProbeAndDrainsWaitingMessagesOnce() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        defer { model.retireAfterPairingReplacement() }
        let stream = ReachabilityStreamStandIn()
        model.startDiagnosticReplay(hostEpoch: "epoch") { _ in stream }
        try await waitForOutbox { model.eventStreamLive }
        MessageRecoveryURLProtocol.fail(path: "/api/v1/devices", error: .timedOut)
        await model.check()
        XCTAssertEqual(model.macConnected, false)
        model.editDraft("Waiting while the probe failed", chat: chat.id)
        await model.send(chat)
        let waiting = try XCTUnwrap(model.composers[chat.id]?.outbox?.first)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat/history/activity", body: Self.outboxActivity)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat",
            body: try outboxSnapshot([(waiting.id, waiting.body)], sequence: 2))
        try stream.deliver(sequence: 1)
        try await waitForOutbox { stream.acknowledged == 1 }
        XCTAssertEqual(model.macConnected, true, "current authenticated stream settles the failed auxiliary probe")
        XCTAssertNil(model.composerOfflineNotice)
        // Bound the pre-fix failure without waiting for an outbox that cannot start.
        guard model.macConnected == true else { return }
        try await waitForOutbox { !model.isDrainingOutbox && model.composers[chat.id]?.outbox == nil && model.composers[chat.id]?.pending == nil }
        XCTAssertEqual(try sentRequests().map(\.clientMessageId), [waiting.id])
        try stream.deliver(sequence: 2)
        try await waitForOutbox { stream.acknowledged == 2 }
        XCTAssertEqual(try sentRequests().count, 1, "further activity does not resend accepted work")
    }

    @MainActor func testSupersededExpiredAndRefusedStreamsCannotRestoreReachability() async throws {
        for state in ["superseded", "expired", "refused", "oldRoute", "oldSession", "oldPairing"] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let model = recoveryModel(root: root)
            defer { model.retireAfterPairingReplacement() }
            let stream = ReachabilityStreamStandIn()
            model.startDiagnosticReplay(hostEpoch: "epoch") { _ in stream }
            try await waitForOutbox { model.eventStreamLive }
            let saved = try XCTUnwrap(model.connection)
            if state == "superseded" {
                model.installDiagnosticEventStream(ReachabilityStreamStandIn()) { _ in ReachabilityStreamStandIn() }
            } else if state == "refused" {
                MessageRecoveryURLProtocol.enqueue(path: "/api/v1/devices", body: Data("[]".utf8))
                await model.check()
                XCTAssertTrue(model.accessEnded)
            } else if state == "oldPairing" {
                model.retireAfterPairingReplacement()
            } else {
                var value = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(saved)) as? [String: Any])
                var credential = try XCTUnwrap(value["credential"] as? [String: Any])
                if state == "expired" { credential["expiresAtMs"] = 1 }
                if state == "oldSession" { credential["sessionToken"] = "replacement-session" }
                value["credential"] = credential
                if state == "oldRoute" {
                    value["origin"] = "https://camera-unit.invalid:8443"
                    value["routes"] = [["origin": "https://camera-unit.invalid:8443", "kind": "tailscale"]]
                }
                model.connection = try JSONDecoder().decode(SavedConnection.self, from: JSONSerialization.data(withJSONObject: value))
            }
            MessageRecoveryURLProtocol.hold(path: "/api/v1/devices")
            model.macConnected = false
            try stream.deliver(sequence: 1)
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertEqual(model.macConnected, false, state)
            if state != "superseded" {
                XCTAssertFalse(model.composerReceivesUpdates, "ineligible stream must not suppress the offline notice: \(state)")
            }
            MessageRecoveryURLProtocol.releaseHeld()
        }
    }

    @MainActor func testRefusedSessionWithFailedRenewalCannotBeRevivedByOldFrames() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }
        defer { model.retireAfterPairingReplacement() }
        let stream = ReachabilityStreamStandIn()
        model.startDiagnosticReplay(hostEpoch: "epoch") { _ in stream }
        try await waitForOutbox { model.eventStreamLive }
        try stream.deliver(sequence: 1)
        try await waitForOutbox { stream.acknowledged == 1 }
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/devices", status: 401)
        MessageRecoveryURLProtocol.fail(path: "/api/v1/pairing/session/refresh-challenge", error: .timedOut)
        await model.check()
        XCTAssertEqual(model.macConnected, false)
        XCTAssertFalse(model.accessEnded, "a network miss during renewal is recoverable, not revocation")
        MessageRecoveryURLProtocol.hold(path: "/api/v1/devices")
        try stream.deliver(sequence: 2)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(model.macConnected, false, "the refused session is ineligible until authenticated recovery")
        XCTAssertFalse(model.composerReceivesUpdates)
    }

    @MainActor func testCancelledConnectionCheckCannotRestoreReachability() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        let model = recoveryModel(root: root)
        defer { model.retireAfterPairingReplacement() }
        model.macConnected = false
        MessageRecoveryURLProtocol.hold(path: "/api/v1/devices")
        let check = Task { await model.check() }
        try await waitForRecoveryRequests(path: "/api/v1/devices")
        check.cancel()
        MessageRecoveryURLProtocol.releaseHeld()
        await check.value
        XCTAssertEqual(model.macConnected, false)
    }

    // While the connection runs, the model renews ahead of expiry on its own,
    // schedules the renewed session's renewal, and stops when the app leaves.
    @MainActor func testRenewalTimerRenewsAheadOfExpiryAndStopsInTheBackground() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let now = UInt64(Date().timeIntervalSince1970 * 1000)
        let challenge = "/api/v1/pairing/session/refresh-challenge"
        // Due 300 ms from now (five minutes before its end).
        let model = renewalModel(root: root, expiresAtMs: now + SessionRenewal.marginMs + 300) { _ in }
        // The renewed session is due 400 ms after it arrives; the third never comes.
        try enqueueRenewal(token: "second-session", expiresAtMs: now + SessionRenewal.marginMs + 700)
        try enqueueRenewal(token: "third-session", expiresAtMs: now + SessionRenewal.marginMs + 60 * 60_000)
        model.installDiagnosticEventStream(RenewalStreamStandIn()) { _ in RenewalStreamStandIn() }
        model.setForeground(false)
        model.setForeground(true)
        for _ in 0..<300 where model.connection?.credential.sessionToken != "second-session" { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertEqual(model.connection?.credential.sessionToken, "second-session", "renewed without any check being asked for")
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: challenge, includingEmpty: true).count, 1)
        for _ in 0..<300 where model.connection?.credential.sessionToken != "third-session" { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertEqual(model.connection?.credential.sessionToken, "third-session", "the renewed session was scheduled in turn")
        XCTAssertFalse(model.accessEnded)

        // In the background nothing renews, even when due.
        MessageRecoveryURLProtocol.reset()
        let awayRoot = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: awayRoot) }
        let later = UInt64(Date().timeIntervalSince1970 * 1000)
        let away = renewalModel(root: awayRoot, expiresAtMs: later + SessionRenewal.marginMs + 300) { _ in }
        try enqueueRenewal(token: "background-session", expiresAtMs: later + 60 * 60_000)
        away.installDiagnosticEventStream(RenewalStreamStandIn()) { _ in RenewalStreamStandIn() }
        away.setForeground(false)
        away.setForeground(true)
        away.setForeground(false)
        try await Task.sleep(for: .milliseconds(600))
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: challenge, includingEmpty: true).isEmpty, "the background stops the timer")
        XCTAssertEqual(away.connection?.credential.sessionToken, "camera-unit-session")
    }

    // A time-limited pairing ends at a fixed time; near it, renewal returns
    // that same end. The model does not then renew again and again.
    @MainActor func testRenewalNearAFixedPairingEndDoesNotRepeat() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let end = UInt64(Date().timeIntervalSince1970 * 1000) + 2 * 60_000
        let model = renewalModel(root: root, expiresAtMs: end) { _ in }
        for _ in 0..<5 { try enqueueRenewal(token: "capped-session", expiresAtMs: end) }
        model.installDiagnosticEventStream(RenewalStreamStandIn()) { _ in RenewalStreamStandIn() }
        model.setForeground(false)
        model.setForeground(true)
        try await Task.sleep(for: .milliseconds(500))
        await model.check()
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/pairing/session/refresh-challenge", includingEmpty: true).count, 1,
                       "one renewal, then none until the pairing ends")
        XCTAssertEqual(model.connection?.credential.sessionToken, "capped-session")
        XCTAssertEqual(model.macConnected, true)
        model.setForeground(false)
    }

    // A stream still opening when the session is renewed signed in with the
    // old session and would end with it: it is replaced, not installed.
    @MainActor func testStreamOpenedWithARenewedAwaySessionIsReplaced() async throws {
        MessageRecoveryURLProtocol.reset()
        let model = renewalModel { _ in }
        let opened = RenewalStreamLog()
        let gate = RenewalOpenGate()
        defer { gate.open = true; model.setForeground(false) }
        // Connected, with the read cache prepared (preparing it starts a new read generation).
        await model.check()
        model.startDiagnosticReplay(hostEpoch: "epoch") { saved in
            let stream = RenewalStreamStandIn()
            opened.entries.append((saved.credential.sessionToken, stream))
            while opened.entries.count == 1, !gate.open { try await Task.sleep(for: .milliseconds(10)) }
            return stream
        }
        for _ in 0..<300 where opened.entries.isEmpty { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertEqual(opened.entries.map(\.token), ["camera-unit-session"])
        try enqueueRenewal(token: "renewed-session", expiresAtMs: UInt64(Date().timeIntervalSince1970 * 1000) + 60 * 60_000)
        await model.check(renew: true)
        XCTAssertEqual(model.connection?.credential.sessionToken, "renewed-session")
        gate.open = true
        for _ in 0..<300 where opened.entries.count < 2 || !model.eventStreamLive { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertEqual(opened.entries.map(\.token), ["camera-unit-session", "renewed-session"])
        XCTAssertEqual(opened.entries.first?.stream.closed, true, "the stream on the old session was not kept")
        XCTAssertEqual(opened.entries.last?.stream.closed, false)
        XCTAssertTrue(model.eventStreamLive)
        XCTAssertFalse(model.accessEnded)
    }

    @MainActor func testRenewalMovesTheLiveStreamOntoTheRenewedSession() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = renewalModel(root: root) { _ in }
        let old = RenewalStreamStandIn()
        let opened = RenewalStreamLog()
        model.installDiagnosticEventStream(old) { saved in
            let stream = RenewalStreamStandIn()
            opened.entries.append((saved.credential.sessionToken, stream))
            return stream
        }
        defer { model.setForeground(false) }
        try enqueueRenewal(token: "renewed-session", expiresAtMs: UInt64(Date().timeIntervalSince1970 * 1000) + 60 * 60_000)
        await model.check(renew: true)
        XCTAssertEqual(opened.entries.map { $0.token }, ["renewed-session"], "the replacement stream signs in with the renewed session")
        XCTAssertTrue(old.closed, "the stream on the old session is retired once the new one answered")
        XCTAssertEqual(opened.entries.first?.stream.closed, false)
        XCTAssertTrue(model.eventStreamLive)
        XCTAssertEqual(model.macConnected, true)
        XCTAssertNil(model.connectionLoss)
    }

    @MainActor private func renewalModel(root: URL? = nil, expiresAtMs: UInt64? = nil, persist: @escaping (SavedConnection?) throws -> Void) -> ConnectionModel {
        let key = P256.Signing.PrivateKey()
        let signing = SigningIdentity(read: { key.rawRepresentation }, save: { _ in XCTFail("Renewal must not replace the identity") },
            restore: { _ in EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation,
                                                      sign: { try key.signature(for: $0) }) },
            create: { throw SigningIdentityFailure.missing })
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [MessageRecoveryURLProtocol.self]
        let saved = Self.cameraSavedConnection(expiresAtMs: expiresAtMs)
        if let root {
            return ConnectionModel(cameraFixtureStoreRoot: root, saved: saved,
                                   api: PairingAPI(configuration: configuration), replayEnabled: false, signingIdentity: signing)
        }
        return ConnectionModel(saved: saved, persistConnection: persist,
                               api: PairingAPI(configuration: configuration), signingIdentity: signing)
    }

    private func enqueueRenewal(host: String = "camera-unit-host", token: String? = nil, expiresAtMs: UInt64? = nil) throws {
        let now = UInt64(Date().timeIntervalSince1970 * 1000)
        let challenge: [String: Any] = ["challengeId": "renewal", "deviceId": "camera-unit-device", "nonce": "synthetic",
            "origin": "https://camera-unit.invalid", "hostInstallationId": host, "offerId": "",
            "issuedAtMs": now, "expiresAtMs": now + 60_000]
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/pairing/session/refresh-challenge",
                                           body: try JSONSerialization.data(withJSONObject: challenge))
        var credential: [String: Any] = ["sessionToken": token ?? "camera-unit-session", "deviceId": "camera-unit-device",
            "csrfToken": "camera-unit-csrf", "hostInstallationId": host]
        credential["expiresAtMs"] = expiresAtMs
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/pairing/session",
                                           body: try JSONSerialization.data(withJSONObject: credential))
    }

    @MainActor func testCancelledPairingDoesNotReadIdentityOrConsumeOffer() async throws {
        let signing = SigningIdentity(read: {
            XCTFail("Cancelled enrollment must not read an identity")
            throw SigningIdentityFailure.missing
        }, save: { _ in XCTFail("Cancelled enrollment must not save a key") }, restore: { _ in
            XCTFail("Cancelled enrollment must not restore a key")
            throw SigningIdentityFailure.missing
        }, create: {
            XCTFail("Cancelled enrollment must not create a key")
            throw SigningIdentityFailure.missing
        })
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [PairingDelayedResponse.self]
        let model = ConnectionModel(persistConnection: { _ in XCTFail("Cancelled enrollment must not save credentials") },
                                    api: PairingAPI(configuration: configuration), signingIdentity: signing)
        model.pair(link: "https://pairing-recovery-unit.invalid/pair#offerId=11111111-1111-1111-1111-111111111111&secret=synthetic&hostInstallationId=test-host", address: "", code: "")
        model.cancel()
        for _ in 0..<100 {
            if !model.busy { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertFalse(model.busy)
        XCTAssertNil(model.connection)
        XCTAssertFalse(PairingDelayedResponse.delivery.isWaiting)
        XCTAssertTrue(model.status.contains("Pairing stopped"))
    }

    @MainActor func testReplacedPairingRejectsLateSendReceiptAndPreservesFreshDraft() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let old = SavedConnection(origin: "https://pairing-recovery-unit.invalid", credential: Self.cameraSavedConnection().credential)
        let chat = try Self.cameraChat(id: "pairing-recovery-chat")
        var initial = ComposerIntent(); initial.draft = "Possibly sent before reconnect"
        try initial.begin(device: old.credential.deviceId)
        let request = try XCTUnwrap(initial.pending?.request)
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [PairingDelayedResponse.self]
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: old, chat: chat, initialIntent: initial,
                                    api: PairingAPI(configuration: configuration), replayEnabled: false)
        let delivery = Task { await model.deliver(chat) }
        for _ in 0..<200 {
            if PairingDelayedResponse.delivery.isWaiting { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(PairingDelayedResponse.delivery.isWaiting)
        model.retireAfterPairingReplacement()
        let store = ReadStore(root: root, host: old.credential.hostInstallationId, device: old.storageDeviceId)
        var fresh = ComposerIntent(); fresh.draft = "Typed after new pairing"
        try store.saveComposer(fresh, conversation: chat.id)
        let receipt: [String: Any] = ["clientMessageId": request.clientMessageId, "wonderMessageId": "accepted-old-message",
            "bodySha256": ConversationFile.digest(Data(request.body.utf8)), "conversationId": chat.id, "deliveryState": "accepted_by_wonder"]
        PairingDelayedResponse.delivery.complete(try JSONSerialization.data(withJSONObject: receipt))
        await delivery.value
        XCTAssertEqual(try store.loadComposer(conversation: chat.id).draft, fresh.draft)
        XCTAssertNil(try store.loadComposer(conversation: chat.id).pending)
        model.editDraft("Stale view edit", chat: chat.id)
        XCTAssertEqual(try store.loadComposer(conversation: chat.id).draft, fresh.draft)
        XCTAssertTrue(model.accessEnded)
    }

    // A same-host replacement leaves the old model's host and assignment scope
    // intact. A late folder/model rejection must not reset that draft's retry ID.
    @MainActor func testReplacedPairingIgnoresLateNewChatRejections() async throws {
        let saved = SavedConnection(origin: "https://pairing-recovery-unit.invalid", credential: Self.cameraSavedConnection().credential)
        let host = saved.credential.hostInstallationId
        defer { NewChatDraftStore.remove(host: host); PairingDelayedResponse.delivery.complete(Data(), status: 500) }
        for status in [412, 422] {
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let configuration = URLSessionConfiguration.ephemeral
            configuration.protocolClasses = [PairingDelayedResponse.self]
            let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved,
                                        api: PairingAPI(configuration: configuration), replayEnabled: false)
            var draft = NewChatDraft(destination: .project(id: "project"), text: "Keep this request", family: .codex, model: "model")
            draft.submittedDeviceID = saved.credential.deviceId
            draft.freeze()
            XCTAssertTrue(NewChatDraftStore.save(draft, host: host))
            let attempt = NewChatSendAttempt(scope: model.assignmentScope, hostID: host, requestID: draft.requestID)
            XCTAssertTrue(attempt.applies(to: model, hostID: host, draft: draft))
            let response = Task {
                try await model.projects.createThread(projectID: "project", draft: draft,
                                                      body: "Keep this request", deviceID: saved.credential.deviceId)
            }
            for _ in 0..<200 {
                if PairingDelayedResponse.delivery.isWaiting { break }
                try await Task.sleep(for: .milliseconds(10))
            }
            guard PairingDelayedResponse.delivery.isWaiting else {
                response.cancel()
                XCTFail("The new-chat request did not reach the delayed response")
                continue
            }
            model.retireAfterPairingReplacement()
            XCTAssertEqual(model.assignmentScope, attempt.scope)
            XCTAssertTrue(model.accessEnded)
            PairingDelayedResponse.delivery.complete(Data(), status: status)
            do {
                _ = try await response.value
                XCTFail("Expected HTTP \(status)")
            } catch PairingFailure.response(let code) {
                XCTAssertEqual(code, status)
            } catch {
                XCTFail("Expected HTTP \(status), got \(error)")
            }
            // This is the same response fence the New Chat catch paths use.
            if attempt.applies(to: model, hostID: host, draft: draft) {
                _ = NewChatDraftStore.reject(draft, host: host)
            }
            XCTAssertEqual(NewChatDraftStore.load(host: host)?.requestID, draft.requestID)
            XCTAssertEqual(NewChatDraftStore.savedMessages(host: host).first?.requestID, draft.requestID)
            NewChatDraftStore.remove(host: host)
        }
    }

    // The first request is still waiting at the Mac when the same host is
    // paired again. Its control must unlock before that HTTP request returns.
    @MainActor func testNewChatSendTaskReleasesControlOnPairingReplacement() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root); PairingDelayedResponse.delivery.complete(Data(), status: 500) }
        let saved = SavedConnection(origin: "https://pairing-recovery-unit.invalid", credential: Self.cameraSavedConnection().credential)
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [PairingDelayedResponse.self]
        let model = ConnectionModel(cameraFixtureStoreRoot: root, saved: saved,
                                    api: PairingAPI(configuration: configuration), replayEnabled: false)
        let draft = NewChatDraft(destination: .project(id: "project"), text: "Keep this request", family: .codex, model: "model")
        var send = NewChatSendTask()
        let oldGeneration = send.generation
        let oldRequest = Task {
            _ = try? await model.projects.createThread(projectID: "project", draft: draft,
                                                       body: draft.text, deviceID: saved.credential.deviceId)
        }
        send.start(oldRequest)
        for _ in 0..<200 {
            if PairingDelayedResponse.delivery.isWaiting { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        guard PairingDelayedResponse.delivery.isWaiting else {
            send.cancel()
            XCTFail("The new-chat request did not reach the delayed response")
            return
        }
        model.retireAfterPairingReplacement()
        send.cancel()
        XCTAssertFalse(send.isActive, "The new pairing must release Send without waiting for the old Mac")
        XCTAssertNotEqual(send.generation, oldGeneration)
        let newGeneration = send.generation
        let newRequest = Task { }
        send.start(newRequest)
        XCTAssertTrue(send.isActive)
        PairingDelayedResponse.delivery.complete(Data(), status: 422)
        await oldRequest.value
        XCTAssertFalse(send.finish(oldGeneration), "The old completion must not release the new send")
        XCTAssertTrue(send.isActive)
        await newRequest.value
        XCTAssertTrue(send.finish(newGeneration))
        XCTAssertFalse(send.isActive)
    }

}

private final class PairingDelayedDelivery: @unchecked Sendable {
    private let lock = NSLock()
    private var response: PairingDelayedResponse?
    var isWaiting: Bool { lock.withLock { response != nil } }
    func hold(_ value: PairingDelayedResponse) { lock.withLock { response = value } }
    func complete(_ data: Data, status: Int = 200) {
        let value = lock.withLock { let value = response; response = nil; return value }
        value?.complete(data, status: status)
    }
}

private final class PairingDelayedResponse: URLProtocol, @unchecked Sendable {
    static let delivery = PairingDelayedDelivery()
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "pairing-recovery-unit.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() { Self.delivery.hold(self) }
    override func stopLoading() { }
    func complete(_ data: Data, status: Int = 200) {
        guard let url = request.url, let response = HTTPURLResponse(url: url, statusCode: status, httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"]) else { return }
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: data)
        client?.urlProtocolDidFinishLoading(self)
    }
}

// Identical regression checks run before and after the Release 2 product patch.
extension WonderDiagnosticsTests {

    @MainActor func testRelease2RegressionRejectedAsyncReplyCanBeCorrectedOrSkipped() async throws {
        for status in [400, 413] {
            for skip in [false, true] {
                Release2RegressionURLProtocol.reset()
                let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
                defer { try? FileManager.default.removeItem(at: root) }
                let model = release2RegressionModel(root: root)
                let chat = try Self.cameraChat(id: "recovery-chat")
                let question = try release2RegressionQuestion()
                let store = ReadStore(root: root, host: "camera-unit-host", device: "camera-unit-device")
                let old = AsyncAnswerIntent(answers: [String(repeating: "a", count: 8193)], skip: false)
                try store.saveIntent(JSONEncoder().encode(old), conversation: "async-reply-q")
                model.saveAnswerDraft(["0": "Editable answer"], id: question.id)
                let path = "/api/v1/conversations/recovery-chat/questions/q"
                Release2RegressionURLProtocol.enqueue(path: path, status: status)
                await model.replyAsync(question, chat: chat, answers: ["Ignored during exact retry"], skip: false)
                XCTAssertNil(try store.loadIntent(conversation: "async-reply-q"))
                XCTAssertEqual(model.answerDraft(question.id), ["0": "Editable answer"])
                Release2RegressionURLProtocol.enqueue(path: path, status: 204)
                await model.replyAsync(question, chat: chat, answers: skip ? [] : ["Corrected answer"], skip: skip)
                let bodies = Release2RegressionURLProtocol.bodies(path: path)
                XCTAssertEqual(bodies.count, 2)
                let retried = try JSONDecoder().decode(AsyncAnswerIntent.self, from: XCTUnwrap(bodies.last))
                XCTAssertEqual(retried.skip, skip)
                XCTAssertEqual(retried.answers, skip ? [] : ["Corrected answer"])
                XCTAssertNil(model.attentionErrors[question.id])
            }
        }
    }

    @MainActor private func release2RegressionModel(root: URL) -> ConnectionModel {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [Release2RegressionURLProtocol.self]
        return ConnectionModel(cameraFixtureStoreRoot: root, saved: Self.cameraSavedConnection(),
            api: PairingAPI(configuration: configuration), replayEnabled: false)
    }

    private func release2RegressionQuestion() throws -> AsyncQuestion {
        try JSONDecoder().decode(AsyncQuestion.self, from: Data(#"{"id":"q","conversationId":"recovery-chat","turnId":"turn","itemId":"item","questions":[{"title":"Question?"}],"state":"pending","expiresAtMs":9999999999999}"#.utf8))
    }

}

/// Synthetic, scoped transport. Held requests are released explicitly by tests;
/// no real host, credentials or model execution is involved.
private final class Release2RegressionURLProtocol: URLProtocol, @unchecked Sendable {
    private enum Reply { case response(Int, Data), failure(URLError.Code) }
    private final class State: @unchecked Sendable {
        let lock = NSLock()
        var replies: [String: [Reply]] = [:]
        var requests: [(String, Data?)] = []
        var heldPath: String?
        var held: [() -> Void] = []
    }
    private static let state = State()
    static func reset() {
        releaseHeld()
        state.lock.withLock { state.replies = [:]; state.requests = []; state.heldPath = nil }
    }
    static func enqueue(path: String, status: Int = 200, body: Data = Data("{}".utf8)) {
        state.lock.withLock { state.replies[path, default: []].append(.response(status, body)) }
    }
    static func fail(path: String, error: URLError.Code) {
        state.lock.withLock { state.replies[path, default: []].append(.failure(error)) }
    }
    static func bodies(path: String, includingEmpty: Bool = false) -> [Data] {
        state.lock.withLock { state.requests.filter { $0.0 == path }.compactMap { $0.1 ?? (includingEmpty ? Data() : nil) } }
    }
    static func hold(path: String) { state.lock.withLock { state.heldPath = path } }
    static func releaseHeld() {
        let work = state.lock.withLock { let work = state.held; state.held = []; state.heldPath = nil; return work }
        work.forEach { $0() }
    }
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "camera-unit.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let path = request.url!.path
        let body: Data?
        if let data = request.httpBody { body = data }
        else if let stream = request.httpBodyStream {
            stream.open(); defer { stream.close() }
            var data = Data(), buffer = [UInt8](repeating: 0, count: 4096)
            while stream.hasBytesAvailable {
                let count = stream.read(&buffer, maxLength: buffer.count)
                guard count > 0 else { break }
                data.append(contentsOf: buffer.prefix(count))
            }
            body = data
        } else { body = nil }
        let held = Self.state.lock.withLock {
            Self.state.requests.append((path, body))
            guard Self.state.heldPath == path else { return false }
            Self.state.held.append { [weak self] in self?.respond(path: path, body: body) }
            return true
        }
        if !held { respond(path: path, body: body) }
    }
    override func stopLoading() {}
    private func respond(path: String, body: Data?) {
        let reply: Reply? = Self.state.lock.withLock {
            guard Self.state.replies[path]?.isEmpty == false else { return nil }
            return Self.state.replies[path]?.removeFirst()
        }
        if case .failure(let code) = reply { client?.urlProtocol(self, didFailWithError: URLError(code)); return }
        if case .response(let status, let data) = reply { finish(status: status, body: data); return }
        if path == "/api/v1/devices" {
            finish(status: 200, body: Data(#"[{"id":"camera-unit-device","revokedAt":null}]"#.utf8)); return
        }
        if path == "/api/v1/host/status" {
            finish(status: 200, body: Data(#"{"hostInstallationId":"camera-unit-host"}"#.utf8)); return
        }
        finish(status: 200, body: Data("[]".utf8))
    }
    private func finish(status: Int, body: Data) {
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: nil, headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: body)
        client?.urlProtocolDidFinishLoading(self)
    }
}

/// Synthetic, scoped transport. Held requests are released explicitly by tests;
/// no real host, credentials or model execution is involved.
/// The streams a renewal opened, with the session each signed in with.
@MainActor final class RenewalStreamLog {
    var entries: [(token: String, stream: RenewalStreamStandIn)] = []
}

/// Holds a stand-in stream's opening until a test lets it finish.
@MainActor final class RenewalOpenGate { var open = false }

/// A live stream that answers at once and records whether it was closed.
final class RenewalStreamStandIn: LiveEventStream, @unchecked Sendable {
    private let lock = NSLock()
    private var isClosed = false
    var closed: Bool { lock.withLock { isClosed } }
    func receiveFrame() async throws -> Data {
        try await Task.sleep(for: .seconds(3600))
        throw CancellationError()
    }
    func acknowledge(hostEpoch: String, sequence: UInt64) async throws {}
    func confirmAlive() async throws {}
    func close() { lock.withLock { isClosed = true } }
}

/// Buffered frames exercise the real replay consumer, cursor and acknowledgements.
final class ReachabilityStreamStandIn: LiveEventStream, @unchecked Sendable {
    private let frames = AsyncThrowingStream<Data, Error>.makeStream()
    private let lock = NSLock()
    private var lastAcknowledged: UInt64 = 0
    var acknowledged: UInt64 { lock.withLock { lastAcknowledged } }
    func deliver(sequence: UInt64) throws {
        let event = ReplayEvent(eventId: "reach-\(sequence)", hostEpoch: "epoch", sequence: sequence,
            occurredAt: "1", conversationId: nil, event: EventBody(type: "activity", category: "mac_dialogs"))
        frames.continuation.yield(try JSONEncoder().encode([event]))
    }
    func receiveFrame() async throws -> Data {
        for try await frame in frames.stream { return frame }
        throw CancellationError()
    }
    func acknowledge(hostEpoch: String, sequence: UInt64) async throws { lock.withLock { lastAcknowledged = sequence } }
    func confirmAlive() async throws {}
    func close() { frames.continuation.finish(throwing: CancellationError()) }
}

final class MessageRecoveryURLProtocol: URLProtocol, @unchecked Sendable {
    private enum Reply { case response(Int, Data, String), failure(URLError.Code) }
    private final class State: @unchecked Sendable {
        let lock = NSLock()
        var replies: [String: [Reply]] = [:]
        var requests: [(String, Data?)] = []
        var heldPath: String?
        var heldMethod: String?
        var heldAfter = 0
        var held: [() -> Void] = []
        var offline = false
    }
    private static let state = State()
    static func reset() {
        releaseHeld()
        state.lock.withLock { state.replies = [:]; state.requests = []; state.heldPath = nil; state.heldMethod = nil; state.offline = false }
    }
    /// The synthetic Mac stops answering until `reset`: every request fails as
    /// it would with no network, so a fixture opened offline stays offline.
    static func goOffline() { state.lock.withLock { state.offline = true } }
    static var requestCount: Int { state.lock.withLock { state.requests.count } }
    static func enqueue(path: String, method: String? = nil, status: Int = 200, body: Data = Data("{}".utf8),
                        contentType: String = "application/json") {
        state.lock.withLock { state.replies[method.map { $0 + " " + path } ?? path, default: []].append(.response(status, body, contentType)) }
    }
    static func fail(path: String, method: String? = nil, error: URLError.Code) {
        state.lock.withLock { state.replies[method.map { $0 + " " + path } ?? path, default: []].append(.failure(error)) }
    }
    static func bodies(path: String, includingEmpty: Bool = false) -> [Data] {
        state.lock.withLock { state.requests.filter { $0.0 == path }.compactMap { $0.1 ?? (includingEmpty ? Data() : nil) } }
    }
    static func hold(path: String, method: String? = nil, after: Int = 0) { state.lock.withLock { state.heldPath = path; state.heldMethod = method; state.heldAfter = after } }
    static func releaseHeld() {
        let work = state.lock.withLock { let work = state.held; state.held = []; state.heldPath = nil; state.heldMethod = nil; return work }
        work.forEach { $0() }
    }
    static func releaseNewestHeld() {
        let work = state.lock.withLock { state.held.popLast() }
        work?()
    }
    /// The synthetic Mac and the synthetic account's push service.
    override class func canInit(with request: URLRequest) -> Bool { ["camera-unit.invalid", "push.example.test"].contains(request.url?.host ?? "") }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let path = request.url!.path
        let body: Data?
        if let data = request.httpBody { body = data }
        else if let stream = request.httpBodyStream {
            stream.open(); defer { stream.close() }
            var data = Data(), buffer = [UInt8](repeating: 0, count: 4096)
            while stream.hasBytesAvailable {
                let count = stream.read(&buffer, maxLength: buffer.count)
                guard count > 0 else { break }
                data.append(contentsOf: buffer.prefix(count))
            }
            body = data
        } else { body = nil }
        let held = Self.state.lock.withLock {
            Self.state.requests.append((path, body))
            guard Self.state.heldPath == path, Self.state.heldMethod == nil || Self.state.heldMethod == request.httpMethod else { return false }
            if Self.state.heldAfter > 0 { Self.state.heldAfter -= 1; return false }
            Self.state.held.append { [weak self] in self?.respond(path: path, body: body) }
            return true
        }
        if !held { respond(path: path, body: body) }
    }
    override func stopLoading() {}
    private func respond(path: String, body: Data?) {
        if Self.state.lock.withLock({ Self.state.offline }) {
            client?.urlProtocol(self, didFailWithError: URLError(.notConnectedToInternet)); return
        }
        let reply: Reply? = Self.state.lock.withLock {
            let methodKey = (request.httpMethod ?? "GET") + " " + path
            // A reply queued for "path?query" answers only that query.
            let queried = request.url?.query.map { path + "?" + $0 }
            let key = queried.flatMap { Self.state.replies[$0]?.isEmpty == false ? $0 : nil }
                ?? (Self.state.replies[methodKey]?.isEmpty == false ? methodKey : path)
            guard Self.state.replies[key]?.isEmpty == false else { return nil }
            return Self.state.replies[key]?.removeFirst()
        }
        if case .failure(let code) = reply { client?.urlProtocol(self, didFailWithError: URLError(code)); return }
        if case .response(let status, let data, let contentType) = reply {
            finish(status: status, body: data, contentType: contentType); return
        }
        if path == "/api/v1/conversations/recovery-chat/messages", let body,
           let sent = try? JSONDecoder().decode(SendRequest.self, from: body) {
            let value = ["clientMessageId": sent.clientMessageId, "wonderMessageId": "accepted",
                "bodySha256": ConversationFile.digest(Data(sent.body.utf8)), "conversationId": "recovery-chat", "deliveryState": "accepted_by_wonder"]
            finish(status: 202, body: try! JSONEncoder().encode(value)); return
        }
        if path == "/api/v1/devices" {
            finish(status: 200, body: Data(#"[{"id":"camera-unit-device","revokedAt":null}]"#.utf8)); return
        }
        if path == "/api/v1/host/status" {
            finish(status: 200, body: Data(#"{"hostInstallationId":"camera-unit-host"}"#.utf8)); return
        }
        finish(status: 200, body: Data("[]".utf8))
    }
    private func finish(status: Int, body: Data, contentType: String = "application/json") {
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: nil, headerFields: ["Content-Type": contentType])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: body)
        client?.urlProtocolDidFinishLoading(self)
    }
}

extension WonderDiagnosticsTests {

    @MainActor func testAsyncReplyRestoresPendingSubmissionAndRetriesExactBytes() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let question = try recoveryQuestion()
        let chat = try Self.cameraChat(id: "recovery-chat")
        let store = ReadStore(root: root, host: "camera-unit-host", device: "camera-unit-device")
        let bytes = Data(#"{ "skip" : false, "answers" : ["Original answer"] }"#.utf8)
        try store.saveIntent(bytes, conversation: "async-reply-q")
        let model = recoveryModel(root: root)
        let listPath = "/api/v1/conversations/recovery-chat/questions"
        let replyPath = listPath + "/q"
        MessageRecoveryURLProtocol.enqueue(path: listPath, body: try JSONEncoder().encode([question]))
        await model.loadAsyncQuestions(chat)
        XCTAssertEqual(model.savedAsyncReplies[question.id]?.answers, ["Original answer"])
        XCTAssertTrue(model.retryableAsyncReplies.contains(question.id))
        XCTAssertNotNil(model.attentionErrors[question.id])
        MessageRecoveryURLProtocol.enqueue(path: listPath, body: try JSONEncoder().encode([question]))
        MessageRecoveryURLProtocol.enqueue(path: replyPath, status: 204)
        await model.replyAsync(question, chat: chat, answers: [], skip: true, retry: true)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: replyPath), [bytes])
        XCTAssertNil(try store.loadIntent(conversation: "async-reply-q"))
        XCTAssertNil(model.savedAsyncReplies[question.id])
    }

    @MainActor func testAsyncReplyReconcilesConfirmedTerminalResponseWithoutResubmitting() async throws {
        for state in ["answered", "dismissed"] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: root) }
            let chat = try Self.cameraChat(id: "recovery-chat")
            let answer = AsyncAnswerIntent(answers: state == "answered" ? ["Confirmed reply"] : [], skip: state == "dismissed")
            let terminal = try recoveryQuestionWithState(state, response: answer)
            let store = ReadStore(root: root, host: "camera-unit-host", device: "camera-unit-device")
            try store.saveIntent(JSONEncoder().encode(answer), conversation: "async-reply-q")
            let model = recoveryModel(root: root)
            let listPath = "/api/v1/conversations/recovery-chat/questions"
            MessageRecoveryURLProtocol.enqueue(path: listPath, body: try JSONEncoder().encode([terminal]))
            await model.loadAsyncQuestions(chat)
            XCTAssertNil(try store.loadIntent(conversation: "async-reply-q"))
            XCTAssertNil(model.savedAsyncReplies[terminal.id])
            XCTAssertNil(model.attentionErrors[terminal.id])
            XCTAssertFalse(model.retryableAsyncReplies.contains(terminal.id))
            MessageRecoveryURLProtocol.enqueue(path: listPath, body: try JSONEncoder().encode([terminal]))
            await model.replyAsync(terminal, chat: chat, answers: [], skip: true, retry: true)
            XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: listPath + "/q").isEmpty)
        }
    }

    @MainActor func testAsyncReplyStatusFailureAndMissingQuestionNeverResubmit() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let chat = try Self.cameraChat(id: "recovery-chat")
        let question = try recoveryQuestion()
        let store = ReadStore(root: root, host: "camera-unit-host", device: "camera-unit-device")
        let bytes = try JSONEncoder().encode(AsyncAnswerIntent(answers: ["My reply"], skip: false))
        try store.saveIntent(bytes, conversation: "async-reply-q")
        let model = recoveryModel(root: root)
        let listPath = "/api/v1/conversations/recovery-chat/questions"
        MessageRecoveryURLProtocol.enqueue(path: listPath, body: try JSONEncoder().encode([question]))
        await model.loadAsyncQuestions(chat)
        MessageRecoveryURLProtocol.enqueue(path: listPath, status: 503)
        await model.replyAsync(question, chat: chat, answers: [], skip: true, retry: true)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: listPath + "/q").isEmpty)
        XCTAssertNotNil(model.savedAsyncReplies[question.id])
        MessageRecoveryURLProtocol.enqueue(path: listPath, body: Data("[]".utf8))
        await model.replyAsync(question, chat: chat, answers: [], skip: true, retry: true)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: listPath + "/q").isEmpty)
        XCTAssertEqual(model.asyncQuestions[chat.id]?.map(\.id), [question.id])
        XCTAssertNotNil(model.attentionErrors[question.id])
        XCTAssertFalse(model.retryableAsyncReplies.contains(question.id))
        XCTAssertEqual(try store.loadIntent(conversation: "async-reply-q"), bytes)
    }

    private func recoveryQuestionWithState(_ state: String, response: AsyncAnswerIntent?) throws -> AsyncQuestion {
        var value = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(recoveryQuestion())) as? [String: Any])
        value["state"] = state
        if state == "expired" { value["expiresAtMs"] = 1 }
        if let response { value["response"] = try JSONSerialization.jsonObject(with: JSONEncoder().encode(response)) }
        else { value.removeValue(forKey: "response") }
        return try JSONDecoder().decode(AsyncQuestion.self, from: JSONSerialization.data(withJSONObject: value))
    }
}

// Offline outbox: messages written while the Mac is unreachable wait on the
// phone and are sent in order on reconnect, through the synthetic transport.
extension WonderDiagnosticsTests {
    private static let outboxMessagesPath = "/api/v1/conversations/recovery-chat/messages"
    private static let outboxActivity = Data(#"{"latestTurnId":"settled-turn","latestTurnStatus":"completed","runningElsewhere":false}"#.utf8)

    private func outboxSnapshot(_ sent: [(id: String, body: String)], sequence: Int) throws -> Data {
        let messages: [[String: Any]] = sent.enumerated().map { index, message in
            ["messageId": "server-\(index)", "clientMessageId": message.id, "body": message.body,
             "state": "accepted_by_wonder", "createdAt": "\(2000 + index)", "attachmentIds": [String]()]
        }
        return try JSONSerialization.data(withJSONObject: ["conversationId": "recovery-chat", "hostEpoch": "epoch",
            "lastSequence": sequence, "messages": messages, "assistantMessages": [[String: Any]](),
            "thread": ["hydrated": true, "turns": [["id": "settled-turn", "status": "completed", "items": [[String: Any]]()]]]])
    }

    @MainActor private func waitForOutbox(_ timeout: Duration = .seconds(10), _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + timeout
        while !condition() {
            guard ContinuousClock.now < deadline else { XCTFail("The outbox did not reach the expected state"); return }
            try await Task.sleep(for: .milliseconds(20))
        }
    }

    private func sentRequests() throws -> [SendRequest] {
        try MessageRecoveryURLProtocol.bodies(path: Self.outboxMessagesPath).map { try JSONDecoder().decode(SendRequest.self, from: $0) }
    }

    /// Writes two messages with the Mac unreachable, so neither is sent.
    @MainActor private func queueTwoOffline(root: URL) async throws -> (ConnectionModel, ChatSummary, [OutboxMessage]) {
        let (model, chat) = try recoveryProject(root: root)
        model.macConnected = false
        for body in ["First while offline", "Second while offline"] {
            model.editDraft(body, chat: chat.id)
            XCTAssertTrue(model.canSend(chat), "An unreachable Mac no longer locks Send")
            await model.send(chat)
        }
        let waiting = try XCTUnwrap(model.composers[chat.id]?.outbox)
        XCTAssertEqual(waiting.map(\.body), ["First while offline", "Second while offline"])
        XCTAssertEqual(model.composers[chat.id]?.draft, "")
        XCTAssertNil(model.composers[chat.id]?.pending)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: Self.outboxMessagesPath).isEmpty)
        return (model, chat, waiting)
    }

    // Contract: two messages queued offline survive a relaunch and, on
    // reconnect, are each sent once, in order, the second only after the
    // Mac accepted the first. The outbox and its index are then empty.
    @MainActor func testOutboxSurvivesRelaunchAndDrainsInOrderOnceOnReconnect() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (_, chat, waiting) = try await queueTwoOffline(root: root)

        // Relaunch: a new model reads the same durable store.
        MessageRecoveryURLProtocol.reset()
        let (model, _) = try recoveryProject(root: root)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat/history/activity", body: Self.outboxActivity)
        let path = "/api/v1/conversations/recovery-chat"
        MessageRecoveryURLProtocol.enqueue(path: path, body: try outboxSnapshot([(waiting[0].id, waiting[0].body)], sequence: 2))
        MessageRecoveryURLProtocol.enqueue(path: path, body: try outboxSnapshot(waiting.map { ($0.id, $0.body) }, sequence: 3))
        model.macConnected = false
        model.macConnected = true
        try await waitForOutbox { !model.isDrainingOutbox && model.composers[chat.id]?.pending == nil && model.composers[chat.id]?.outbox == nil }

        let sent = try sentRequests()
        XCTAssertEqual(sent.map(\.clientMessageId), waiting.map(\.id), "Each sent once, in order, under its own ID")
        XCTAssertEqual(sent.map(\.body), ["First while offline", "Second while offline"])
        let saved = Self.cameraSavedConnection()
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        XCTAssertNil(try store.loadComposer(conversation: chat.id).outbox)
        XCTAssertEqual(try store.loadOutboxIndex(), [])
    }

    // Contract: a cancelled outbox drain that ends after its successor started
    // leaves the successor's handle alone, so the outbox never drains twice at
    // once. Regression: the late drain set the handle to nil over the new one.
    @MainActor func testACancelledOutboxDrainFinishingLateKeepsItsSuccessorRunning() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        let (model, _, _) = try await queueTwoOffline(root: root)
        let activity = "/api/v1/conversations/recovery-chat/history/activity"
        MessageRecoveryURLProtocol.hold(path: activity)
        model.macConnected = true
        try await waitForOutbox { !MessageRecoveryURLProtocol.bodies(path: activity, includingEmpty: true).isEmpty }
        XCTAssertTrue(model.isDrainingOutbox)
        // Backgrounding cancels the drain; reconnecting starts the next one before
        // the cancelled drain has run its last lines on the main actor.
        model.setForeground(false)
        model.macConnected = true
        XCTAssertTrue(model.isDrainingOutbox)
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertTrue(model.isDrainingOutbox, "the cancelled drain cleared the handle of the drain that replaced it")
    }

    // Contract: a history re-read that is still running when the computer is
    // paired again does not leave its chat marked as refreshing for the new
    // pairing (Send stays refused for a chat nothing will refresh).
    @MainActor func testRepairingClearsAHistoryRefreshStillRunning() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        model.macConnected = true
        let refresh = "/api/v1/project-conversations/recovery-chat"
        MessageRecoveryURLProtocol.hold(path: refresh)
        let reload = Task { @MainActor in await model.reloadNativeHistory(chat) }
        try await waitForOutbox { model.nativeHistoryRefreshing.contains(chat.id) }
        model.connection = Self.cameraSavedConnection(hostID: "another-host")
        XCTAssertTrue(model.nativeHistoryRefreshing.isEmpty, "the old pairing's refresh marker survived re-pairing")
        MessageRecoveryURLProtocol.releaseHeld()
        await reload.value
        XCTAssertTrue(model.nativeHistoryRefreshing.isEmpty)
    }

    // Contract: chats' saved notification levels are read again once access to
    // the computer has ended and returned; a chat read before is not skipped.
    @MainActor func testNotificationLevelsAreReadAgainAfterAccessEndsAndReturns() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let model = recoveryModel(root: root)
        let path = "/api/v1/conversations/notified-chat/notifications"
        MessageRecoveryURLProtocol.enqueue(path: path, body: Data(#"{"notificationLevel":"mute"}"#.utf8))
        await model.loadChatNotifications(["notified-chat"])
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count, 1)
        model.accessEnded = true
        XCTAssertTrue(model.chatNotificationReads.isEmpty)
        model.accessEnded = false
        MessageRecoveryURLProtocol.enqueue(path: path, body: Data(#"{"notificationLevel":"follow"}"#.utf8))
        await model.loadChatNotifications(["notified-chat"])
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count, 2,
                       "a chat read before access ended was never read again")
    }

    // Contract: a refusal stops the thread's outbox with the reason in plain
    // words; nothing behind it is sent. Retry sends the same message (same
    // client message ID) and then the next.
    @MainActor func testOutboxRefusalStopsTheThreadAndRetryResumesIt() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat, waiting) = try await queueTwoOffline(root: root)
        for _ in 0..<3 { MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat/history/activity", body: Self.outboxActivity) }
        MessageRecoveryURLProtocol.enqueue(path: Self.outboxMessagesPath, status: 409)
        model.macConnected = true
        try await waitForOutbox { !model.isDrainingOutbox && model.composers[chat.id]?.outbox?.first?.refusal != nil }

        XCTAssertEqual(try sentRequests().map(\.clientMessageId), [waiting[0].id], "The message behind a refusal is not sent")
        XCTAssertEqual(model.composers[chat.id]?.outbox?.map(\.id), waiting.map(\.id))
        XCTAssertEqual(model.composers[chat.id]?.outbox?.first?.refusal,
                       "Your computer couldn’t take this message. The thread may be archived; check it on your computer.")
        XCTAssertNil(model.composers[chat.id]?.pending)
        XCTAssertEqual(model.composers[chat.id]?.draft, "", "A refused waiting message never lands in the draft")

        let path = "/api/v1/conversations/recovery-chat"
        MessageRecoveryURLProtocol.enqueue(path: path, body: try outboxSnapshot([(waiting[0].id, waiting[0].body)], sequence: 2))
        MessageRecoveryURLProtocol.enqueue(path: path, body: try outboxSnapshot(waiting.map { ($0.id, $0.body) }, sequence: 3))
        model.retryOutboxMessage(waiting[0].id, chat: chat)
        try await waitForOutbox { !model.isDrainingOutbox && model.composers[chat.id]?.outbox == nil && model.composers[chat.id]?.pending == nil }
        XCTAssertEqual(try sentRequests().map(\.clientMessageId), [waiting[0].id, waiting[0].id, waiting[1].id])
    }

    // Contract: when the connection dies after the request left, the send is
    // checked against the thread before anything is repeated: a message the
    // Mac already has is not sent again, and the next one follows.
    @MainActor func testOutboxChecksAnUncertainSendInsteadOfResendingIt() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat, waiting) = try await queueTwoOffline(root: root)
        for _ in 0..<3 { MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat/history/activity", body: Self.outboxActivity) }
        MessageRecoveryURLProtocol.fail(path: Self.outboxMessagesPath, error: .networkConnectionLost)
        let path = "/api/v1/conversations/recovery-chat"
        // The Mac did receive the first message before the connection dropped.
        MessageRecoveryURLProtocol.enqueue(path: path, body: try outboxSnapshot([(waiting[0].id, waiting[0].body)], sequence: 2))
        MessageRecoveryURLProtocol.enqueue(path: path, body: try outboxSnapshot(waiting.map { ($0.id, $0.body) }, sequence: 3))
        model.macConnected = true
        try await waitForOutbox { !model.isDrainingOutbox && model.composers[chat.id]?.outbox == nil && model.composers[chat.id]?.pending == nil }
        XCTAssertEqual(try sentRequests().map(\.clientMessageId), waiting.map(\.id), "The uncertain first send is not repeated")
    }

    // Contract: a message another pairing wrote is never sent; it stays
    // readable for Copy text or Discard, and this pairing's Send is normal.
    @MainActor func testOutboxNeverSendsAnotherPairingsMessage() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let saved = Self.cameraSavedConnection()
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        var previous = ComposerIntent()
        previous.draft = "Written under the previous pairing"
        try previous.enqueue(device: "previous-device", clientMessageID: "previous-message")
        try store.saveComposer(previous, conversation: "recovery-chat")
        try store.saveOutboxIndex(["recovery-chat"])
        let (model, chat) = try recoveryProject(root: root)
        model.macConnected = false
        model.macConnected = true
        try await Task.sleep(for: .milliseconds(200))
        try await waitForOutbox { !model.isDrainingOutbox }
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: Self.outboxMessagesPath).isEmpty)
        XCTAssertEqual(model.composers[chat.id]?.outbox?.map(\.id), ["previous-message"])
        XCTAssertFalse(model.sendsToOutbox(chat), "Another pairing's message does not hold this pairing's Send")
        model.cancelOutboxMessage("previous-message", chat: chat)
        XCTAssertNil(model.composers[chat.id]?.outbox)
    }

    // Contract: a send that fails after the Mac looked reachable is listed
    // durably before its first attempt and retried by the outbox drain with
    // no chat on screen, under its original client message ID and pairing;
    // the thread is checked before the repeat, and nothing is sent twice
    // once the Mac has it. Regression: only the open chat retried it.
    @MainActor func testFailedSendRetriesWithoutItsChatOnScreenUnderItsOriginalIdentity() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        model.macConnected = true
        try await waitForOutbox { !model.isDrainingOutbox }
        for _ in 0..<3 { MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat/history/activity", body: Self.outboxActivity) }
        MessageRecoveryURLProtocol.enqueue(path: Self.outboxMessagesPath, status: 503)
        model.editDraft("Send after a failure", chat: chat.id)
        XCTAssertFalse(model.sendsToOutbox(chat))
        await model.send(chat)

        let pending = try XCTUnwrap(model.composers[chat.id]?.pending)
        XCTAssertNil(pending.receipt)
        let saved = Self.cameraSavedConnection()
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        XCTAssertEqual(try store.loadOutboxIndex(), [chat.id], "Listed for the drain before the first attempt")
        XCTAssertEqual(try store.loadComposer(conversation: chat.id).pending?.request.clientMessageId, pending.request.clientMessageId)

        // The thread does not have it yet; after the retry it does.
        let path = "/api/v1/conversations/recovery-chat"
        MessageRecoveryURLProtocol.enqueue(path: path, body: try outboxSnapshot([], sequence: 2))
        MessageRecoveryURLProtocol.enqueue(path: path, body: try outboxSnapshot([(pending.request.clientMessageId, pending.request.body)], sequence: 3))
        try await waitForOutbox(.seconds(15)) { !model.isDrainingOutbox && model.composers[chat.id]?.pending == nil }

        let sent = try sentRequests()
        XCTAssertEqual(sent.map(\.clientMessageId), [pending.request.clientMessageId, pending.request.clientMessageId],
                       "One failed attempt, one retry, both as the same message")
        XCTAssertEqual(Set(sent.map(\.deviceId)), [saved.credential.deviceId])
        XCTAssertEqual(sent.last?.body, "Send after a failure")
        XCTAssertEqual(try store.loadOutboxIndex(), [])
        XCTAssertEqual(model.composers[chat.id]?.draft, "")
    }

    // Contract: a New chat sent offline waits on the phone, then on reconnect
    // creates its thread through New chat's own path and sends its first
    // message once, under the request ID.
    @MainActor func testQueuedNewChatCreatesItsThreadThenSendsOnce() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let host = Self.cameraSavedConnection().credential.hostInstallationId
        NewChatDraftStore.remove(host: host)
        defer { try? FileManager.default.removeItem(at: root); NewChatDraftStore.remove(host: host) }
        let model = recoveryModel(root: root)
        var draft = NewChatDraft(destination: .project(id: "project"), text: "Start this while offline", family: .codex, model: "fixture-model")
        draft.queue(device: Self.cameraSavedConnection().credential.deviceId)
        XCTAssertNotNil(NewChatDraftStore.startNew(from: draft, host: host))
        model.reloadQueuedNewChats()
        XCTAssertEqual(model.queuedNewChats.map(\.requestID), [draft.requestID])

        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/projects/project/threads", body: Data(#"{"conversation":{"reference":"codex:fixture","conversationId":"recovery-chat","title":"Start this","family":"codex","updatedAt":1,"isPinned":false,"hasUnread":false,"isWorking":false},"receipt":null}"#.utf8))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/recovery-chat", body: Data(#"{"conversationId":"recovery-chat","projectId":"project","projectName":"Todo app","title":"Start this","family":"codex","model":"fixture-model","accessMode":"workspace","workingFolder":"/fixture","workingFolderName":"Todo app","isPinned":false,"hasUnread":false,"hasNativeSession":true,"folderInProject":true}"#.utf8))
        model.macConnected = false
        model.macConnected = true
        try await waitForOutbox { !model.isDrainingOutbox && model.queuedNewChats.isEmpty && model.composers["recovery-chat"]?.pending?.receipt != nil }
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/projects/project/threads").count, 1)
        let sent = try sentRequests()
        XCTAssertEqual(sent.map(\.clientMessageId), [draft.requestID])
        XCTAssertEqual(sent.first?.body, "Start this while offline")
        XCTAssertTrue(NewChatDraftStore.queued(host: host).isEmpty)
    }
}

// MARK: Demo mode

extension WonderDiagnosticsTests {
    /// Saved computers kept in memory, recording every write the library makes.
    @MainActor private final class DemoStorage {
        var values: [String: Data] = [:]
        var writes: [String] = []
        var storage: ConnectionLibrary.Storage {
            ConnectionLibrary.Storage(read: { self.values[$0] },
                                      save: { data, account in self.values[account] = data; self.writes.append(account) },
                                      forgetLegacy: {})
        }
        func stored() throws -> SavedConnections? {
            try values["connections-v1"].map { try JSONDecoder().decode(SavedConnections.self, from: $0) }
        }
    }

    @MainActor private func waitUntil(_ timeout: Duration = .seconds(10), _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + timeout
        while !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("Timed out") }
            try await Task.sleep(for: .milliseconds(20))
        }
    }

    // Contract: the demo shows one computer while nothing is paired, is never
    // stored with the pairings, and leaves nothing behind when left or when a
    // real Mac is paired (which ends it).
    @MainActor func testDemoIsRemovedOnLeaveAndOnRealPairingWithoutTouchingStoredPairings() async throws {
        let storage = DemoStorage()
        let library = ConnectionLibrary(storage: storage.storage)
        let writesBefore = storage.writes.count
        XCTAssertTrue(library.startDemo())
        let model = try XCTUnwrap(library.demo)
        XCTAssertEqual(library.saved.connections.map(\.credential.hostInstallationId), [DemoMode.hostID])
        XCTAssertTrue(library.paired.isEmpty, "The demo is not a paired computer")
        XCTAssertTrue(library.model(for: DemoMode.connection()) === model)
        model.setForeground(true)
        // Pins arrive before the refresh's other reads; it saves the catalog once they finish.
        try await waitUntil { !model.projects.pinned.isEmpty && !model.projects.loadingProjects }
        let cacheKey = "wonder.projects." + ProjectLibrary.cacheKeySuffix(DemoMode.connection())
        await ProjectCatalogWriter.shared.settle()
        XCTAssertNotNil(UserDefaults.standard.data(forKey: cacheKey), "The demo's catalog is cached while it runs")
        library.filter(DemoMode.hostID)
        XCTAssertEqual(storage.writes.count, writesBefore, "Running the demo stores nothing")

        await library.leaveDemo()
        XCTAssertFalse(library.isDemo)
        XCTAssertTrue(library.saved.connections.isEmpty)
        XCTAssertNil(UserDefaults.standard.data(forKey: cacheKey))
        let temporary = try FileManager.default.contentsOfDirectory(atPath: FileManager.default.temporaryDirectory.path)
        XCTAssertFalse(temporary.contains { $0.hasPrefix("wonder-demo-") }, "The demo's chats are removed")
        XCTAssertEqual(storage.writes.count, writesBefore)

        XCTAssertTrue(library.startDemo())
        let retiringDemo = try XCTUnwrap(library.demo)
        let real = Self.cameraSavedConnection(hostID: "demo-test-real-mac")
        try library.savePairing(real)
        XCTAssertFalse(library.isDemo, "Pairing a real Mac ends the demo")
        XCTAssertEqual(library.saved.connections.map(\.credential.hostInstallationId), ["demo-test-real-mac"])
        XCTAssertEqual(try storage.stored()?.connections.map(\.credential.hostInstallationId), ["demo-test-real-mac"],
                       "Only the real Mac is stored")
        XCTAssertFalse(library.startDemo(), "The demo is offered only while nothing is paired")
        XCTAssertThrowsError(try library.savePairing(DemoMode.connection()))
        // Pairing ends the demo synchronously and drains its writer asynchronously.
        // Finish this fixture's cleanup before the next test starts a new demo.
        try await waitUntil { retiringDemo.connection == nil }
    }

    // Contract: the demo computer never reaches notification enrollment,
    // presence or Live Activities, and none of its requests leave the app.
    @MainActor func testDemoNeverEnrollsNotificationsOrPresence() async throws {
        let storage = DemoStorage()
        let library = ConnectionLibrary(storage: storage.storage)
        XCTAssertTrue(library.startDemo())
        addTeardownBlock { @MainActor in await library.leaveDemo() }
        let model = try XCTUnwrap(library.demo)
        var authorizations = 0
        var values: [String: Data] = [:]
        let push = PushNotifications(read: { values[$0] }, write: { values[$1] = $0 }, authorize: {
            authorizations += 1
            return true
        }, registerForPush: {}, previewKey: { _ in nil }, reportActionFailure: { _ in })
        push.attach(library)
        push.enable(model)
        push.setOpenConversation(host: DemoMode.hostID, id: DemoContent.signOut.id)
        push.setForeground(true)
        push.refresh()
        model.setForeground(true)
        try await waitUntil { !model.projects.pinned.isEmpty }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertFalse(push.isEnabled(DemoMode.hostID))
        XCTAssertNil(push.setupState(DemoMode.hostID))
        XCTAssertEqual(authorizations, 0, "The demo never asks to notify")
        let requested = try values["push-requested-devices-v1"].map { try JSONDecoder().decode([String: String].self, from: $0) } ?? [:]
        XCTAssertNil(requested[DemoMode.hostID])
        XCTAssertTrue(library.paired.isEmpty, "Presence and Live Activities follow paired computers only")
        XCTAssertFalse(DemoServer.shared.servedPaths.isEmpty, "The demo computer answered inside the app")
        XCTAssertFalse(DemoServer.shared.servedPaths.contains { $0.hasPrefix("/api/v1/push") || $0.hasPrefix("/api/v1/events") })
        XCTAssertTrue(storage.writes.filter { $0 == "connections-v1" }.count <= 1, "Only the empty library's first load is stored")
    }

    // Contract: the demo's transport takes every request and answers only the
    // demo computer, so a demo session cannot reach the network.
    func testDemoTransportNeverReachesTheNetwork() async throws {
        let configuration = DemoMode.sessionConfiguration()
        XCTAssertEqual(configuration.protocolClasses?.count, 1)
        XCTAssertTrue(configuration.protocolClasses?.first == DemoURLProtocol.self)
        XCTAssertNil(configuration.urlCache)
        XCTAssertNil(configuration.httpCookieStorage)
        let session = URLSession(configuration: configuration)
        defer { session.invalidateAndCancel() }
        for address in ["https://example.com/api/v1/projects", "http://demo.wonder.invalid/api/v1/projects", "https://getcupola.app/"] {
            let request = URLRequest(url: try XCTUnwrap(URL(string: address)))
            XCTAssertTrue(DemoURLProtocol.canInit(with: request), "The demo's transport takes \(address) instead of the network")
            do {
                _ = try await session.data(for: request)
                XCTFail("\(address) answered")
            } catch let error as URLError {
                XCTAssertEqual(error.code, .notConnectedToInternet, address)
            }
        }
        let (data, response) = try await session.data(from: try XCTUnwrap(URL(string: DemoMode.origin + "/api/v1/host/status")))
        XCTAssertEqual((response as? HTTPURLResponse)?.statusCode, 200)
        XCTAssertNotNil(try JSONSerialization.jsonObject(with: data) as? [String: Any])
    }

}

private extension Data {
    init?(base64URL text: String) {
        var value = text.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        value += String(repeating: "=", count: (4 - value.count % 4) % 4)
        self.init(base64Encoded: value)
    }
}

private extension ComposerPersistenceWorker {
    func holdPhotoPersistenceForTest(_ gate: DispatchSemaphore, started: XCTestExpectation) {
        XCTAssertFalse(Thread.isMainThread)
        started.fulfill()
        gate.wait()
    }
}

/// Supplies UIKit's contact count to the actual recognizer delegate without
/// synthesizing a second remote-input implementation.
private final class ComputerRepeatedTouch: UITouch {
    private let count: Int
    init(count: Int) { self.count = count; super.init() }
    override var tapCount: Int { count }
}

/// Contract: every Settings > Acknowledgements row opens a notice the app bundles,
/// and every Swift package the app links is credited at its pinned version, so a
/// resource left out of the bundle or a dependency bump cannot ship a blank or
/// stale notice.
@MainActor final class AcknowledgementsTests: XCTestCase {
    private var notices: [(title: String, text: String?)] {
        AcknowledgementsView.sections.flatMap(\.1).map { ($0.title, $0.text()) }
    }

    func testLinkedPackagesAreCreditedAtTheirPinnedVersions() throws {
        struct Resolved: Decodable {
            struct Pin: Decodable {
                struct State: Decodable { let version: String? }
                let identity: String
                let state: State
            }
            let pins: [Pin]
        }
        let resolved = try JSONDecoder().decode(Resolved.self, from: Data(contentsOf: DiagnosticTestResources.url(
            "apps/ios/Wonder.xcodeproj/project.xcworkspace/xcshareddata/swiftpm/Package.resolved")))
        // Only SwiftTerm's command-line tools use it; the app links the SwiftTerm library alone.
        let buildOnly: Set = ["swift-argument-parser"]
        let squash = { (text: Substring) in text.lowercased().filter { $0.isLetter || $0.isNumber } }
        let lines = notices.compactMap(\.text).joined(separator: "\n").split(separator: "\n")
        XCTAssertGreaterThan(resolved.pins.count, buildOnly.count)
        for pin in resolved.pins where !buildOnly.contains(pin.identity) {
            let version = try XCTUnwrap(pin.state.version, "\(pin.identity) is pinned without a version")
            let name = squash(Substring(pin.identity))
            XCTAssertTrue(lines.contains { $0.contains(version) && squash($0).contains(name) },
                          "\(pin.identity) \(version) has no notice in Acknowledgements; update its bundled notice "
                          + "(for WebRTC, run fastlane/app-store/collect-webrtc-notices.py)")
        }
    }
}

/// Contract: only the top mounted surface receives input; removing it restores
/// the underlying view. The authenticated loopback endpoint must obey the same
/// ownership and reject bad tokens, oversized bodies and unknown actions.
@MainActor final class DiagnosticTestDriverTests: XCTestCase {
    func testTopSurfaceOwnsControlsAndDismissalUntilUnmounted() throws {
        let driver = DiagnosticTestDriver(), root = UUID(), sheet = UUID()
        var tapped = 0, dismissed = 0
        var active = true
        driver.register(root, .init(priority: 0, active: { active }, state: { ["route": "conversation"] },
            controls: { [.init(id: "send", tap: { tapped += 1 })] }))
        driver.register(sheet, .init(priority: 20, state: { ["sheets": ["preview"]] },
            controls: { [.init(id: "done", tap: { dismissed += 1 })] }, actions: ["dismiss": { _ in dismissed += 1 }]))
        XCTAssertThrowsError(try driver.perform("tap", arguments: ["id": "send"]))
        XCTAssertEqual(tapped, 0)
        XCTAssertEqual(driver.snapshot()["route"] as? String, "conversation")
        XCTAssertEqual(driver.snapshot()["sheets"] as? [String], ["preview"])
        try driver.perform("dismiss", arguments: [:])
        XCTAssertEqual(dismissed, 1)
        driver.remove(sheet)
        try driver.perform("tap", arguments: ["id": "send"])
        XCTAssertEqual(tapped, 1)
        XCTAssertEqual(driver.snapshot()["sheets"] as? [String], [])
        active = false
        XCTAssertThrowsError(try driver.perform("tap", arguments: ["id": "send"]))
        XCTAssertEqual(driver.snapshot()["route"] as? String, "unavailable")
        driver.remove(root)
    }

    func testDisabledDuplicateAndUnknownActionsCannotMutateState() {
        let driver = DiagnosticTestDriver(), owner = UUID()
        var calls = 0
        driver.register(owner, .init(priority: 0, state: { [:] }, controls: {
            [.init(id: "disabled", enabled: false, tap: { calls += 1 }),
             .init(id: "duplicate", tap: { calls += 1 }), .init(id: "duplicate", tap: { calls += 1 })]
        }))
        for id in ["disabled", "duplicate", "missing"] { XCTAssertThrowsError(try driver.perform("tap", arguments: ["id": id])) }
        XCTAssertThrowsError(try driver.perform("send", arguments: [:]))
        XCTAssertEqual(calls, 0)
    }

    func testUpdatedSurfaceUsesCurrentStateAndAction() throws {
        let driver = DiagnosticTestDriver(), owner = UUID()
        var value = "first"
        driver.register(owner, .init(priority: 0, state: { ["value": value] }, actions: ["type": { value = $0["text"] ?? "" }]))
        try driver.perform("type", arguments: ["text": "Exact changelog 🦋"])
        XCTAssertEqual(driver.snapshot()["value"] as? String, "Exact changelog 🦋")
        driver.register(owner, .init(priority: 0, state: { ["route": "replacement"] }))
        XCTAssertThrowsError(try driver.perform("type", arguments: ["text": "stale"]))
        XCTAssertEqual(value, "Exact changelog 🦋")
    }

    private func startedDriver() async throws -> DiagnosticTestDriver {
        let driver = DiagnosticTestDriver()
        driver.start(token: String(repeating: "x", count: 32))
        for _ in 0..<100 where driver.listeningPort == nil { try await Task.sleep(for: .milliseconds(20)) }
        guard driver.listeningPort != nil else { driver.stop(); throw URLError(.cannotConnectToHost) }
        return driver
    }
    private func request(_ driver: DiagnosticTestDriver, token: String? = String(repeating: "x", count: 32),
                         body: Data? = nil, path: String = "/state") async throws -> (Data, Int) {
        var request = URLRequest(url: URL(string: "http://127.0.0.1:\(driver.listeningPort!)\(path)")!)
        request.httpMethod = body == nil ? "GET" : "POST"
        request.httpBody = body
        if let token { request.setValue("Bearer " + token, forHTTPHeaderField: "Authorization") }
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 3
        let session = URLSession(configuration: configuration)
        defer { session.invalidateAndCancel() }
        let (data, response) = try await session.data(for: request)
        return (data, (response as! HTTPURLResponse).statusCode)
    }

    func testEndpointRequiresLaunchOptInAndAuthentication() async throws {
        let disabled = DiagnosticTestDriver()
        disabled.start(token: nil)
        XCTAssertNil(disabled.listeningPort)
        disabled.start(token: "short")
        XCTAssertNil(disabled.listeningPort)
        let driver = try await startedDriver()
        defer { driver.stop() }
        let (_, missing) = try await request(driver, token: nil)
        let (_, wrong) = try await request(driver, token: "incorrect")
        let (state, authorized) = try await request(driver)
        XCTAssertEqual(missing, 401); XCTAssertEqual(wrong, 401); XCTAssertEqual(authorized, 200)
        XCTAssertEqual((try JSONSerialization.jsonObject(with: state) as? [String: Any])?["version"] as? Int, 1)
    }

    func testEndpointExecutesActionsAndRejectsMalformedOrOversizedInput() async throws {
        let driver = try await startedDriver(), owner = UUID()
        defer { driver.stop() }
        var draft = ""
        driver.register(owner, .init(priority: 0, state: { ["composer": ["text": draft]] },
            actions: ["type": { draft += $0["text"] ?? "" }]))
        let body = try JSONSerialization.data(withJSONObject: ["action": "type", "text": "Exact changelog 🦋"])
        let (_, sent) = try await request(driver, body: body, path: "/action")
        XCTAssertEqual(sent, 200); XCTAssertEqual(draft, "Exact changelog 🦋")
        let (_, unknown) = try await request(driver, body: Data("{\"action\":\"unknown\"}".utf8), path: "/action")
        let (_, malformed) = try await request(driver, body: Data("not json".utf8), path: "/action")
        let (_, oversized) = try await request(driver, body: Data(repeating: 120, count: 60001), path: "/action")
        XCTAssertEqual(unknown, 409); XCTAssertEqual(malformed, 409); XCTAssertEqual(oversized, 400)
        XCTAssertEqual(draft, "Exact changelog 🦋")
    }
}

/// Observable contract: only confirmed host settings are selected, unavailable
/// switches stay hidden, and an old request cannot update a replaced page.
/// Owner: the settings controller used by the computer settings view.
@MainActor final class ComputerUseSettingsTests: XCTestCase {
    private func payload(enabled: Bool = true, driver: Bool = true, native: Bool = true, available: Bool = true) throws -> ComputerUseSettings {
        try JSONDecoder().decode(ComputerUseSettings.self, from: JSONSerialization.data(withJSONObject: [
            "computerUseAvailable": available, "cuaDriverEnabled": available && enabled, "cuaDriverInstalled": driver,
            "cuaDriverStatus": !available ? "Computer use is off on this computer" : !enabled ? "Off" : driver ? "Using Cua Driver" : "Cua Driver isn't installed",
            "codexUsesChatGPT": available && native
        ]))
    }

    private func waitUntil(_ condition: () -> Bool) async throws {
        for _ in 0..<10_000 {
            if condition() { return }
            await Task.yield()
        }
        XCTFail("The settings request did not reach its held state")
        throw URLError(.timedOut)
    }

    func testToggleRoundTripKeepsConfirmedValueOnFailureAndRetries() async throws {
        let state = ComputerUseSettingsModel(), initial = try payload()
        await state.load { initial }
        var resume: CheckedContinuation<ComputerUseSettings, Error>?
        let save = Task { await state.save(false) { try await withCheckedThrowingContinuation { resume = $0 } } }
        try await waitUntil { resume != nil }
        XCTAssertEqual(state.saving, false)
        XCTAssertEqual(state.settings?.cuaDriverEnabled, true)
        await state.save(false) { XCTFail("A second save must wait"); return initial }
        resume?.resume(throwing: PairingFailure.response(503))
        await save.value
        XCTAssertNil(state.saving)
        XCTAssertNotNil(state.failure)
        XCTAssertEqual(state.settings, initial)
        let saved = try payload(enabled: false)
        await state.save(false) { saved }
        XCTAssertEqual(state.settings, saved)
        XCTAssertNil(state.failure)
        // A newly owned screen reads the saved host response rather than a phone default.
        let reopened = ComputerUseSettingsModel()
        await reopened.load { saved }
        XCTAssertEqual(reopened.settings?.cuaDriverEnabled, false)
        XCTAssertEqual(reopened.settings?.codexUsesChatGPT, true)
        let on = try payload()
        await reopened.save(true) { on }
        XCTAssertEqual(reopened.settings?.cuaDriverEnabled, true)
    }

    func testLoadFailureCanRetryAndOldHostResponseCannotReplaceNewHost() async throws {
        let state = ComputerUseSettingsModel(), initial = try payload()
        await state.load { throw PairingFailure.response(503) }
        XCTAssertFalse(state.loading)
        XCTAssertNil(state.settings)
        XCTAssertNotNil(state.failure)
        var resume: CheckedContinuation<ComputerUseSettings, Error>?
        let stale = Task { await state.load { try await withCheckedThrowingContinuation { resume = $0 } } }
        try await waitUntil { resume != nil }
        XCTAssertTrue(state.loading)
        let replacement = try payload(native: false)
        await state.load { replacement }
        resume?.resume(returning: initial)
        await stale.value
        XCTAssertEqual(state.settings, replacement)
        XCTAssertFalse(state.loading)
        XCTAssertNil(state.failure)
    }

    func testPairingChangeIgnoresLateSaveAndCancellationIsNotFailure() async throws {
        let state = ComputerUseSettingsModel(), initial = try payload()
        await state.load { initial }
        var resume: CheckedContinuation<ComputerUseSettings, Error>?
        let stale = Task { await state.save(false) { try await withCheckedThrowingContinuation { resume = $0 } } }
        try await waitUntil { resume != nil }
        state.invalidate()
        resume?.resume(returning: try payload(enabled: false))
        await stale.value
        XCTAssertEqual(state.settings, initial)
        XCTAssertNil(state.saving)
        await state.load { throw CancellationError() }
        XCTAssertNil(state.failure)
        XCTAssertFalse(state.loading)
    }

    func testToggleWriteUsesHostRequestShape() throws {
        for enabled in [true, false] {
            let json = try XCTUnwrap(JSONSerialization.jsonObject(with: ComputerUseSettings.body(enabled)) as? [String: Bool])
            XCTAssertEqual(json, ["cuaDriverEnabled": enabled])
        }
    }
}

/// Contract: dragging the Chats drawer redraws only the moving card. The sidebar
/// and the conversation are built by the layout that places the card, so a drag
/// frame must not ask them to update. Regression (B-046): the drag position lived
/// in that layout, rebuilding the whole chat behind the drawer on every frame.
@MainActor final class DrawerDragTests: XCTestCase {
    final class Counter {
        var bodies = 0
        var positions: [CGFloat] = []
    }

    /// Stands in for the conversation or the sidebar. Like the conversation's
    /// sidebar button, it carries a closure from the scope that built it; SwiftUI
    /// cannot compare closures, so the probe redraws whenever that scope rebuilds it.
    struct Probe: View {
        let counter: Counter
        let action: () -> Void
        var body: some View {
            let _ = counter.bodies += 1
            Color.clear.onGeometryChange(for: CGFloat.self) { $0.frame(in: .global).minX } action: {
                counter.positions.append($0)
            }
        }
    }

    static func card(shell: ShellState, drag: DrawerDrag, window: CGSize,
                     sidebar: Counter, conversation: Counter) -> some View {
        let built = UUID()
        return DrawerCard(drag: drag, shell: shell, sidebarOpen: shell.sidebarOpen, edgeOpens: true,
                          wide: false, coverDrawer: false, width: SidebarCard.sidebarWidth(for: window),
                          wideHidden: false, sidebarShown: shell.sidebarOpen, window: window, insets: EdgeInsets()) {
            Probe(counter: sidebar) { _ = built }
        } conversation: {
            Probe(counter: conversation) { _ = built }
        }
    }

    /// The shipped shape: the layout holds the drag without observing it.
    struct Layout: View {
        @ObservedObject var shell: ShellState
        let drag: DrawerDrag
        let window: CGSize
        let sidebar: Counter, conversation: Counter
        var body: some View {
            DrawerDragTests.card(shell: shell, drag: drag, window: window, sidebar: sidebar, conversation: conversation)
        }
    }

    /// The regression's shape: the layout itself redraws on every drag frame.
    struct ObservingLayout: View {
        @ObservedObject var shell: ShellState
        @ObservedObject var drag: DrawerDrag
        let window: CGSize
        let sidebar: Counter, conversation: Counter
        var body: some View {
            DrawerDragTests.card(shell: shell, drag: drag, window: window, sidebar: sidebar, conversation: conversation)
        }
    }

    /// Opens the drawer by 10 pt steps, one rendered frame apart, and reports how
    /// often each piece of content redrew while the card moved.
    private func drag(observing: Bool) async throws -> (sidebar: Int, conversation: Int, moved: CGFloat) {
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        let previousWindow = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        let shell = ShellState(), drag = DrawerDrag()
        let sidebar = Counter(), conversation = Counter()
        let size = window.bounds.size
        window.rootViewController = observing
            ? UIHostingController(rootView: AnyView(ObservingLayout(shell: shell, drag: drag, window: size,
                                                                    sidebar: sidebar, conversation: conversation)))
            : UIHostingController(rootView: AnyView(Layout(shell: shell, drag: drag, window: size,
                                                           sidebar: sidebar, conversation: conversation)))
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previousWindow?.makeKey()
        }
        func frame() async throws {
            window.layoutIfNeeded()
            try await Task.sleep(for: .milliseconds(16))
        }
        for _ in 0..<5 { try await frame() }
        XCTAssertGreaterThan(conversation.bodies, 0, "The conversation is rendered before the drag")
        let before = (sidebar: sidebar.bodies, conversation: conversation.bodies)
        let start = conversation.positions.last ?? 0
        drag.engaged = true
        drag.width = SidebarCard.sidebarWidth(for: size)
        for travel in stride(from: CGFloat(10), through: 200, by: 10) {
            drag.offset = travel
            try await frame()
        }
        return (sidebar.bodies - before.sidebar, conversation.bodies - before.conversation,
                (conversation.positions.last ?? 0) - start)
    }

}
#endif // os(iOS)

#if os(iOS)
/// Observable contract: a list load that began before Run Now cannot replace
/// the run Run Now showed, a retry after a lost response cannot start a second
/// run, and "Last" counts manual runs.
final class AutomationRunBoardTests: XCTestCase {
    private func run(_ id: String, status: String, startedAt: String) throws -> AutomationRun {
        try JSONDecoder().decode(AutomationRun.self, from: Data(#"{"id":"\#(id)","automationId":"brief","status":"\#(status)","startedAt":"\#(startedAt)","error":null}"#.utf8))
    }

    private func item(lastRunAt: String?) throws -> AutomationItem {
        let last = lastRunAt.map { "\"\($0)\"" } ?? "null"
        return try JSONDecoder().decode(AutomationItem.self, from: Data(#"{"id":"brief","revision":3,"name":"Morning brief","scopeType":"project_thread","scopeId":"thread","prompt":"Check progress","rrule":"FREQ=DAILY","timezone":"UTC","notificationPolicy":"all_runs","status":"active","nextRunAt":null,"lastRunAt":\#(last),"lastAttemptAt":null}"#.utf8))
    }

    func testLoadThatBeganBeforeRunNowCannotOverwriteItsRun() throws {
        var board = AutomationRunBoard()
        let loadStarted = board.revision
        // Run Now starts and reports its run while the load is in flight.
        board.changed()
        board.record(try run("manual", status: "running", startedAt: "2026-10-20T12:00:00Z"))
        // The load saw no runs (the automation had never run when it began).
        XCTAssertFalse(board.applyLoad([:], startedAt: loadStarted))
        XCTAssertEqual(board.latest["brief"]?.id, "manual")
        // A load that begins after the run takes effect.
        let fresh = try run("manual", status: "failed", startedAt: "2026-10-20T12:00:00Z")
        XCTAssertTrue(board.applyLoad(["brief": fresh], startedAt: board.revision))
        XCTAssertEqual(board.latest["brief"]?.status, "failed")
    }

    func testRunNowRetryAfterLostResponseReusesItsRequest() {
        var board = AutomationRunBoard()
        let first = board.runRequestID(for: "brief")
        // The Mac started the run but the response never arrived.
        board.settleRunRequest(for: "brief", failure: URLError(.timedOut))
        XCTAssertEqual(board.runRequestID(for: "brief"), first)
        board.settleRunRequest(for: "brief", failure: PairingFailure.response(502))
        XCTAssertEqual(board.runRequestID(for: "brief"), first)
        // The retry returned the run; the next tap is a new run.
        board.settleRunRequest(for: "brief", failure: nil)
        let second = board.runRequestID(for: "brief")
        XCTAssertNotEqual(second, first)
        // A refusal started nothing, so the next tap is a new run too.
        board.settleRunRequest(for: "brief", failure: PairingFailure.hostMessage(409, "Needs an active paired owner device."))
        XCTAssertNotEqual(board.runRequestID(for: "brief"), second)
    }

}

/// Observable contract: the Automations list asks for each thread name and
/// newest run once, several at a time within a bound, and a failure marks
/// only what it was for as missing.
final class AutomationListDetailsTests: XCTestCase {
    private func item(_ id: String, thread: String, hasRuns: Bool) throws -> AutomationItem {
        let attempt = hasRuns ? "\"2026-10-20T12:00:00Z\"" : "null"
        return try JSONDecoder().decode(AutomationItem.self, from: Data(#"{"id":"\#(id)","name":"\#(id)","scopeType":"project_thread","scopeId":"\#(thread)","prompt":"p","rrule":"FREQ=DAILY","timezone":"UTC","notificationPolicy":"all_runs","status":"active","nextRunAt":null,"lastRunAt":null,"lastAttemptAt":\#(attempt)}"#.utf8))
    }

    private func run(_ automation: String) throws -> AutomationRun {
        try JSONDecoder().decode(AutomationRun.self, from: Data(#"{"id":"run-\#(automation)","automationId":"\#(automation)","status":"completed","startedAt":"2026-10-20T12:00:00Z","error":null}"#.utf8))
    }

    private actor Gauge {
        private(set) var running = 0, peak = 0
        func enter() { running += 1; peak = max(peak, running) }
        func leave() { running -= 1 }
    }

}

/// Observable contract: the Mac's timestamps are read once when decoded, with
/// or without fractional seconds, and a malformed one reads as unknown.
final class AutomationDateTests: XCTestCase {
    func testFractionalWholeAndMalformedTimes() throws {
        let whole = try XCTUnwrap(AutomationDate.parse("2026-10-20T12:00:00Z"))
        XCTAssertEqual(AutomationDate.parse("2026-10-20T12:00:00.000Z"), whole)
        XCTAssertEqual(AutomationDate.parse("2026-10-20T12:00:00.250Z")?.timeIntervalSince(whole) ?? 0, 0.25, accuracy: 0.001)
        XCTAssertNil(AutomationDate.parse("yesterday"))
        XCTAssertNil(AutomationDate.parse(nil))
        let run = try JSONDecoder().decode(AutomationRun.self, from: Data(#"{"id":"r","automationId":"a","status":"failed","startedAt":"soon","error":"No phone."}"#.utf8))
        XCTAssertNil(run.startedDate)
        XCTAssertNil(run.startedText)
        XCTAssertEqual(run.startedAt, "soon")
        let fractional = try JSONDecoder().decode(AutomationRun.self, from: Data(#"{"id":"r","automationId":"a","status":"completed","startedAt":"2026-10-20T12:00:00.000Z","error":null}"#.utf8))
        XCTAssertEqual(fractional.startedDate, whole)
        XCTAssertNotNil(fractional.startedText)
    }
}

/// Observable contract: a schedule saved in another time zone is described
/// by that zone's name and kept as it is; one in this device's zone is
/// changed in place; only simple repeats are rebuilt by the editor.
final class AutomationScheduleTests: XCTestCase {
    private let locale = Locale(identifier: "en_US")
    private let here = TimeZone(identifier: "America/New_York")!

    func testSimpleRepeatsRoundTripAndOthersStayAsTheyAre() {
        for rule in ["FREQ=DAILY;BYHOUR=9;BYMINUTE=0", "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=7;BYMINUTE=30",
                     "FREQ=WEEKLY;BYDAY=SU;BYHOUR=23;BYMINUTE=59"] {
            XCTAssertEqual(AutomationSchedule(rrule: rule)?.rrule, rule)
        }
        XCTAssertNil(AutomationSchedule(rrule: "FREQ=MONTHLY;BYMONTHDAY=1;BYHOUR=9;BYMINUTE=0"))
        XCTAssertNil(AutomationSchedule(rrule: "FREQ=DAILY;BYHOUR=24;BYMINUTE=0"))
    }

}
#endif

/// Each way a computer is lost gets its own words; Disconnect asks the
/// computer to drop this device. Synthetic transports only: no real host.
@MainActor final class ConnectionLossTests: XCTestCase {
    private func saved(origin: String, pin: String? = nil, routes: [ConnectionRoute]? = nil) throws -> SavedConnection {
        let credential = try JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"loss-session","deviceId":"loss-device","csrfToken":"loss-csrf","hostInstallationId":"loss-host"}"#.utf8))
        return try SavedConnection(routes: routes ?? [ConnectionRoute(kind: .tailscale, origin: origin)], origin: origin,
                                   credential: credential, hostName: "Studio", hostKeyPin: pin)
    }

    private func signer() -> (SigningIdentity, P256.Signing.PublicKey) {
        let key = P256.Signing.PrivateKey()
        let identity = SigningIdentity(read: { key.rawRepresentation }, save: { _ in }, restore: { _ in
            EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation, sign: { try key.signature(for: $0) })
        }, create: { throw SigningIdentityFailure.missing })
        return (identity, key.publicKey)
    }

    // A1: a computer whose key changed (Cupola reset there) reads as a key
    // change that pairing again fixes, not as "could not reach".
    func testAComputerProvingAnotherKeyIsExplainedInsteadOfUnreachable() async throws {
        _ = RouteProofURLProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let model = ConnectionModel(saved: try saved(origin: "https://studio.example.ts.net", pin: "paired-with-another-key"),
                                    persistConnection: { _ in }, api: PairingAPI(configuration: configuration))
        defer { model.retireAfterPairingReplacement() }
        await model.check(renew: true)
        XCTAssertEqual(model.connectionLoss, .keyChanged)
        XCTAssertEqual(model.status, ConnectionLoss.keyChanged.message(computer: "Studio"))
        XCTAssertEqual(model.macConnected, false)
        XCTAssertFalse(model.accessEnded, "a key change keeps saved chats readable")
        XCTAssertTrue(RouteProofURLProtocol.requests.allSatisfy { $0.value(forHTTPHeaderField: "Cookie") == nil },
                      "nothing signed in reaches a computer with another key")
    }

    // A return from the background confirms the route's key even while the
    // computer still counts as connected and the session is current (no
    // renewal). Regression: with renewal tied to expiry, the return skipped
    // the route check and a computer reset meanwhile looked connected.
    func testReturningProvesTheComputersKeyWithoutRenewing() async throws {
        _ = RouteProofURLProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let model = ConnectionModel(saved: try saved(origin: "https://studio.example.ts.net", pin: "paired-with-another-key"),
                                    persistConnection: { _ in }, api: PairingAPI(configuration: configuration))
        defer { model.retireAfterPairingReplacement() }
        model.macConnected = true
        await model.check(verifyRoute: true)
        XCTAssertEqual(model.connectionLoss, .keyChanged)
        XCTAssertEqual(model.macConnected, false)
        XCTAssertFalse(RouteProofURLProtocol.requests.contains { $0.url?.path == "/api/v1/pairing/session/refresh-challenge" },
                       "a current session is not renewed")
    }

    // B4: Disconnect first has the computer prove the paired key without
    // credentials, then signs a revoke of this device only; the computer
    // checks it as `device.revoke` against the same transcript.
    func testDisconnectAsksTheComputerToRevokeThisDeviceThenForgetsIt() async throws {
        let pin = RouteProofURLProtocol.reset()
        let path = "/api/v1/devices/loss-device/revoke"
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let (identity, publicKey) = signer()
        let connection = try saved(origin: "https://studio.example.ts.net", pin: pin)
        let model = ConnectionModel(saved: connection, persistConnection: { _ in },
                                    api: PairingAPI(configuration: configuration), signingIdentity: identity)
        await model.forget()
        XCTAssertNil(model.connection)
        let requests = RouteProofURLProtocol.requests
        let revokeIndex = try XCTUnwrap(requests.firstIndex { $0.url?.path == path })
        XCTAssertEqual(requests.prefix(revokeIndex).map { $0.url!.path }, ["/readyz", "/api/v1/host/proof"])
        XCTAssertTrue(requests.prefix(revokeIndex).allSatisfy { $0.value(forHTTPHeaderField: "Cookie") == nil },
                      "the key is proved before any credential is sent")
        let revoke = requests[revokeIndex]
        XCTAssertEqual(revoke.value(forHTTPHeaderField: "Cookie"), "__Host-wonder_session=loss-session")
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: XCTUnwrap(revoke.httpBody)) as? [String: Any])
        let nonce = try XCTUnwrap(body["actionNonce"] as? String)
        let issued = try XCTUnwrap(body["issuedAtMs"] as? UInt64)
        let signature = try XCTUnwrap(body["signature"] as? String)
        let transcript = try SignedAction.transcript(action: "device.revoke", path: path, values: [], nonce: nonce,
                                                     connection: connection, issuedAtMs: issued)
        let raw = signature.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        let decoded = try XCTUnwrap(Data(base64Encoded: raw + String(repeating: "=", count: (4 - raw.count % 4) % 4)))
        XCTAssertTrue(publicKey.isValidSignature(try P256.Signing.ECDSASignature(rawRepresentation: decoded), for: transcript))
    }

    // A computer that proved another key gets nothing signed in on Disconnect:
    // Tailscale's TLS says nothing about Cupola's key.
    func testDisconnectAfterAKeyChangeSendsNothingToThatComputer() async throws {
        _ = RouteProofURLProtocol.reset()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let model = ConnectionModel(saved: try saved(origin: "https://studio.example.ts.net", pin: "paired-with-another-key"),
                                    persistConnection: { _ in }, api: PairingAPI(configuration: configuration),
                                    signingIdentity: signer().0)
        await model.check(renew: true)
        XCTAssertEqual(model.connectionLoss, .keyChanged)
        let before = RouteProofURLProtocol.requests.count
        await model.forget()
        XCTAssertNil(model.connection)
        XCTAssertNil(model.error)
        XCTAssertEqual(RouteProofURLProtocol.requests.count, before, "Disconnect sends nothing to a computer with another key")
        XCTAssertTrue(RouteProofURLProtocol.requests.allSatisfy {
            $0.value(forHTTPHeaderField: "Cookie") == nil && $0.value(forHTTPHeaderField: "x-wonder-csrf") == nil
        })
    }

    // An unreachable computer never blocks Disconnect on this device.
    func testDisconnectStillForgetsWhenTheComputerIsUnreachable() async throws {
        Release2RegressionURLProtocol.reset()
        Release2RegressionURLProtocol.fail(path: "/readyz", error: .cannotConnectToHost)
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [Release2RegressionURLProtocol.self]
        let model = ConnectionModel(saved: try saved(origin: "https://camera-unit.invalid", pin: "paired-key"), persistConnection: { _ in },
                                    api: PairingAPI(configuration: configuration), signingIdentity: signer().0)
        await model.forget()
        XCTAssertNil(model.connection)
        XCTAssertNil(model.error)
        XCTAssertTrue(Release2RegressionURLProtocol.bodies(path: "/api/v1/devices/loss-device/revoke", includingEmpty: true).isEmpty)
    }
}

#if os(iOS)
/// An open Project chat sleeps until the Mac wakes it or its fallback deadline
/// passes, instead of checking five times a second. No wake is lost: one that
/// arrives before the wait or between checks is kept until taken.
@MainActor final class NativeWakeSignalTests: XCTestCase {
    func testWakeBeforeTheWaitIsKeptAndCoalesced() async {
        let signal = NativeWakeSignal()
        signal.record("chat")
        signal.record("chat")
        let woken = await signal.wait("chat", until: .now + .seconds(30))
        XCTAssertTrue(woken, "A wake recorded before the screen waits is not lost")
        XCTAssertFalse(signal.take("chat"), "Repeated wakes coalesce into one")
    }

    func testFallbackDeadlineAndCancellationEndTheWait() async {
        let signal = NativeWakeSignal()
        let expired = await signal.wait("chat", until: .now + .milliseconds(50))
        XCTAssertFalse(expired, "Hosts without live wake-ups keep the fallback check")
        XCTAssertEqual(signal.waitingCount, 0)

        let waiter = Task { await signal.wait("chat", until: .now + .seconds(30)) }
        while signal.waitingCount == 0 { await Task.yield() }
        waiter.cancel()
        let cancelled = await waiter.value
        XCTAssertFalse(cancelled, "Leaving the chat ends its wait")
        XCTAssertEqual(signal.waitingCount, 0, "A cancelled wait leaves nothing behind")
        signal.record("chat")
        XCTAssertTrue(signal.take("chat"), "A later wake is kept for the next screen")
    }

    func testWakesFromAnEndedConnectionDoNotCarryOver() async {
        let signal = NativeWakeSignal()
        signal.record("chat")
        signal.discardPending()
        let woken = await signal.wait("chat", until: .now + .milliseconds(50))
        XCTAssertFalse(woken, "A wake from the previous connection is not counted for the next one")
    }
}

/// Observable contract: the share sheet reaches the computer the way the app
/// does (a home address only once it proves the pinned key, the relay only
/// through its sealed connection, never in plaintext), and a share whose
/// answer was lost is finished after the sheet restarts with the same message
/// and uploads instead of being sent twice. A new share of the same content
/// is a separate share.
@MainActor final class ShareTransportTests: XCTestCase {
    private var journal: URL!

    override func setUp() async throws {
        journal = FileManager.default.temporaryDirectory.appendingPathComponent("share-transport-\(UUID().uuidString)", isDirectory: true)
        ShareTransportURLProtocol.reset()
    }

    override func tearDown() async throws { try? FileManager.default.removeItem(at: journal) }

    private func credential(host: String) throws -> Credential {
        try JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"first","deviceId":"phone","csrfToken":"csrf","hostInstallationId":"\#(host)"}"#.utf8))
    }

    nonisolated(unsafe) fileprivate static let catalog: [String: Any] = [
        "projects": [["id": "p1", "name": "Wonder", "isIncluded": true, "isPinned": false, "rootsRevision": 3, "lastFamily": "codex",
                      "folders": [["id": "f1", "path": "/Users/fixture/Wonder", "name": "Wonder", "isPrimary": true, "isAvailable": true]],
                      "createdAt": "2026-01-01T00:00:00Z"]],
        "families": [[String: Any]](), "pinned": [[String: Any]](),
    ]
    nonisolated(unsafe) fileprivate static let page: [String: Any] = [
        "threads": [["reference": "wonder:t1", "conversationId": "c-1", "title": "Share sheet", "family": "codex",
                     "updatedAt": 1_760_000_000, "isPinned": false, "hasUnread": false, "isWorking": false]],
        "nextCursor": NSNull(), "partial": [[String: Any]](),
    ]

    private func model(_ saved: SavedConnection, api: PairingAPI, sign: SealedRPC.Sign? = nil) throws -> ShareModel {
        let data = try JSONEncoder().encode(SavedConnections(legacy: saved))
        return ShareModel(api: api, sign: sign, journal: ShareJournal(directory: journal), readConnections: { data }, finish: { _ in })
    }

    private func wait(_ condition: () -> Bool) async throws {
        for _ in 0..<500 where !condition() { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(condition(), "timed out")
    }

    private func png() -> NSItemProvider {
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        let data = UIGraphicsImageRenderer(size: CGSize(width: 40, height: 30), format: format).pngData { context in
            UIColor.systemTeal.setFill(); context.fill(CGRect(x: 0, y: 0, width: 40, height: 30))
        }
        let provider = NSItemProvider(item: data as NSData, typeIdentifier: UTType.png.identifier)
        provider.suggestedName = "Mockup.png"
        return provider
    }

    func testARelayOnlyComputerIsReachedThroughTheSealedConnectionAndNeverInPlaintext() async throws {
        let relay = ConnectionRoute(kind: .relay, origin: "https://r-abcdefghijklmnopqrstuvwxyz.getcupola.app")
        let computer = RelayStandIn(origin: relay.origin, serves: { $0.hasPrefix("/api/v1/projects") }, refused: [])
        computer.bodies = ["/api/v1/projects": Self.catalog, "/api/v1/projects/p1/threads": Self.page]
        let saved = try SavedConnection(routes: [relay], credential: credential(host: "relay-check-studio"), hostName: "Studio", hostKeyPin: computer.pin)
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ShareTransportURLProtocol.self]
        let api = PairingAPI(configuration: configuration, sealedSocket: { _ in computer.connect() })
        let key = P256.Signing.PrivateKey()
        let share = try model(saved, api: api, sign: { @Sendable data in try key.signature(for: data).rawRepresentation.base64URL })
        share.start(providers: [])
        try await wait { !share.threads.isEmpty || share.destinationError != nil }
        XCTAssertNil(share.destinationError)
        XCTAssertEqual(share.threads.map(\.title), ["Share sheet"])
        XCTAssertTrue(computer.requested.contains("/api/v1/projects"), "\(computer.requested)")
        XCTAssertEqual(computer.sessions, ["first"], "the session went inside the sealed connection")
        XCTAssertTrue(ShareTransportURLProtocol.requests.isEmpty, "nothing was sent to the relay in plaintext")

        // Without the device key the relay is never contacted.
        ShareTransportURLProtocol.reset()
        let unsigned = try model(saved, api: PairingAPI(configuration: configuration))
        unsigned.start(providers: [])
        try await wait { unsigned.destinationError != nil }
        XCTAssertTrue(ShareTransportURLProtocol.requests.isEmpty)
        share.cancel(); unsigned.cancel()
    }

    func testTheHomeAddressIsUsedOnlyWhenItProvesThePinnedKey() async throws {
        let pin = RouteProofURLProtocol.reset()
        let tailscale = ConnectionRoute(kind: .tailscale, origin: "https://studio.tail1234.ts.net")
        let home = ConnectionRoute(kind: .home, origin: "https://192.168.1.20:3778")
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [RouteProofURLProtocol.self]
        let api = PairingAPI(configuration: configuration)
        // Listed after Tailscale, the home address still wins on the local network.
        let saved = try SavedConnection(routes: [tailscale, home], credential: credential(host: "studio"), hostKeyPin: pin)
        let chosen = try await ShareModel.connect(saved, api: api, trust: HomeNetworkTrust(), sign: nil)
        XCTAssertEqual(chosen.origin, home.origin)
        XCTAssertEqual(chosen.routes, saved.routes, "the choice is not a new saved connection")
        XCTAssertTrue(RouteProofURLProtocol.requests.contains { $0.url?.host == "192.168.1.20" && $0.url?.path == "/api/v1/host/proof" })
        XCTAssertTrue(RouteProofURLProtocol.requests.allSatisfy { $0.value(forHTTPHeaderField: "Cookie") == nil })

        // Another key at every address: nothing is chosen and the sheet says why.
        _ = RouteProofURLProtocol.reset()
        let other = try SavedConnection(routes: [tailscale, home], credential: credential(host: "studio"), hostName: "Studio", hostKeyPin: "other-key")
        do { _ = try await ShareModel.connect(other, api: api, trust: HomeNetworkTrust(), sign: nil); XCTFail("another key was accepted") }
        catch PairingFailure.wrongHost {
            XCTAssertEqual(ShareModel.message(for: PairingFailure.wrongHost, connection: other),
                           "Studio couldn’t prove its security key, so nothing was sent. Open \(ShareModel.appName) to pair it again.")
        }
        XCTAssertTrue(RouteProofURLProtocol.requests.allSatisfy { $0.value(forHTTPHeaderField: "Cookie") == nil })
    }

    func testALostReceiptIsFinishedAfterARestartWithTheSameMessageAndUploads() async throws {
        let saved = try SavedConnection(routes: [ConnectionRoute(kind: .tailscale, origin: "https://studio.tail1234.ts.net")],
                                        credential: credential(host: "studio"), hostName: "Studio")
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ShareTransportURLProtocol.self]
        ShareTransportURLProtocol.reset(dropMessages: true)

        let first = try model(saved, api: PairingAPI(configuration: configuration))
        first.start(providers: [png()])
        try await wait { !first.loadingItems && !first.threads.isEmpty }
        XCTAssertEqual(first.files.count, 1)
        first.choose(.existing("thread:wonder:t1"))
        first.note = "Look at this"
        await first.send()
        XCTAssertNotNil(first.sendError, "the message's answer was lost")
        let lost = try XCTUnwrap(ShareTransportURLProtocol.messages.first)
        XCTAssertEqual(ShareTransportURLProtocol.uploads, 1)

        // The sheet restarts: the share comes back as a recovery item, not a new send.
        ShareTransportURLProtocol.dropMessages = false
        let second = try model(saved, api: PairingAPI(configuration: configuration))
        second.start(providers: [])
        try await wait { !second.loadingItems && !second.threads.isEmpty }
        let recovery = try XCTUnwrap(second.recovery)
        XCTAssertEqual(recovery.id, lost.clientMessageId)
        await second.finishRecovery()
        XCTAssertNil(second.recoveryError)
        XCTAssertNil(second.recovery)
        XCTAssertEqual(ShareTransportURLProtocol.messages.map(\.clientMessageId), [lost.clientMessageId, lost.clientMessageId])
        XCTAssertEqual(ShareTransportURLProtocol.messages.last?.attachmentIds, lost.attachmentIds)
        XCTAssertEqual(ShareTransportURLProtocol.uploads, 1, "the verified upload was reused")
        XCTAssertTrue(ShareJournal(directory: journal).load().isEmpty, "retired after the confirmed receipt")

        // Sharing the same words again on purpose is a new message.
        second.choose(.existing("thread:wonder:t1"))
        second.note = "Look at this"
        await second.send()
        XCTAssertNil(second.sendError)
        XCTAssertEqual(ShareTransportURLProtocol.messages.count, 3)
        XCTAssertNotEqual(ShareTransportURLProtocol.messages.last?.clientMessageId, lost.clientMessageId)
    }

    // D-01: the computer ends every session an hour after it starts and only
    // the app renewed it, so a share an hour after the app was last in front
    // refused and dropped the item. The sheet now renews with the device key.
    func testAnExpiredSavedSessionIsRenewedInTheSheetAndTheShareIsSent() async throws {
        let ended = try JSONDecoder().decode(Credential.self, from: Data(
            #"{"sessionToken":"first","deviceId":"phone","csrfToken":"csrf","hostInstallationId":"studio","expiresAtMs":1000}"#.utf8))
        let saved = try SavedConnection(routes: [ConnectionRoute(kind: .tailscale, origin: "https://studio.tail1234.ts.net")],
                                        credential: ended, hostName: "Studio")
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ShareTransportURLProtocol.self]
        ShareTransportURLProtocol.reset()
        let key = P256.Signing.PrivateKey()
        let share = try model(saved, api: PairingAPI(configuration: configuration),
                              sign: { @Sendable data in try key.signature(for: data).rawRepresentation.base64URL })
        share.start(providers: [])
        try await wait { !share.threads.isEmpty || share.destinationError != nil }
        XCTAssertNil(share.destinationError)
        XCTAssertEqual(share.threads.map(\.title), ["Share sheet"])
        let paths = ShareTransportURLProtocol.requests.compactMap { $0.url?.path }
        XCTAssertEqual(paths.prefix(2), ["/api/v1/pairing/session/refresh-challenge", "/api/v1/pairing/session"])
        let projects = try XCTUnwrap(ShareTransportURLProtocol.requests.first { $0.url?.path == "/api/v1/projects" })
        XCTAssertEqual(projects.value(forHTTPHeaderField: "Cookie"), "__Host-wonder_session=renewed", "the renewed session was used")
        share.choose(.existing("thread:wonder:t1"))
        share.note = "After an hour"
        await share.send()
        XCTAssertNil(share.sendError)
        XCTAssertEqual(ShareTransportURLProtocol.messages.count, 1)
        share.cancel()
    }

}

/// The computer for the share sheet's ordinary routes. It records every
/// request; `dropMessages` loses the message's answer after the computer took it.
private final class ShareTransportURLProtocol: URLProtocol, @unchecked Sendable {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var recorded: [URLRequest] = []
    nonisolated(unsafe) private static var sent: [SendRequest] = []
    nonisolated(unsafe) private static var uploaded = 0
    nonisolated(unsafe) private static var dropping = false
    static func reset(dropMessages: Bool = false) {
        lock.withLock { recorded = []; sent = []; uploaded = 0; dropping = dropMessages }
    }
    static var requests: [URLRequest] { lock.withLock { recorded } }
    static var messages: [SendRequest] { lock.withLock { sent } }
    static var uploads: Int { lock.withLock { uploaded } }
    static var dropMessages: Bool {
        get { lock.withLock { dropping } }
        set { lock.withLock { dropping = newValue } }
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}
    override func startLoading() {
        var body = request.httpBody
        if body == nil, let stream = request.httpBodyStream {
            stream.open(); defer { stream.close() }
            var data = Data(), buffer = [UInt8](repeating: 0, count: 65536)
            while stream.hasBytesAvailable { let count = stream.read(&buffer, maxLength: buffer.count); if count <= 0 { break }; data.append(buffer, count: count) }
            body = data
        }
        Self.lock.withLock { Self.recorded.append(request) }
        let path = request.url?.path ?? ""
        let fields = body.flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] } ?? [:]
        switch path {
        case "/api/v1/pairing/session/refresh-challenge":
            let now = UInt64(Date().timeIntervalSince1970 * 1000)
            finish(["challengeId": "ch-1", "deviceId": "phone", "nonce": "n", "origin": "https://studio.tail1234.ts.net", "hostInstallationId": "studio",
                    "offerId": "o", "issuedAtMs": now, "expiresAtMs": now + 60_000])
        case "/api/v1/pairing/session":
            finish(["sessionToken": "renewed", "deviceId": "phone", "csrfToken": "csrf2", "hostInstallationId": "studio",
                    "expiresAtMs": UInt64(Date().timeIntervalSince1970 * 1000) + 3_600_000])
        case "/api/v1/projects": finish(ShareTransportTests.catalog)
        case "/api/v1/projects/p1/threads": finish(ShareTransportTests.page)
        case "/api/v1/conversations/c-1/files":
            let attempt: Int = Self.lock.withLock { Self.uploaded += 1; return Self.uploaded }
            let data = Data(base64Encoded: fields["contentBase64"] as? String ?? "") ?? Data()
            finish(["id": "file-\(attempt)", "name": fields["name"] ?? "", "mimeType": fields["mimeType"] ?? "", "byteSize": data.count,
                    "sha256": ConversationFile.digest(data), "state": "available", "updatedAt": "now"])
        case "/api/v1/conversations/c-1/messages":
            guard let body, let message = try? JSONDecoder().decode(SendRequest.self, from: body) else { finish([:], status: 400); return }
            let drop: Bool = Self.lock.withLock { Self.sent.append(message); return Self.dropping }
            if drop { client?.urlProtocol(self, didFailWithError: URLError(.networkConnectionLost)); return }
            finish(["clientMessageId": message.clientMessageId, "wonderMessageId": "m-\(message.clientMessageId)",
                    "bodySha256": ConversationFile.digest(Data(message.body.utf8)), "conversationId": "c-1", "deliveryState": "accepted"])
        default: finish([:], status: 404)
        }
    }

    private func finish(_ value: Any, status: Int = 200) {
        let data = (try? JSONSerialization.data(withJSONObject: value)) ?? Data("{}".utf8)
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: data)
        client?.urlProtocolDidFinishLoading(self)
    }
}
#endif

/// Composer feedback: a failed draft write keeps its reason and Send retries
/// it; an unreadable draft clears once a reopen reads it; uploads report per
/// file; a Stop that can't reach the computer never claims the reply stopped.
extension WonderDiagnosticsTests {
    // Regression: after a failed save Send stayed grey while the reason faded,
    // and only editing the draft retried the write.
    @MainActor func testFailedDraftSaveKeepsItsReasonAndSendRetriesTheWrite() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        let store = ReadStore(root: root, host: "camera-unit-host", device: "camera-unit-device")
        let failure = "This draft could not be saved. Free some storage before sending."
        let messages = "/api/v1/conversations/\(chat.id)/messages"
        model.editDraft("", chat: chat.id) // Materialize the lazily created store.
        try FileManager.default.setAttributes([.posixPermissions: 0o500], ofItemAtPath: store.directory.path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: store.directory.path) }
        model.editDraft("Send me when storage frees up", chat: chat.id)
        XCTAssertEqual(model.composerErrors[chat.id], failure)
        XCTAssertEqual(model.owedComposerWrite(chat.id), .intent)
        XCTAssertTrue(model.canSend(chat), "A failed save leaves Send available to retry it, with the reason showing")

        model.dismissComposerNotice(chat.id)
        XCTAssertNil(model.composerErrors[chat.id])
        await model.send(chat)
        XCTAssertEqual(model.composerErrors[chat.id], failure, "Send retried the write; the same failure shows again after a dismissal")
        XCTAssertEqual(model.owedComposerWrite(chat.id), .intent)
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: messages).isEmpty, "Nothing is sent while the draft is unsaved")

        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: store.directory.path)
        await model.send(chat)
        XCTAssertNil(model.composerErrors[chat.id])
        XCTAssertNil(model.owedComposerWrite(chat.id))
        let bodies = MessageRecoveryURLProtocol.bodies(path: messages)
        XCTAssertEqual(bodies.count, 1)
        XCTAssertEqual(try JSONDecoder().decode(SendRequest.self, from: XCTUnwrap(bodies.first)).body, "Send me when storage frees up")
    }

    // Start over after an unreadable draft keeps the unreadable bytes aside
    // (renamed, unread) and only then opens an empty draft.
    @MainActor func testStartOverAfterAnUnreadableDraftKeepsACopy() throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat) = try recoveryProject(root: root)
        let store = ReadStore(root: root, host: "camera-unit-host", device: "camera-unit-device")
        let unreadable = Data("not a saved message".utf8)
        try store.saveIntent(unreadable, conversation: chat.id)
        model.typeDraft("Lost?", chat: chat.id)
        XCTAssertTrue(model.draftUnreadable(chat.id))

        XCTAssertTrue(model.startOverUnreadableDraft(chat.id))
        XCTAssertFalse(model.draftUnreadable(chat.id))
        XCTAssertNil(model.composerErrors[chat.id])
        XCTAssertEqual(model.composers[chat.id]?.draft, "")
        XCTAssertNil(try store.loadIntent(conversation: chat.id), "The unreadable file left its place")
        let kept = try FileManager.default.contentsOfDirectory(at: store.directory, includingPropertiesForKeys: nil)
            .filter { $0.lastPathComponent.hasPrefix("intent-") }
            .compactMap { try? Data(contentsOf: $0) }
        XCTAssertTrue(kept.contains(unreadable), "A copy of the unreadable message is kept")
        model.editDraft("A fresh start", chat: chat.id)
        XCTAssertNil(model.composerErrors[chat.id])
        XCTAssertEqual(try store.loadComposer(conversation: chat.id).draft, "A fresh start")
    }

    @MainActor func testVideoUploadSurvivesNavigationCancelsAndRetriesWithFreshIdentity() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root); MessageRecoveryURLProtocol.releaseHeld() }
        let (model, chat) = try recoveryProject(root: root)
        model.editDraft("Review this video", chat: chat.id)
        let bytes = Data([0, 0, 0, 24]) + Data("ftypisom".utf8) + Data(repeating: 0, count: 260_000)
        let file = try StagedFile(name: "Clip.mp4", mimeType: "video/mp4", data: bytes)
        var draft = try XCTUnwrap(model.composers[chat.id]); draft.stagedFiles = [file]
        model.composers[chat.id] = draft
        let base = "/api/v1/conversations/\(chat.id)/files/uploads"
        func receipt(_ id: String, _ offset: Int, complete: Bool = false) throws -> Data {
            var value: [String: Any] = ["uploadId": id, "receivedBytes": offset, "totalBytes": bytes.count,
                "maxChunkBytes": 131072, "state": complete ? "complete" : "uploading"]
            if complete { value["file"] = ["id": "uploaded-video", "name": file.name, "mimeType": file.mimeType,
                "byteSize": bytes.count, "sha256": file.sha256, "state": "available", "updatedAt": "2026-10-28"] }
            return try JSONSerialization.data(withJSONObject: value)
        }
        MessageRecoveryURLProtocol.enqueue(path: base, body: try receipt(file.id, 0))
        MessageRecoveryURLProtocol.enqueue(path: "PATCH " + base + "/" + file.id, body: try receipt(file.id, 131072))
        MessageRecoveryURLProtocol.hold(path: base + "/" + file.id, method: "PATCH", after: 1)
        MessageRecoveryURLProtocol.enqueue(path: "DELETE " + base + "/" + file.id, body: Data("{}".utf8))
        let upload = Task { await model.retryUpload(chat) }
        defer { upload.cancel() }
        try await waitForRecoveryRequests(path: base + "/" + file.id, count: 2)
        XCTAssertEqual(model.uploadProgress[chat.id]?.receivedBytes, 131072)
        model.selectedChat = nil
        XCTAssertTrue(model.uploading.contains(chat.id), "Navigating away must not end the upload")
        model.cancelUpload(chat)
        await upload.value
        XCTAssertFalse(model.uploading.contains(chat.id))
        XCTAssertNotNil(model.uploadFailures[chat.id])
        let saved = try ReadStore(root: root, host: "camera-unit-host", device: "camera-unit-device").loadComposer(conversation: chat.id)
        let retained = try XCTUnwrap(saved.stagedFiles?.first)
        XCTAssertEqual(retained.id, file.id)
        XCTAssertEqual(retained.sha256, file.sha256)
        let retryID = try XCTUnwrap(retained.uploadID)
        XCTAssertNotEqual(retryID, file.id)
        MessageRecoveryURLProtocol.releaseHeld()
        MessageRecoveryURLProtocol.enqueue(path: base, body: try receipt(retryID, 131072))
        MessageRecoveryURLProtocol.enqueue(path: base + "/" + retryID, body: try receipt(retryID, bytes.count))
        MessageRecoveryURLProtocol.enqueue(path: base + "/" + retryID + "/complete", body: try receipt(retryID, bytes.count, complete: true))
        await model.retryUpload(chat)
        XCTAssertEqual(model.composers[chat.id]?.stagedFiles?.first?.uploaded?.id, "uploaded-video")
        XCTAssertNil(model.uploadFailures[chat.id])
        XCTAssertNil(model.uploadProgress[chat.id])
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: "/api/v1/conversations/\(chat.id)/messages").isEmpty)
    }

    // Regression: a waiting message whose promotion failed to save only set
    // the notice, so the next unrelated save cleared it and nothing retried it.
    @MainActor func testFailedWaitingMessageSaveStaysOwedUntilTheDrainSavesIt() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let (model, chat, waiting) = try await queueTwoOffline(root: root)
        let saved = Self.cameraSavedConnection()
        let store = ReadStore(root: root, host: saved.credential.hostInstallationId, device: saved.storageDeviceId)
        let failure = "A waiting message couldn’t be saved. Free some storage, then reopen this chat to try again."
        for _ in 0..<3 { MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/recovery-chat/history/activity", body: Self.outboxActivity) }
        try FileManager.default.setAttributes([.posixPermissions: 0o500], ofItemAtPath: store.directory.path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: store.directory.path) }
        model.macConnected = true
        try await waitForOutbox { !model.isDrainingOutbox && model.composerErrors[chat.id] == failure }
        XCTAssertTrue(model.owesQueueWrite(chat.id))
        XCTAssertTrue(try sentRequests().isEmpty, "Nothing is sent before its promotion is saved")

        // An unrelated whole-intent save succeeds; the waiting message's change is still owed.
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: store.directory.path)
        model.editDraft("", chat: chat.id)
        XCTAssertEqual(model.composerErrors[chat.id], failure, "An unrelated save does not clear the waiting message's reason")
        XCTAssertTrue(model.owesQueueWrite(chat.id))

        // The retry redoes it through the drain; its own save clears it.
        let path = "/api/v1/conversations/recovery-chat"
        MessageRecoveryURLProtocol.enqueue(path: path, body: try outboxSnapshot([(waiting[0].id, waiting[0].body)], sequence: 2))
        MessageRecoveryURLProtocol.enqueue(path: path, body: try outboxSnapshot(waiting.map { ($0.id, $0.body) }, sequence: 3))
        model.retryOwedComposerWrite(chat.id)
        try await waitForOutbox { !model.isDrainingOutbox && model.composers[chat.id]?.outbox == nil && model.composers[chat.id]?.pending == nil }
        XCTAssertFalse(model.owesQueueWrite(chat.id))
        XCTAssertNil(model.composerErrors[chat.id])
        XCTAssertEqual(try sentRequests().map(\.clientMessageId), waiting.map(\.id))
    }
}

// Shell recovery: each failure the Chats list or New chat can meet shows where
// the owner looks, with the action that recovers from it.
extension WonderDiagnosticsTests {
    /// Saved computers in memory whose reads and writes can be made to fail.
    @MainActor private final class FailingComputerStorage {
        var values: [String: Data] = [:]
        var locked = false
        var failWrites = false
        var reads = 0
        var storage: ConnectionLibrary.Storage {
            ConnectionLibrary.Storage(read: { key in
                self.reads += 1
                if self.locked { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
                return self.values[key]
            }, save: { data, account in
                if self.failWrites { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
                self.values[account] = data
            }, forgetLegacy: {})
        }
    }

    // Contract: saved computers that can't be read while the device is locked
    // open on their own once a scene returns to the foreground; ones this
    // version can't read are not retried, since only removing them helps (B-258).
    @MainActor func testLockedSavedComputersOpenWhenTheAppReturnsAndUnreadableOnesAreNotRetried() throws {
        let storage = FailingComputerStorage()
        var paired = SavedConnections()
        paired.save(Self.cameraSavedConnection(hostID: "locked-launch-host"))
        storage.values["connections-v1"] = try JSONEncoder().encode(paired)
        storage.locked = true
        let library = ConnectionLibrary(storage: storage.storage, widgetPublisher: { _, _ in true })
        XCTAssertEqual(library.savedComputersFailure, .locked)
        XCTAssertFalse(library.loaded)
        XCTAssertTrue(library.paired.isEmpty, "Nothing is shown as paired, so New chat must not offer first-run setup")
        XCTAssertTrue(SavedComputersFailure.locked.message.contains("unlock"))

        library.setScene(UUID(), phase: .active)
        XCTAssertEqual(library.savedComputersFailure, .locked, "Still locked: the failure stays")
        storage.locked = false
        library.setScene(UUID(), phase: .active)
        XCTAssertNil(library.savedComputersFailure)
        XCTAssertNil(library.error)
        XCTAssertTrue(library.loaded)
        XCTAssertEqual(library.paired.map(\.credential.hostInstallationId), ["locked-launch-host"])

        let unreadable = FailingComputerStorage()
        unreadable.values["connections-v1"] = Data("not saved computers".utf8)
        let broken = ConnectionLibrary(storage: unreadable.storage, widgetPublisher: { _, _ in true })
        XCTAssertEqual(broken.savedComputersFailure, .unreadable)
        let reads = unreadable.reads
        broken.setScene(UUID(), phase: .active)
        XCTAssertEqual(unreadable.reads, reads, "Retrying can't read a format this version doesn't know")
        XCTAssertEqual(broken.savedComputersFailure, .unreadable)
    }

}

#if os(iOS)
/// Observable contract: an open tool panel keeps the reader's selection and
/// scroll position when its output grows, is replaced, shrinks or changes
/// font; a reader page change starts at the top. Regression: replacing
/// `attributedText` on every update dropped both.
@MainActor final class ToolTextRendererTests: XCTestCase {
    private var font: UIFont { .monospacedSystemFont(ofSize: 16, weight: .regular) }
    private func lines(_ range: Range<Int>, suffix: String = "") -> String {
        range.map { "Output line \($0)\(suffix)" }.joined(separator: "\n")
    }
    private func panel() -> UITextView {
        let view = UITextView(usingTextLayoutManager: false)
        view.isEditable = false; view.isSelectable = true
        view.frame = CGRect(x: 0, y: 0, width: 360, height: 260)
        return view
    }

}
#endif
/// Contract: a running agent task's transcript re-reads its newest page only
/// while Cupola is in front, regroups only when that page changed, and keeps
/// loaded older activity while the newest page updates in place.
final class SubagentTranscriptPagesTests: XCTestCase {
    private func page(cursor: String?, turns: [ReadTurn], sequence: UInt64 = 5) -> ConversationSnapshot {
        ConversationSnapshot(conversationId: "child", hostEpoch: "epoch", lastSequence: sequence, messages: [], assistantMessages: [],
                             thread: ThreadProjection(nextCursor: cursor, hydrated: true, turns: turns), messageRecords: nil)
    }
    private func command(_ id: String, state: String, output: String) -> ReadItem {
        let json: [String: Any] = ["id": id, "type": "commandExecution", "state": state, "createdAt": "1000",
                                   "payload": ["command": "swift test", "output": output]]
        return try! JSONDecoder().decode(ReadItem.self, from: JSONSerialization.data(withJSONObject: json))
    }

}

#if os(iOS)
// Request-level regression owners for B-044/B-142/B-140/B-405. These drive
// the real frame consumer and model reads against a synthetic host only.
extension WonderDiagnosticsTests {
    private func refreshScopeEvent(_ sequence: UInt64, chat: String?, body: EventBody) -> ReplayEvent {
        ReplayEvent(eventId: "refresh-\(sequence)", hostEpoch: "epoch", sequence: sequence,
            occurredAt: "2026-10-27T12:00:00Z", conversationId: chat, event: body, turnId: "turn", itemId: "item")
    }

    @MainActor func testRefreshCoordinatorQueueEditReadsAfterAPreMutationGET() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let (model, chat) = try recoveryProject(root: root, hostID: "refresh-scope-" + UUID().uuidString)
        defer { model.setForeground(false); model.projects.forgetCache(); MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        let item = try JSONDecoder().decode(QueuedMessage.self, from: Data(#"{"id":"queued-1","clientMessageId":"client-1","body":"Before edit","revision":1,"attachmentIds":[]}"#.utf8))
        let path = "/api/v1/conversations/\(chat.id)/queue"
        MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONEncoder().encode([item]))
        MessageRecoveryURLProtocol.hold(path: path, method: "GET")
        let older = Task { try await model.loadQueue(chat) }
        try await waitForRecoveryRequests(path: path)
        MessageRecoveryURLProtocol.enqueue(path: path + "/queued-1", body: Data("{}".utf8))
        MessageRecoveryURLProtocol.enqueue(path: path, body: Data(#"[{"id":"queued-1","clientMessageId":"client-1","body":"After edit","revision":2,"attachmentIds":[]}]"#.utf8))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/conversations/\(chat.id)", body: try JSONEncoder().encode(try XCTUnwrap(model.snapshots[chat.id])))
        let edit = Task { try await model.changeQueue(chat, item: item, body: "After edit") }
        try await waitForRecoveryRequests(path: path + "/queued-1")
        MessageRecoveryURLProtocol.releaseHeld()
        try await older.value; try await edit.value
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count, 2)
        XCTAssertEqual(model.queues[chat.id]?.first?.body, "After edit", "A mutation's reconciliation cannot join the GET that started before it")
        XCTAssertEqual(model.queues[chat.id]?.first?.revision, 2)
    }

    @MainActor func testRefreshCoordinatorResumeCancelReadsAfterAPreMutationDetailGET() async throws {
        for status in [200, 404] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            let (model, chat) = try recoveryProject(root: root, hostID: "refresh-scope-" + UUID().uuidString)
            defer { model.setForeground(false); model.projects.forgetCache(); MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
            let path = "/api/v1/project-conversations/\(chat.id)", cancelPath = "/api/v1/conversations/\(chat.id)/usage-resume"
            let fresh = try JSONEncoder().encode(try XCTUnwrap(model.projects.details[chat.id]))
            var scheduled = try XCTUnwrap(JSONSerialization.jsonObject(with: fresh) as? [String: Any])
            scheduled["resumesAt"] = "2099-10-27T12:00:00Z"
            MessageRecoveryURLProtocol.enqueue(path: path, body: try JSONSerialization.data(withJSONObject: scheduled))
            MessageRecoveryURLProtocol.hold(path: path, method: "GET")
            let older = Task { try await model.projects.loadDetail(chat.id) }
            try await waitForRecoveryRequests(path: path)
            MessageRecoveryURLProtocol.enqueue(path: cancelPath, method: "DELETE", status: status)
            MessageRecoveryURLProtocol.enqueue(path: path, body: fresh)
            let cancel = Task { await model.projects.cancelResume(chat.id) }
            try await waitForRecoveryRequests(path: cancelPath)
            MessageRecoveryURLProtocol.releaseHeld()
            _ = try await older.value; await cancel.value
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: path, includingEmpty: true).count, 2)
            XCTAssertNil(model.projects.details[chat.id]?.resumesAt, "A pre-DELETE read cannot restore Resumes at (DELETE \(status))")
            XCTAssertNil(model.projects.resumeFailures[chat.id])
        }
    }

    @MainActor func testRefreshCoordinatorJoinedNativeImportRecordsRevisionOnlyAfterSuccessfulCanonicalGET() async throws {
        for outcome in ["completed", "importFailed", "getFailed", "getFailedAfterOlderRead"] {
            MessageRecoveryURLProtocol.reset()
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            let (model, chat) = try recoveryProject(root: root, hostID: "refresh-scope-" + UUID().uuidString)
            defer { model.setForeground(false); model.projects.forgetCache(); MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
            let history = "/api/v1/conversations/\(chat.id)", importPath = history + "/history/refresh", activityPath = history + "/history/activity"
            let detailPath = "/api/v1/project-conversations/\(chat.id)"
            let detail = try JSONEncoder().encode(try XCTUnwrap(model.projects.details[chat.id]))
            let snapshot = try JSONEncoder().encode(try XCTUnwrap(model.snapshots[chat.id]))
            let activity = Data(#"{"latestTurnId":"settled-turn","latestTurnStatus":"completed","runningElsewhere":false,"latestTurnItems":0,"historyRevision":"10:1"}"#.utf8)
            _ = await model.nativeActivityChanged(chat) // Consume recoveryProject's initial activity reply.
            var older: Task<Void, Never>?
            if outcome == "getFailedAfterOlderRead" {
                MessageRecoveryURLProtocol.enqueue(path: history, body: snapshot)
                MessageRecoveryURLProtocol.hold(path: history, method: "GET")
                older = Task { await model.refreshConversation(chat) }
                try await waitForRecoveryRequests(path: history)
            }
            MessageRecoveryURLProtocol.enqueue(path: detailPath, body: detail)
            MessageRecoveryURLProtocol.enqueue(path: importPath, method: "POST",
                body: Data((outcome == "importFailed" ? #"{"state":"failed"}"# : #"{"state":"completed"}"#).utf8))
            if outcome != "importFailed" {
                MessageRecoveryURLProtocol.enqueue(path: history, status: outcome.hasPrefix("getFailed") ? 503 : 200, body: snapshot)
            }
            MessageRecoveryURLProtocol.hold(path: importPath, method: "POST")
            // An event started this import without an opening revision.
            let eventRead = Task { await model.reloadNativeHistory(chat) }
            try await waitForRecoveryRequests(path: importPath)
            MessageRecoveryURLProtocol.enqueue(path: activityPath, body: activity)
            let opening = Task { await model.reloadNativeHistoryIfBehind(chat) }
            try await waitUntil { MessageRecoveryURLProtocol.bodies(path: activityPath, includingEmpty: true).count == 2 }
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: importPath).count, 1)
            if older != nil {
                // Complete the import first. Its canonical GET must wait for
                // the older GET, whose success cannot hide the fresh GET's failure.
                MessageRecoveryURLProtocol.releaseNewestHeld()
                try await Task.sleep(for: .milliseconds(100))
            }
            MessageRecoveryURLProtocol.releaseHeld()
            await older?.value
            await eventRead.value; await opening.value
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: importPath).count, 1, "The opening joined the event's import")
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: history, includingEmpty: true).count,
                outcome == "importFailed" ? 0 : older == nil ? 1 : 2)
            // Reopen at that same revision. Failed imports/GETs must retry;
            // a completed joined read must not import again.
            MessageRecoveryURLProtocol.enqueue(path: activityPath, body: activity)
            MessageRecoveryURLProtocol.enqueue(path: detailPath, body: detail)
            MessageRecoveryURLProtocol.enqueue(path: importPath, method: "POST", body: Data(#"{"state":"completed"}"#.utf8))
            MessageRecoveryURLProtocol.enqueue(path: history, body: snapshot)
            await model.reloadNativeHistoryIfBehind(chat)
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: importPath).count, outcome == "completed" ? 1 : 2, outcome)
        }
    }

    @MainActor func testRefreshCoordinatorNativeImportKeepsCanonicalGETAndSuppressesImportInvalidationReads() async throws {
        MessageRecoveryURLProtocol.reset()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let (model, chat) = try recoveryProject(root: root, hostID: "refresh-scope-" + UUID().uuidString)
        defer { model.setForeground(false); model.projects.forgetCache(); MessageRecoveryURLProtocol.releaseHeld(); try? FileManager.default.removeItem(at: root) }
        model.presentConversation(chat)
        try model.diagnosticConsumeRefreshEvents([], startingAt: try XCTUnwrap(model.snapshots[chat.id]))
        let detail = try XCTUnwrap(model.projects.details[chat.id])
        let history = "/api/v1/conversations/\(chat.id)", importPath = "/api/v1/conversations/\(chat.id)/history/refresh"
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/\(chat.id)", body: try JSONEncoder().encode(detail))
        MessageRecoveryURLProtocol.enqueue(path: importPath, method: "POST", body: Data(#"{"state":"completed"}"#.utf8))
        MessageRecoveryURLProtocol.hold(path: importPath, method: "POST")
        let imported = Data(#"{"conversationId":"recovery-chat","hostEpoch":"epoch","lastSequence":20,"messages":[],"assistantMessages":[],"thread":{"hydrated":true,"turns":[{"id":"imported","status":"completed","items":[{"id":"reply","type":"agentMessage","state":"completed","text":"Imported reply","createdAt":"2026-10-27T12:00:00Z"}]}]}}"#.utf8)
        MessageRecoveryURLProtocol.enqueue(path: history, body: imported)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/project-conversations/\(chat.id)", body: try JSONEncoder().encode(detail))
        MessageRecoveryURLProtocol.enqueue(path: importPath, method: "POST", body: Data(#"{"state":"completed"}"#.utf8))
        MessageRecoveryURLProtocol.enqueue(path: history, body: imported)
        model.requestNativeHistoryRefresh(chat)
        try await waitForRecoveryRequests(path: importPath)
        let firstImport = ContinuousClock.now
        for _ in 0..<20 { model.requestNativeHistoryRefresh(chat) }
        let invalidation = EventBody(type: "activity", category: "thread_item_upsert", state: "updated",
            detail: #"{"turnId":"imported","itemId":"reply","state":"completed","historyRefresh":true,"lifecycleAuthority":2,"itemType":"agentMessage","item":{"id":"reply","type":"agentMessage"}}"#)
        try model.diagnosticConsumeRefreshEvents((UInt64(2)...10).map { refreshScopeEvent($0, chat: chat.id, body: invalidation) })
        try await Task.sleep(for: .milliseconds(400))
        XCTAssertTrue(MessageRecoveryURLProtocol.bodies(path: history, includingEmpty: true).isEmpty,
                      "Import-generated invalidations must wait for the import's canonical GET")
        MessageRecoveryURLProtocol.releaseHeld()
        try await waitUntil { model.snapshots[chat.id]?.thread.turns?.last?.id == "imported" && !model.nativeHistoryRefreshing.contains(chat.id) }
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: importPath).count, 1)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: history, includingEmpty: true).count, 1)
        XCTAssertEqual(model.snapshots[chat.id]?.rows(author: "Claude").last?.text, "Imported reply")
        // Replay catches up to the GET, without initiating another download.
        try model.diagnosticConsumeRefreshEvents((UInt64(11)...20).map { refreshScopeEvent($0, chat: chat.id, body: invalidation) })
        try await Task.sleep(for: .milliseconds(350))
        XCTAssertFalse(model.cachedConversationIds.contains(chat.id))
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: history, includingEmpty: true).count, 1)
        try await waitUntil { MessageRecoveryURLProtocol.bodies(path: importPath).count == 2 }
        XCTAssertGreaterThanOrEqual(ContinuousClock.now - firstImport, .seconds(2), "Wakes during import retain one spaced trailing import")
        try await waitUntil { MessageRecoveryURLProtocol.bodies(path: history, includingEmpty: true).count == 2 && !model.nativeHistoryRefreshing.contains(chat.id) }
        try await Task.sleep(for: .milliseconds(350))
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: importPath).count, 2)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: history, includingEmpty: true).count, 2)
    }
}

/// The rewind fixture stands in for the Mac the way the real one behaves: the
/// rewind is committed before its answer, a repeat finds the message gone, and
/// every request is counted so a scenario's "once" is checkable.
final class DiagnosticRewindStateTests: XCTestCase {
    private func post(_ state: DiagnosticRewindState, turn: String, restoreFiles: Bool = false) throws -> DiagnosticRewindState.Reply {
        let body = try JSONSerialization.data(withJSONObject: ["turnId": turn, "restoreFiles": restoreFiles])
        return try XCTUnwrap(state.respond(method: "POST", path: "/api/v1/conversations/chat/rewind", body: body, conversation: "chat"))
    }

    func testTheRewindCommitsOnceAndARepeatFindsTheMessageGone() throws {
        let state = DiagnosticRewindState()
        let snapshot: [String: Any] = ["historyRevision": 0, "messages": [
            ["codexTurnId": "turn-1", "createdAt": "1"], ["codexTurnId": "turn-2", "createdAt": "2"]], "assistantMessages": []]
        XCTAssertEqual((state.trim(snapshot)["messages"] as? [[String: Any]])?.count, 2, "nothing is cut before the request")
        let first = try post(state, turn: "turn-2")
        XCTAssertEqual(first.status, 200)
        let trimmed = state.trim(snapshot)
        XCTAssertEqual((trimmed["messages"] as? [[String: Any]])?.count, 1)
        XCTAssertEqual(trimmed["historyRevision"] as? Int, 1)
        let repeated = try post(state, turn: "turn-2")
        XCTAssertEqual(repeated.status, 409)
        XCTAssertEqual(String(decoding: repeated.body, as: UTF8.self), DiagnosticSubagentFixture.rewindGone)
        XCTAssertEqual(state.counts.rewinds, 2)
    }

    func testFileRestoreReadsAreCountedAndAFileRewindReportsItsFiles() throws {
        let state = DiagnosticRewindState()
        for _ in 0..<2 {
            let reply = try XCTUnwrap(state.respond(method: "GET", path: "/api/v1/settings/file-restore", body: nil, conversation: "chat"))
            XCTAssertEqual(reply.status, 200)
        }
        XCTAssertEqual(state.counts.backupReads, 2)
        let result = try post(state, turn: "turn-1", restoreFiles: true)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: result.body) as? [String: Any])
        XCTAssertEqual(object["filesRestored"] as? Bool, true)
        XCTAssertEqual(object["filesChanged"] as? Int, 2)
    }
}
#endif
