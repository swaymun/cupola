import XCTest
import Security
import CryptoKit
import WonderPairing
@testable import Wonder

/// The Mac pushes the Live Activity's content and iOS decodes it with these
/// types. A codec mismatch drops every push (the activity would freeze); a
/// wrong presentation would show finished or unknown work as running.
final class AgentLiveActivityTests: XCTestCase {
    typealias State = AgentActivityAttributes.ContentState

    /// Healthy read publications are frequent (300 ms during refresh, 2 s
    /// during streaming). Neither proves that signed registration is accepted.
    @MainActor func testRepeatedHealthyReadsCannotRearmRejectedRegistration() async throws {
        for status in [409, 401] {
            let h = try LifecycleHarness()
            defer { h.stop(); MessageRecoveryURLProtocol.releaseHeld() }
            MessageRecoveryURLProtocol.reset()
            h.active = []
            let model = h.library.model(for: h.saved)
            model.macConnected = true
            h.push.receivedToken(Data([1, 2]))
            h.coordinator.attach(h.library)
            h.coordinator.scan()
            let first = try XCTUnwrap(h.active.first?.id)
            for _ in 0..<3 {
                MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", status: status)
            }
            await h.coordinator.registerUpdateToken("update-\(first)", activityID: first)
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").count, 3)

            // Two more healthy refreshes after a fully rejected batch must
            // not re-arm it, even when ActivityKit repeats the same token.
            for _ in 0..<30 {
                MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", status: status)
            }
            for _ in 0..<2 {
                h.clock = h.clock.addingTimeInterval(0.3)
                model.macConnected = true
                await h.coordinator.registerUpdateToken("update-\(first)", activityID: first)
            }
            // Leave any further attempts in the same rejecting transport until
            // their deadline. Only real registration retry backoff elapses;
            // the foreground publication/scan cadence uses injected time.
            MessageRecoveryURLProtocol.hold(path: "/api/v1/push/live-activity")
            var submitted: Set<String> = [first]
            var deliveries: [Task<Void, Never>] = []
            for tick in 1...820 {
                h.clock = h.clock.addingTimeInterval(0.3)
                model.macConnected = true
                if tick % 10 == 0 { h.coordinator.scan() }
                if let id = h.active.first?.id, submitted.insert(id).inserted {
                    let count = MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").count
                    deliveries.append(Task { await h.coordinator.registerUpdateToken("update-\(id)", activityID: id) })
                    try await h.waitForTokenRequests(count + 1)
                }
                await Task.yield()
            }
            let sends = MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").count
            print("REJECTED_REFRESH status=\(status) elapsed=246.6 publications=822 scanCadence=3s requests=\(h.requests) ends=\(h.ends.count) tokenSubmissions=\(sends) active=\(h.active.map(\.id))")
            XCTAssertEqual(h.requests, 1, "Repeated healthy reads cannot start another registration window")
            XCTAssertEqual(h.ends, [first])
            XCTAssertTrue(h.active.isEmpty, "Unconfirmed work ends at the original deadline")
            XCTAssertEqual(sends, 3, "A failed batch must not restart on every healthy read")
            h.stop()
            MessageRecoveryURLProtocol.releaseHeld()
            for delivery in deliveries { await delivery.value }
        }
    }

    @MainActor func testCancelledRetryCannotReleaseSuccessorTaskOwnership() async throws {
        let h = try LifecycleHarness()
        defer { h.stop(); MessageRecoveryURLProtocol.releaseHeld() }
        MessageRecoveryURLProtocol.reset()
        h.holdApplications = true
        let activity = try XCTUnwrap(h.active.first)
        h.coordinator.attach(h.library)
        await h.coordinator.registerUpdateToken("old", activityID: activity.id)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true,"state":{"phase":"running","running":1,"needsInput":0}}"#.utf8))
        h.push.receivedToken(Data([1, 2]))
        h.coordinator.scan()
        try await h.waitForApplications(1)

        h.enqueueRemovalReplies()
        h.coordinator.setEnabled(false, host: "mac")
        try await h.waitForRemoval()
        h.coordinator.scan()
        // Reuse the injected client's ID to force overlapping generations:
        // its cancelled apply is still suspended when a successor owns the ID.
        h.storage.pairingLocked = true
        try h.replaceLibrary(h.saved, pairingLocked: true, preserveOtherMac: true)
        h.active = [activity]
        h.coordinator.setEnabled(true, host: "mac")
        await h.coordinator.registerUpdateToken("successor", activityID: activity.id)
        h.storage.pairingLocked = false
        h.library.load()
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true,"state":{"phase":"running","running":2,"needsInput":0}}"#.utf8))
        h.push.receivedToken(Data([1, 2]))
        h.coordinator.scan()
        try await h.waitForApplications(2)

        h.releaseApplication(1)
        try await h.waitForCompletedApplications(1)
        h.enqueueRemovalReplies()
        h.coordinator.setEnabled(false, host: "mac")
        try await h.waitForRemoval()
        h.releaseApplication(2)
        try await h.waitForCompletedApplications(2)
        let submissions = MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity")
        let updates = submissions.compactMap { body -> String? in
            guard let fields = try? JSONSerialization.jsonObject(with: body) as? [String: Any],
                  fields["kind"] as? String == "update",
                  let token = fields["token"] as? String, !token.isEmpty else { return nil }
            return token
        }
        print("RETRY_OWNERSHIP oldCancelled=\(h.completedApplications[1] == true) successorCancelled=\(h.completedApplications[2] == true) updateTokens=\(updates) totalSubmissions=\(submissions.count)")
        XCTAssertEqual(h.completedApplications[1], true)
        XCTAssertEqual(h.completedApplications[2], true, "The old completion must leave its successor tracked and cancellable")
        XCTAssertEqual(h.appliedStates.map(\.running), [1, 2])
        XCTAssertEqual(updates, ["old", "successor"], "Empty update tokens remove registrations during explicit off")
    }

    @MainActor func testLateRegistrationAfterDeadlineCannotApplyOrRestartActivity() async throws {
        let h = try LifecycleHarness()
        defer { h.stop(); MessageRecoveryURLProtocol.releaseHeld() }
        MessageRecoveryURLProtocol.reset()
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true,"state":{"phase":"running","running":1,"needsInput":0}}"#.utf8))
        MessageRecoveryURLProtocol.hold(path: "/api/v1/push/live-activity")
        h.push.receivedToken(Data([1, 2]))
        h.coordinator.attach(h.library)
        let registration = Task { await h.coordinator.registerUpdateToken("late", activityID: "owned") }
        try await h.waitForTokenRequests(1)
        h.clock = h.clock.addingTimeInterval(61)
        h.coordinator.scan()
        h.coordinator.scan() // The ended activity has disappeared from ActivityKit.
        MessageRecoveryURLProtocol.releaseHeld()
        await registration.value
        h.clock = h.clock.addingTimeInterval(600)
        h.coordinator.scan()
        XCTAssertEqual(h.applied, [])
        XCTAssertEqual(h.ends, ["owned"])
        XCTAssertEqual(h.requests, 0)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").count, 1)
    }

    @MainActor func testRotatedTokenGetsRegistrationWindowAndOldReplyCannotApply() async throws {
        let h = try LifecycleHarness()
        defer { h.stop(); MessageRecoveryURLProtocol.releaseHeld() }
        MessageRecoveryURLProtocol.reset()
        h.push.receivedToken(Data([1, 2]))
        h.coordinator.attach(h.library)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true}"#.utf8))
        await h.coordinator.registerUpdateToken("initial", activityID: "owned")
        h.clock = h.clock.addingTimeInterval(120)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true,"state":{"phase":"needsApproval","running":1,"needsInput":1}}"#.utf8))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true,"state":{"phase":"running","running":2,"needsInput":0}}"#.utf8))
        MessageRecoveryURLProtocol.hold(path: "/api/v1/push/live-activity")
        let registration = Task { await h.coordinator.registerUpdateToken("rotated", activityID: "owned") }
        try await h.waitForTokenRequests(2)
        h.clock = h.clock.addingTimeInterval(59)
        h.coordinator.scan()
        XCTAssertEqual(h.ends, [], "A new ActivityKit token gets its own registration window")
        await h.coordinator.registerUpdateToken("latest", activityID: "owned")
        MessageRecoveryURLProtocol.releaseHeld()
        await registration.value
        try await h.waitForTokenRequests(3)
        for _ in 0..<200 {
            if h.applied.count == 1 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(h.appliedStates.map(\.phase), [.running])
        XCTAssertEqual(h.appliedStates.map(\.running), [2])
        h.clock = h.clock.addingTimeInterval(1000)
        h.coordinator.scan()
        XCTAssertEqual(h.active.map(\.id), ["owned"])
        XCTAssertEqual(h.requests, 0)
        XCTAssertEqual(h.ends, [])
    }

    @MainActor func testUnknownAuthorityCannotStartOrSendActivityTokens() async throws {
        let h = try LifecycleHarness()
        defer { h.stop() }
        MessageRecoveryURLProtocol.reset()
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true}"#.utf8))
        h.coordinator.attach(h.library)
        do {
            _ = try await h.push.liveActivityToken(host: "mac", kind: "start", token: "synthetic-start")
            XCTFail("Awaiting APNs is not permission to send an ActivityKit token")
        } catch {}
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").count, 0)
        h.active = []
        h.coordinator.scan()
        XCTAssertEqual(h.requests, 0)
        h.push.receivedToken(Data([1, 2]))
        h.coordinator.scan()
        h.coordinator.scan()
        XCTAssertEqual(h.requests, 1, "Confirmed authority can start one activity for actual work")
    }

    /// Owner feedback 33: an ended activity still showing Done while the phone
    /// sees a thread running is taken down and replaced by a Running one.
    @MainActor func testEndedDoneOverRunningWorkIsReplacedByRunning() throws {
        let h = try LifecycleHarness()
        defer { h.stop() }
        h.push.receivedToken(Data([1, 2]))
        h.coordinator.attach(h.library)
        h.library.model(for: h.saved).macConnected = true
        h.active[0].isActive = false
        h.active[0].phase = .done
        h.coordinator.scan()
        XCTAssertEqual(h.ends, ["owned"])
        XCTAssertEqual(h.requests, 1, "Running work gets a Running activity in the same scan")
    }

    @MainActor func testEndedDoneWithNoRunningWorkIsLeftAlone() throws {
        let h = try LifecycleHarness()
        defer { h.stop() }
        h.push.receivedToken(Data([1, 2]))
        h.coordinator.attach(h.library)
        let model = h.library.model(for: h.saved)
        model.macConnected = true
        model.snapshots["chat"] = try Self.reply("turn", status: "completed")
        h.active[0].isActive = false
        h.active[0].phase = .done
        h.coordinator.scan()
        XCTAssertEqual(h.ends, [])
        XCTAssertEqual(h.requests, 0)
    }

    @MainActor func testConfirmedLossEndsOwnedActivityAndLateAPNsCannotRestartIt() throws {
        for receivedAPNs in [false, true] {
            for loss in ["live-off", "notifications-off", "sign-out", "unpaired", "replacement"] {
                let h = try LifecycleHarness()
                defer { h.stop() }
                if receivedAPNs { h.push.receivedToken(Data([1, 2])) }
                h.coordinator.attach(h.library)
                switch loss {
                case "live-off": h.coordinator.setEnabled(false, host: "mac")
                case "notifications-off": h.push.disable(h.library.model(for: h.saved))
                case "sign-out": h.signedIn = false; h.push.accountEnded()
                case "unpaired": try h.replaceLibrary(nil)
                default: try h.library.savePairing(LifecycleHarness.connection(device: "replacement")); h.push.refresh()
                }
                h.coordinator.scan()
                h.push.receivedToken(Data([1, 2]))
                h.coordinator.scan()
                XCTAssertEqual(h.ends, ["owned"], loss)
                XCTAssertEqual(h.requests, 0, loss)
                XCTAssertEqual(h.active.count, 0, loss)
            }
        }
    }

    @MainActor func testLockedSnapshotStillHonorsExplicitOffAndAccountEnd() throws {
        for signOut in [false, true] {
            let h = try LifecycleHarness(locked: true)
            defer { h.stop() }
            let original = h.values
            h.coordinator.attach(h.library)
            if signOut { h.push.accountEnded() }
            else { h.push.disable(h.library.model(for: h.saved)) }
            h.coordinator.scan()
            XCTAssertEqual(h.ends, ["owned"])
            XCTAssertEqual(h.values, original)
            XCTAssertEqual(h.writes, 0)
            h.locked = false
            h.push.receivedToken(Data([1, 2]))
            h.coordinator.scan()
            XCTAssertEqual(h.active.count, 0)
            XCTAssertEqual(h.requests, 0)
        }
    }

    @MainActor func testAppWideOffEndsEveryComputersActivityThroughLegacyAPI() throws {
        for notificationOff in [false, true] {
            let h = try LifecycleHarness()
            defer { h.stop() }
            h.active.append(.init(id: "other-owned", attributes: AgentActivityAttributes(hostID: "other", registrationId: LifecycleHarness.otherRegistration, computer: "Laptop", conversationID: nil)))
            h.push.receivedToken(Data([1, 2]))
            h.coordinator.attach(h.library)
            if notificationOff { h.push.disable(h.library.model(for: h.saved)) }
            else { h.coordinator.setEnabled(false, host: "mac") }
            h.coordinator.scan()
            h.push.receivedToken(Data([1, 2]))
            h.coordinator.scan()
            XCTAssertEqual(Set(h.ends), ["owned", "other-owned"])
            XCTAssertTrue(h.active.isEmpty)
            XCTAssertEqual(h.requests, 0)
        }
    }

    @MainActor func testAppWideLiveActivityMigrationAndLaterPairing() throws {
        for disabled in [[String](), ["mac"], ["mac", "other"]] {
            let h = try LifecycleHarness()
            defer { h.stop() }
            h.defaults.set(disabled, forKey: AgentLiveActivities.disabledKey)
            h.coordinator.attach(h.library)
            let expected = disabled.count < 2
            XCTAssertEqual(h.coordinator.enabled, expected)
            XCTAssertEqual(h.coordinator.isEnabled("mac"), expected)
            XCTAssertEqual(h.coordinator.isEnabled("other"), expected)
            XCTAssertNotNil(h.defaults.object(forKey: AgentLiveActivities.preferenceKey))
            XCTAssertNil(h.defaults.object(forKey: AgentLiveActivities.disabledKey))
            try h.library.savePairing(LifecycleHarness.connection(host: "later"))
            h.coordinator.scan()
            XCTAssertEqual(h.coordinator.isEnabled("later"), expected)
            h.coordinator.setAll(enabled: false, library: h.library)
            // Stale legacy input cannot turn a saved Off back on.
            h.defaults.set([], forKey: AgentLiveActivities.disabledKey)
            let restored = AgentLiveActivities(defaults: h.defaults, push: h.push,
                                                activityClient: .init(activities: { [] }, request: { _, _, _ in XCTFail("Off cannot start an activity"); return "unexpected" }, end: { _, _ in }, observe: false))
            restored.attach(h.library)
            XCTAssertFalse(restored.enabled)
            XCTAssertFalse(restored.isEnabled("later"))
        }
        let h = try LifecycleHarness()
        defer { h.stop() }
        let data = try JSONEncoder().encode(SavedConnections())
        let empty = ConnectionLibrary(storage: .init(read: { $0 == "connections-v1" ? data : nil }, save: { _, _ in }, forgetLegacy: {}))
        h.coordinator.attach(empty)
        XCTAssertFalse(h.coordinator.enabled, "No current On computer migrates to Off")
        h.coordinator.setAll(enabled: true, library: empty)
        try empty.savePairing(LifecycleHarness.connection(host: "later"))
        h.coordinator.scan()
        XCTAssertTrue(h.coordinator.isEnabled("later"), "An explicit On with no computers enrolls later pairings")
    }

    /// A cached push-to-start token must reach a later pairing even while
    /// that computer is idle, with duplicate scans producing no new enrollment.
    @MainActor func testStartTokenEnrollsLaterIdleComputerAndOffRemovesEveryToken() async throws {
        let h = try LifecycleHarness()
        defer { h.stop(); MessageRecoveryURLProtocol.reset() }
        MessageRecoveryURLProtocol.reset()
        h.active = []
        h.library.model(for: h.saved).snapshots = [:]
        h.push.receivedToken(Data([1, 2]))
        // Push already has both capabilities. Stage the Live Activity library
        // from one pairing to two to isolate delivery of its cached start token.
        let data = try JSONEncoder().encode(SavedConnections(legacy: h.saved))
        let library = ConnectionLibrary(storage: .init(read: { $0 == "connections-v1" ? data : nil }, save: { _, _ in }, forgetLegacy: {}))
        h.coordinator.attach(library)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true}"#.utf8))
        h.coordinator.receivedStartToken("cached-start")
        try await h.waitForTokenRequests(1)
        for _ in 0..<20 { await Task.yield() }
        h.coordinator.scan(); h.coordinator.scan()
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").count, 1)
        try library.savePairing(LifecycleHarness.connection(host: "other"))
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true}"#.utf8))
        h.coordinator.scan()
        try await h.waitForTokenRequests(2)
        // A successful deletion reports the resulting registration state.
        for _ in 0..<4 { MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":false}"#.utf8)) }
        h.coordinator.setAll(enabled: false)
        try await h.waitForRemoval()
        let fields = try MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").map {
            try XCTUnwrap(JSONSerialization.jsonObject(with: $0) as? [String: Any])
        }
        XCTAssertEqual(fields.filter { ($0["token"] as? String)?.isEmpty == false }.count, 2)
        XCTAssertEqual(fields.filter { ($0["token"] as? String) == "" }.count, 4, "Both computers forget start and update tokens")
        XCTAssertEqual(h.requests, 0, "Idle enrollment does not create an activity")
    }

    @MainActor func testAppWideNotificationOffRevokesEveryComputer() async throws {
        let h = try LifecycleHarness()
        defer { h.stop(); MessageRecoveryURLProtocol.reset() }
        MessageRecoveryURLProtocol.reset()
        // Each Mac's revoke travels on that computer's own client.
        try h.replaceLibrary(h.saved, preserveOtherMac: true, api: LifecycleHarness.syntheticAPI())
        let registrations = try JSONDecoder().decode([String: PushRegistration].self, from: XCTUnwrap(h.values["push-registrations-v1"]))
        for record in registrations.values {
            MessageRecoveryURLProtocol.enqueue(path: "/v1/registrations/\(try XCTUnwrap(record.id))/revoke", body: Data(#"{"revoked":true}"#.utf8))
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/revoke", body: Data(#"{"revoked":true}"#.utf8))
        }
        h.push.setAll(enabled: false, library: h.library)
        for _ in 0..<200 {
            if MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/revoke").count == 2 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertFalse(h.push.enabled)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/revoke").count, 2)
        for record in registrations.values {
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/v1/registrations/\(try XCTUnwrap(record.id))/revoke").count, 1)
        }
    }

    @MainActor func testKnownRevocationWinsWhilePairingIsStillLoading() throws {
        for signOut in [false, true] {
            let h = try LifecycleHarness(pairingLocked: true)
            defer { h.stop() }
            h.coordinator.attach(h.library)
            h.coordinator.scan()
            XCTAssertEqual(h.ends, [])
            if signOut { h.signedIn = false; h.push.accountEnded() }
            else { h.push.disable(h.library.model(for: h.saved)) }
            h.coordinator.scan()
            XCTAssertEqual(h.ends, ["owned"])
            h.push.receivedToken(Data([1, 2]))
            h.coordinator.scan()
            XCTAssertEqual(h.active.count, 0)
            XCTAssertEqual(h.requests, 0)
        }
    }

    @MainActor func testLaunchBuffersUpdateTokenUntilAuthorityIsConfirmed() async throws {
        let h = try LifecycleHarness()
        defer { h.stop(); MessageRecoveryURLProtocol.releaseHeld() }
        MessageRecoveryURLProtocol.reset()
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true,"state":{"phase":"running","running":1,"needsInput":0}}"#.utf8))
        MessageRecoveryURLProtocol.hold(path: "/api/v1/push/live-activity")
        h.coordinator.attach(h.library)
        await h.coordinator.registerUpdateToken("synthetic-update", activityID: "owned")
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").count, 0)
        h.clock = h.clock.addingTimeInterval(120)
        h.coordinator.scan()
        XCTAssertEqual(h.ends, [])
        h.push.receivedToken(Data([1, 2]))
        h.coordinator.scan()
        try await h.waitForTokenRequests(1)
        // A subsequent explicit delivery shares the in-flight registration.
        await h.coordinator.registerUpdateToken("synthetic-update", activityID: "owned")
        MessageRecoveryURLProtocol.releaseHeld()
        for _ in 0..<200 {
            if h.applied == ["owned"] { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(h.applied, ["owned"])
        h.clock = h.clock.addingTimeInterval(61)
        h.coordinator.scan()
        XCTAssertEqual(h.active.map(\.id), ["owned"])
        XCTAssertEqual(h.ends, [])
        XCTAssertEqual(h.requests, 0)
        XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").count, 1)
    }

    @MainActor func testLateRegistrationReplyCannotReviveRevokedOrReplacedActivity() async throws {
        for loss in ["live-off", "notifications-off", "sign-out", "unpaired", "replacement"] {
            let h = try LifecycleHarness()
            defer { h.stop(); MessageRecoveryURLProtocol.releaseHeld() }
            MessageRecoveryURLProtocol.reset()
            MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":true,"state":{"phase":"running","running":1,"needsInput":0}}"#.utf8))
            MessageRecoveryURLProtocol.hold(path: "/api/v1/push/live-activity")
            h.push.receivedToken(Data([1, 2]))
            h.coordinator.attach(h.library)
            let registration = Task { await h.coordinator.registerUpdateToken("synthetic-update", activityID: "owned") }
            try await h.waitForTokenRequests(1)
            switch loss {
            case "live-off": h.coordinator.setEnabled(false, host: "mac")
            case "notifications-off": h.push.disable(h.library.model(for: h.saved))
            case "sign-out": h.signedIn = false; h.push.accountEnded()
            case "unpaired": try h.replaceLibrary(nil)
            default: try h.library.savePairing(LifecycleHarness.connection(device: "replacement")); h.push.refresh()
            }
            h.coordinator.scan()
            MessageRecoveryURLProtocol.releaseHeld()
            await registration.value
            h.push.receivedToken(Data([1, 2]))
            h.clock = h.clock.addingTimeInterval(120)
            h.coordinator.scan()
            XCTAssertEqual(h.applied, [], loss)
            XCTAssertEqual(h.ends, ["owned"], loss)
            XCTAssertEqual(h.requests, 0, loss)
            XCTAssertEqual(h.active.count, 0, loss)
            XCTAssertEqual(MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").count, 1, loss)
        }
    }

    @MainActor func testKnownInvalidRegistrationEndsExistingActivity() throws {
        for invalid in ["disabled", "not-accepted", "missing-id", "different-owner"] {
            let h = try LifecycleHarness(changeRecord: { record in
                switch invalid {
                case "disabled": record.enabled = false
                case "not-accepted": record.macRegistered = false
                case "missing-id": record.id = nil
                default: record.id = "9f24d8ab-fde5-4a9e-9bcc-f7b398cd0d30"
                }
            })
            defer { h.stop() }
            h.push.receivedToken(Data([1, 2]))
            h.coordinator.attach(h.library)
            h.coordinator.scan()
            XCTAssertEqual(h.ends, ["owned"], invalid)
            XCTAssertEqual(h.requests, invalid == "different-owner" ? 1 : 0, invalid)
        }
    }

    /// A chat whose newest reply has this status.
    private static func reply(_ turn: String, status: String, chat: String = "chat", hydrated: Bool = true) throws -> ConversationSnapshot {
        try JSONDecoder().decode(ConversationSnapshot.self, from: Data(#"{"conversationId":"\#(chat)","hostEpoch":"epoch","lastSequence":1,"messages":[],"assistantMessages":[],"thread":{"hydrated":\#(hydrated),"turns":[{"id":"\#(turn)","status":"\#(status)","items":[]}]}}"#.utf8))
    }

    @MainActor private final class LifecycleHarness {
        static let otherRegistration = "9f24d8ab-fde5-4a9e-9bcc-f7b398cd0d29"
        final class Storage {
            var values: [String: Data] = [:]
            var locked = false
            var pairingLocked = false
            var signedIn = true
            var writes = 0
        }
        let storage: Storage
        let saved: SavedConnection
        var library: ConnectionLibrary
        let defaults: UserDefaults
        let suite = "live-activity-tests-" + UUID().uuidString
        var values: [String: Data] { storage.values }
        var locked: Bool { get { storage.locked } set { storage.locked = newValue } }
        var signedIn: Bool { get { storage.signedIn } set { storage.signedIn = newValue } }
        var clock = Date()
        var writes: Int { storage.writes }
        var requests = 0
        var onActivities: (() -> Void)?
        var onRequest: (() -> Void)?
        var appliedAt: TimeInterval?
        var ends: [String] = []
        var applied: [String] = []
        var appliedStates: [State] = []
        var holdApplications = false
        var applicationGates: [Int: CheckedContinuation<Void, Never>] = [:]
        var completedApplications: [Int: Bool] = [:]
        var active: [AgentLiveActivities.OwnedActivity]
        lazy var push: PushNotifications = {
            let key = try! P256.Signing.PrivateKey(rawRepresentation: Data(repeating: 1, count: 32))
            @Sendable func wrap(_ key: P256.Signing.PrivateKey) -> EnrollmentSigningIdentity {
                EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation, sign: { try key.signature(for: $0) })
            }
            let signing = SigningIdentity(read: { key.rawRepresentation }, save: { _ in },
                                          restore: { try wrap(P256.Signing.PrivateKey(rawRepresentation: $0)) }, create: { wrap(key) })
            return PushNotifications(api: Self.syntheticAPI(), signingIdentity: signing, read: { [storage] key in
            if storage.locked { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
            return storage.values[key]
        }, write: { [storage] data, key in storage.values[key] = data; storage.writes += 1 },
            authorize: { throw CancellationError() }, registerForPush: {},
            accountService: { [storage] in storage.signedIn ? "https://push.example.test" : nil })
        }()
        lazy var coordinator = AgentLiveActivities(defaults: defaults, push: push, activityClient: .init(
            activities: { [unowned self] in onActivities?(); return active },
            request: { [unowned self] attributes, _, _ in
                requests += 1
                onRequest?()
                let id = "requested-\(requests)"
                active.append(.init(id: id, attributes: attributes))
                return id
            }, end: { [unowned self] id, _ in ends.append(id); active.removeAll { $0.id == id } },
            apply: { [weak self] id, state in await self?.apply(id, state: state) },
            allows: { true }, observe: false), now: { [unowned self] in clock })

        init(locked: Bool = false, pairingLocked: Bool = false, pushStarted: Bool = false, device: String = "device",
             changeRecord: (inout PushRegistration) -> Void = { _ in }) throws {
            let storage = Storage()
            self.storage = storage
            storage.locked = locked; storage.pairingLocked = pairingLocked
            saved = try Self.connection(device: device)
            var connections = SavedConnections(legacy: saved)
            connections.save(try Self.connection(host: "other"))
            let data = try JSONEncoder().encode(connections)
            library = ConnectionLibrary(storage: .init(read: {
                if storage.pairingLocked { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
                return $0 == "connections-v1" ? data : nil
            }, save: { _, _ in }, forgetLegacy: {}))
            defaults = UserDefaults(suiteName: suite)!
            var record = PushRegistration(host: "mac", device: device, endpoint: "https://push.example.test", key: Data(repeating: 1, count: 32))
            record.id = "9f24d8ab-fde5-4a9e-9bcc-f7b398cd0d28"; record.token = "0102"
            record.macRegistered = true; record.previewVersion = 1; record.registeredAt = Date()
            let activityRegistration = record.id
            changeRecord(&record)
            var other = PushRegistration(host: "other", device: "device", endpoint: record.endpoint, key: record.key)
            other.id = Self.otherRegistration; other.token = "0102"
            other.macRegistered = true; other.previewVersion = 1; other.registeredAt = Date()
            storage.values = ["push-registrations-v1": try JSONEncoder().encode(["mac": record, "other": other]),
                      "push-requested-devices-v1": try JSONEncoder().encode(["mac": device, "other": "device"])]
            active = [.init(id: "owned", attributes: AgentActivityAttributes(hostID: pushStarted ? nil : "mac", registrationId: activityRegistration, computer: "Studio", conversationID: "chat"))]
            push.attach(library)
            push.setForeground(false)
            let model = library.model(for: saved)
            model.snapshots["chat"] = try JSONDecoder().decode(ConversationSnapshot.self, from: Data(#"{"conversationId":"chat","hostEpoch":"epoch","lastSequence":1,"messages":[],"assistantMessages":[],"thread":{"hydrated":true,"turns":[{"id":"turn","status":"inProgress","items":[]}]}}"#.utf8))
            XCTAssertTrue(AgentLiveActivities.hasWork(model))
        }
        func stop() {
            for running in Array(applicationGates.keys) { releaseApplication(running) }
            signedIn = false
            coordinator.setEnabled(false, host: "mac")
            push.setForeground(false)
            defaults.removePersistentDomain(forName: suite)
        }
        func waitForTokenRequests(_ count: Int) async throws {
            for _ in 0..<200 {
                if MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").count >= count { return }
                try await Task.sleep(for: .milliseconds(10))
            }
            XCTFail("The actual push coordinator never reached the synthetic transport")
            throw CancellationError()
        }
        /// App-wide Off forgets the start and update tokens on every paired computer.
        func enqueueRemovalReplies() {
            for _ in 0..<(2 * library.paired.count) {
                MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":false}"#.utf8))
            }
        }
        func waitForRemoval() async throws {
            for _ in 0..<1000 {
                if (defaults.stringArray(forKey: AgentLiveActivities.pendingRemovalKey) ?? []).isEmpty { return }
                try await Task.sleep(for: .milliseconds(10))
            }
            XCTFail("Every computer must acknowledge both explicit-revocation removals")
            throw CancellationError()
        }
        func apply(_ id: String, state: State) async {
            appliedAt = ProcessInfo.processInfo.systemUptime
            applied.append(id); appliedStates.append(state)
            if holdApplications {
                await withCheckedContinuation { applicationGates[state.running] = $0 }
                completedApplications[state.running] = Task.isCancelled
            }
        }
        /// The owner swipes the activity off the Lock Screen while it shows work.
        func dismiss(_ id: String) {
            for index in active.indices where active[index].id == id {
                active[index].isActive = false
                active[index].dismissedWhileWorking = true
            }
        }
        func releaseApplication(_ running: Int) { applicationGates.removeValue(forKey: running)?.resume() }
        func waitForApplications(_ count: Int) async throws {
            for _ in 0..<200 {
                if applied.count >= count { return }
                try await Task.sleep(for: .milliseconds(10))
            }
            XCTFail("The successor registration never applied its accepted state")
            throw CancellationError()
        }
        func waitForCompletedApplications(_ count: Int) async throws {
            for _ in 0..<200 {
                if completedApplications.count >= count { return }
                try await Task.sleep(for: .milliseconds(10))
            }
            XCTFail("The released registration did not complete")
            throw CancellationError()
        }
        func replaceLibrary(_ connection: SavedConnection?, pairingLocked: Bool = false, preserveOtherMac: Bool = false, api: PairingAPI? = nil) throws {
            var saved = SavedConnections(legacy: connection)
            if preserveOtherMac { saved.save(try Self.connection(host: "other")) }
            let data = try JSONEncoder().encode(saved)
            library = ConnectionLibrary(storage: .init(read: { [storage] in
                if pairingLocked && storage.pairingLocked { throw SigningIdentityFailure.keychain(errSecInteractionNotAllowed) }
                return $0 == "connections-v1" ? data : nil
            }, save: { _, _ in }, forgetLegacy: {}), api: api)
            push.attach(library)
            push.setForeground(false)
            coordinator.attach(library)
        }
        /// The push service and the Macs, without a real host.
        static func syntheticAPI() -> PairingAPI {
            let configuration = URLSessionConfiguration.ephemeral
            configuration.protocolClasses = [MessageRecoveryURLProtocol.self]
            return PairingAPI(configuration: configuration)
        }
        static func connection(device: String = "device", host: String = "mac") throws -> SavedConnection {
            try JSONDecoder().decode(SavedConnection.self, from: JSONSerialization.data(withJSONObject: [
                "origin": "https://camera-unit.invalid", "hostName": "Studio", "credential": [
                    "sessionToken": "synthetic", "csrfToken": "synthetic", "deviceId": device, "hostInstallationId": host
                ]]))
        }
    }

    /// The content exactly as the push Worker sends it (the shared protocol fixture).
    func testContentDecodesWhatTheServicePushes() throws {
        let fixture = try JSONSerialization.jsonObject(with: Data(contentsOf:
            DiagnosticTestResources.url("packages/protocol/fixtures/push-v1.json"))) as? [String: Any]
        let reply = try XCTUnwrap((fixture?["pushLiveActivityReply"] as? [String: Any])?["state"])
        let state = try JSONDecoder().decode(State.self, from: JSONSerialization.data(withJSONObject: reply))
        let row = AgentActivityAttributes.ThreadRow(title: "Fix login crash", project: "Cupola", status: .needsApproval, startedAt: 1789839900,
                                                    routeId: "5b0c9a3e-2f7d-4e61-9a8b-0c1d2e3f4a5b",
                                                    registrationId: "11111111-1111-4111-8111-111111111111")
        XCTAssertEqual(state, State(phase: .needsApproval, running: 2, needsInput: 1, threads: [row]))
        let done = try JSONDecoder().decode(State.self, from: Data(#"{"phase":"done","running":0,"needsInput":0}"#.utf8))
        XCTAssertEqual(done, State(phase: .done, running: 0))
        // Encoding round-trips with only the pushed keys: no conversation IDs or text beyond the rows.
        let keys = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(state)) as? [String: Any]).keys
        XCTAssertEqual(Set(keys), ["phase", "running", "needsInput", "threads"])
    }

    /// Fixed copy: a terminal-only card says how the work ended, never "Stopped" or a count.
    func testTerminalCardCopyIsFixed() {
        let attributes = AgentActivityAttributes(hostID: "mac", registrationId: nil, computer: "Studio", conversationID: nil)
        for (phase, copy) in [(AgentActivityAttributes.Phase.done, "Agent work completed"), (.failed, "Agent work failed")] {
            let shown = AgentActivityPresentation(attributes: attributes, state: State(phase: phase, running: 0, needsInput: 0), isStale: false)
            XCTAssertEqual(shown.detail, copy)
        }
    }

    /// A state of Done never shows while the phone reads a named reply still running.
    @MainActor func testTruthfulReplacesDoneOnlyWhileWorkRuns() throws {
        let h = try LifecycleHarness()
        defer { h.stop() }
        h.coordinator.attach(h.library)
        let model = h.library.model(for: h.saved)
        let host = h.saved.credential.hostInstallationId
        let done = State(phase: .done, running: 0, needsInput: 0)
        model.snapshots["chat"] = try Self.reply("turn", status: "inProgress")
        model.macConnected = false
        XCTAssertEqual(h.coordinator.truthful(done, host: host).phase, .done, "An unconfirmed Mac proves nothing")
        model.macConnected = true
        let shown = h.coordinator.truthful(done, host: host)
        XCTAssertEqual(shown.phase, .running)
        XCTAssertEqual(shown.running, 1)
        let running = State(phase: .running, running: 2, needsInput: 0)
        XCTAssertEqual(h.coordinator.truthful(running, host: host), running, "Only a finished state is corrected")
        model.snapshots["chat"] = try Self.reply("turn", status: "completed")
        XCTAssertEqual(h.coordinator.truthful(done, host: host).phase, .done, "A genuine Done stands")
    }

    /// The owner swipes the card away: the service is told, no card starts, and
    /// opening Cupola again lifts it and tells the service.
    @MainActor func testSwipedCardIsReportedAndStaysGoneUntilCupolaOpensAgain() async throws {
        let h = try LifecycleHarness()
        defer { h.stop(); MessageRecoveryURLProtocol.reset() }
        MessageRecoveryURLProtocol.reset()
        h.push.receivedToken(Data([1, 2]))
        h.coordinator.attach(h.library)
        h.library.model(for: h.saved).macConnected = true
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":false}"#.utf8))
        h.dismiss("owned")
        h.coordinator.scan()
        XCTAssertTrue(h.coordinator.isDismissed)
        try await h.waitForTokenRequests(1)
        func kinds() throws -> [String] {
            try MessageRecoveryURLProtocol.bodies(path: "/api/v1/push/live-activity").map {
                try XCTUnwrap((JSONSerialization.jsonObject(with: $0) as? [String: Any])?["kind"] as? String)
            }
        }
        XCTAssertEqual(try kinds(), ["dismissed"])
        // Work keeps running and finishing; no new card starts on its own.
        for _ in 0..<20 { await Task.yield() }
        h.coordinator.scan(); h.coordinator.scan()
        XCTAssertEqual(h.requests, 0)
        XCTAssertTrue(h.coordinator.isDismissed)
        // Opening Cupola lifts it: the service is told, then a card starts for the running work.
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":false}"#.utf8))
        h.coordinator.setForeground(true)
        XCTAssertFalse(h.coordinator.isDismissed)
        try await h.waitForTokenRequests(2)
        XCTAssertEqual(try kinds(), ["dismissed", "foreground"])
        for _ in 0..<200 where h.requests == 0 { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertEqual(h.requests, 1)
        // iOS reports a swipe when the app resumes; seen while Cupola is in front the
        // owner is already back, so it is not a dismissal.
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":false}"#.utf8))
        h.dismiss("requested-1")
        h.coordinator.scan()
        XCTAssertFalse(h.coordinator.isDismissed)
        h.coordinator.setForeground(false)
    }

    /// The service already holds a dismissal (the card was gone while Wonder was
    /// suspended): the activity the phone just started is ended, and no card follows
    /// until the owner opens Cupola.
    @MainActor func testServiceDismissalAnswerEndsTheNewActivity() async throws {
        let h = try LifecycleHarness()
        defer { h.stop(); MessageRecoveryURLProtocol.reset() }
        MessageRecoveryURLProtocol.reset()
        h.push.receivedToken(Data([1, 2]))
        h.coordinator.attach(h.library)
        MessageRecoveryURLProtocol.enqueue(path: "/api/v1/push/live-activity", body: Data(#"{"registered":false,"dismissed":true}"#.utf8))
        await h.coordinator.registerUpdateToken("aa", activityID: "owned")
        XCTAssertEqual(h.ends, ["owned"])
        XCTAssertTrue(h.coordinator.isDismissed)
        h.coordinator.scan()
        XCTAssertEqual(h.requests, 0)
    }

    /// A new card replaces one that lingers as Done: only one is ever on the Lock Screen.
    @MainActor func testNewActivityEndsALingeringFinishedOne() throws {
        let h = try LifecycleHarness()
        defer { h.stop() }
        h.active.append(.init(id: "lingering", attributes: h.active[0].attributes, isActive: false, phase: .done))
        h.coordinator.attach(h.library)
        XCTAssertEqual(h.ends, ["lingering"])
    }

    /// A row's tap names its own computer's registration and route; the card's
    /// tap follows its most urgent row.
    func testTapFollowsTheRowsRegistrationAndRoute() throws {
        let attributes = AgentActivityAttributes(hostID: "mac_1", registrationId: "9f24d8ab-fde5-4a9e-9bcc-f7b398cd0d28", computer: "Studio", conversationID: "chat-1")
        let route = "5b0c9a3e-2f7d-4e61-9a8b-0c1d2e3f4a5b", other = "9f24d8ab-fde5-4a9e-9bcc-f7b398cd0d29"
        let rows = [
            AgentActivityAttributes.ThreadRow(title: "A", project: "P", status: .needsApproval, startedAt: 1, routeId: route, registrationId: other),
            AgentActivityAttributes.ThreadRow(title: "B", project: "P", status: .running, startedAt: 2, routeId: route, registrationId: nil),
        ]
        let url = try XCTUnwrap(AgentActivityLink.url(attributes: attributes, row: rows[0], bundleIdentifier: "com.swaymun.wonder"))
        XCTAssertEqual(url.absoluteString, "wonder://v1/registrations/\(other)/routes/\(route)")
        XCTAssertEqual(WonderDeepLink.parse(url, scheme: "wonder"), .liveActivityRoute(host: nil, registration: other, route: route))
        XCTAssertNil(AgentActivityLink.url(attributes: attributes, row: rows[1], bundleIdentifier: "com.swaymun.wonder"), "A row names its computer")
        let state = State(phase: .needsApproval, running: 2, needsInput: 1, threads: rows)
        XCTAssertEqual(AgentActivityLink.url(attributes: attributes, state: state, bundleIdentifier: "com.swaymun.wonder"), url)
        // Finished with no rows: the chat it started from. Several without rows: just the app.
        let done = State(phase: .done, running: 0)
        XCTAssertEqual(AgentActivityLink.url(attributes: attributes, state: done, bundleIdentifier: "com.swaymun.wonder")?.absoluteString,
                       "wonder://v1/hosts/mac_1/chats/chat-1")
        XCTAssertNil(AgentActivityLink.url(attributes: attributes, state: State(phase: .running, running: 2),
                                           bundleIdentifier: "com.swaymun.wonder"))
        XCTAssertNil(WonderDeepLink.parse(URL(string: "wonder://v1/hosts/mac_1/routes/not-a-uuid")!, scheme: "wonder"))
    }

}
