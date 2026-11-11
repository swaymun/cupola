import XCTest
@testable import WonderPairing

final class SavedConnectionsTests: XCTestCase {
    private func connection(_ host: String, token: String = "session", device: String = "phone", origin: String? = nil) throws -> SavedConnection {
        let data = try JSONSerialization.data(withJSONObject: ["origin": origin ?? "https://\(host).invalid", "hostName": host,
            "credential": ["hostInstallationId": host, "deviceId": device, "sessionToken": token, "csrfToken": "csrf"]])
        return try JSONDecoder().decode(SavedConnection.self, from: data)
    }

    func testLegacyMigrationPreservesPairingAndDefaultsToAll() throws {
        let legacy = try connection("studio")
        let saved = SavedConnections(legacy: legacy)
        let restored = try JSONDecoder().decode(SavedConnections.self, from: JSONEncoder().encode(saved))
        XCTAssertEqual(restored.connections.count, 1)
        XCTAssertEqual(restored.connections.first?.credential.sessionToken, legacy.credential.sessionToken)
        XCTAssertTrue(restored.selectedHostIDs.isEmpty)
        var filtered = restored
        try filtered.select("studio")
        XCTAssertEqual(filtered.selectedHostIDs, ["studio"])
    }

    func testLegacyKeychainValueMigratesToOneTailscaleRoute() throws {
        let legacy = Data(#"{"connections":[{"origin":"https://studio.tail.ts.net","credential":{"hostInstallationId":"studio","deviceId":"phone","sessionToken":"session","csrfToken":"csrf"}}],"selectedHostIDs":["studio"]}"#.utf8)
        let saved = try JSONDecoder().decode(SavedConnections.self, from: legacy)
        let encoded = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(saved)) as? [String: Any])
        let connections = try XCTUnwrap(encoded["connections"] as? [[String: Any]])
        let routes = try XCTUnwrap(connections[0]["routes"] as? [[String: Any]])
        XCTAssertEqual(routes.count, 1)
        XCTAssertEqual(routes[0]["kind"] as? String, "tailscale")
        XCTAssertEqual(routes[0]["origin"] as? String, "https://studio.tail.ts.net")
        XCTAssertEqual(routes[0]["learned"] as? Bool, false)
        XCTAssertNil(saved.connections[0].hostKeyPin)
        XCTAssertNil(saved.connections[0].routes[0].lastOK)
        XCTAssertNil(saved.connections[0].routes[0].lastRTTms)
        XCTAssertEqual(saved.connections[0].credential.sessionToken, "session")
        XCTAssertEqual(saved.connections[0].storageDeviceId, "phone")
        XCTAssertEqual(saved.selectedHostIDs, ["studio"])
    }

    func testPairingAnotherAddressKeepsBothRoutesForOneHost() throws {
        var saved = SavedConnections(legacy: try connection("studio", origin: "https://studio.tail.ts.net:8443"))
        saved.save(try connection("studio", token: "renewed", device: "new-phone", origin: "https://studio.tail.ts.net:8444"))
        let encoded = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(saved)) as? [String: Any])
        let connections = try XCTUnwrap(encoded["connections"] as? [[String: Any]])
        let routes = try XCTUnwrap(connections[0]["routes"] as? [[String: Any]])
        XCTAssertEqual(connections.count, 1)
        XCTAssertEqual(Set(routes.compactMap { $0["origin"] as? String }), ["https://studio.tail.ts.net:8443", "https://studio.tail.ts.net:8444"])
        XCTAssertEqual(saved.connections[0].credential.sessionToken, "renewed")
        XCTAssertEqual(saved.connections[0].storageDeviceId, "phone")
    }

    private let home = ConnectionRoute(kind: .home, origin: "https://192.168.1.20:3778")
    private let tailscale = ConnectionRoute(kind: .tailscale, origin: "https://studio.tail.ts.net")
    private let relay = ConnectionRoute(kind: .relay, origin: "https://opaque.relay.invalid")

    private func routed(_ routes: [ConnectionRoute], origin: String? = nil) throws -> SavedConnection {
        try SavedConnection(routes: routes, origin: origin, credential: connection("studio").credential,
                            hostName: "Studio", requiresPairing: true, storageDeviceId: "storage-phone", hostKeyPin: "qr-pin")
    }

    private func reload(_ value: SavedConnection) throws -> SavedConnection {
        try JSONDecoder().decode(SavedConnection.self, from: JSONEncoder().encode(value))
    }

    // Contract: the Keychain snapshot retains every route, its provenance and
    // measurements, the selected address, and the one host credential/pin.
    func testRouteSnapshotRoundTripPreservesOrderSelectionPinAndMeasurements() throws {
        let measuredHome = ConnectionRoute(kind: .home, origin: home.origin, lastOK: Date(timeIntervalSince1970: 1234), lastRTTms: 4)
        let restored = try reload(routed([relay, measuredHome, tailscale], origin: tailscale.origin))
        XCTAssertEqual(restored.routes, [relay, measuredHome, tailscale])
        XCTAssertEqual(restored.origin, tailscale.origin)
        XCTAssertEqual(restored.hostKeyPin, "qr-pin")
        XCTAssertEqual(restored.credential.deviceId, "phone")
        XCTAssertEqual(restored.credential.csrfToken, "csrf")
        XCTAssertEqual(restored.credential.sessionToken, "session")
        XCTAssertEqual(restored.credential.hostInstallationId, "studio")
        XCTAssertEqual(restored.storageDeviceId, "storage-phone")
        XCTAssertTrue(restored.requiresPairing)
        XCTAssertEqual(restored.hostName, "Studio")
        let switched = try restored.usingRoute(origin: home.origin)
        XCTAssertEqual(switched.origin, home.origin)
        XCTAssertEqual(switched.routes, restored.routes)
        XCTAssertThrowsError(try restored.usingRoute(origin: "https://unknown.invalid"))
    }

    func testLearnedMergeFollowsChangedAddressAndNeverRemovesExplicitRoutes() throws {
        let measuredHome = ConnectionRoute(kind: .home, origin: home.origin, lastOK: Date(timeIntervalSince1970: 1234), lastRTTms: 4)
        var value = try routed([relay, measuredHome])
        value.mergeLearnedRoutes([home, tailscale, tailscale, relay], from: relay.origin)
        value = try reload(value)
        XCTAssertEqual(value.routes.map(\.origin), [relay.origin, home.origin, tailscale.origin])
        XCTAssertEqual(value.routes[1], measuredHome)
        XCTAssertTrue(value.routes[2].learned)
        XCTAssertEqual(value.routes[2].learnedFromOrigins, [relay.origin])
        value = try value.usingRoute(origin: tailscale.origin)
        let movedHome = ConnectionRoute(kind: .home, origin: "https://192.168.2.20:3778")
        value.mergeLearnedRoutes([movedHome], from: relay.origin)
        value = try reload(value)
        XCTAssertEqual(value.routes.map(\.origin), [relay.origin, home.origin, movedHome.origin])
        XCTAssertEqual(value.routes[1], measuredHome)
        XCTAssertEqual(value.routes[2].learnedFromOrigins, [relay.origin])
        XCTAssertEqual(value.origin, relay.origin, "A withdrawn selected address cannot remain in use")
        value.mergeLearnedRoutes([], from: relay.origin)
        XCTAssertEqual(value.routes, [relay, measuredHome])
        XCTAssertEqual(value.hostKeyPin, "qr-pin")
        XCTAssertEqual(value.credential.sessionToken, "session")
    }

    // Contract with the host: the host status Rust serves (its saved fixture,
    // checked against the live response in wonderd) decodes with the phone's
    // route type, passes the phone's origin check, and adds the home network
    // ahead of the Tailscale address the phone paired with.
    func testHostStatusFixtureRoutesDecodeAndAreLearned() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        struct Host: Decodable { let hostInstallationId: String; let hostKeyPin: String?; let routes: [ConnectionRoute]? }
        let host = try JSONDecoder().decode(Host.self, from: Data(contentsOf: root.appendingPathComponent("tests/contracts/native/host-status.json")))
        let routes = try XCTUnwrap(host.routes)
        XCTAssertEqual(routes.map(\.kind), [.home, .tailscale, .relay])
        XCTAssertFalse(routes.contains(where: \.learned))
        XCTAssertEqual(routes.filter { (try? PairingLink.origin($0.origin)) == $0.origin }.count, routes.count)
        XCTAssertEqual(host.hostKeyPin?.utf8.count, 43)
        let paired = ConnectionRoute(kind: .tailscale, origin: routes[1].origin)
        var value = try routed([paired])
        value.mergeLearnedRoutes(routes, from: paired.origin)
        XCTAssertEqual(Set(value.routes.map(\.origin)), Set(routes.map(\.origin)))
        XCTAssertEqual(value.routes.filter(\.learned).map(\.kind), [.home, .relay])
    }

    func testSharedLearnedRouteSurvivesUntilItsLastSourceWithdrawsIt() throws {
        var value = try routed([relay, tailscale])
        value.mergeLearnedRoutes([home], from: relay.origin)
        value.mergeLearnedRoutes([home], from: tailscale.origin)
        value = try reload(value)
        XCTAssertEqual(value.routes[2].learnedFromOrigins, [relay.origin, tailscale.origin])
        let withoutRelay = try XCTUnwrap(value.removingRoute(origin: relay.origin))
        XCTAssertEqual(withoutRelay.routes.map(\.origin), [tailscale.origin, home.origin])
        XCTAssertEqual(withoutRelay.routes[1].learnedFromOrigins, [tailscale.origin])
        value.mergeLearnedRoutes([], from: relay.origin)
        XCTAssertEqual(value.routes[2].learnedFromOrigins, [tailscale.origin])
        value = try XCTUnwrap(value.removingRoute(origin: relay.origin))
        XCTAssertEqual(value.origin, tailscale.origin)
        XCTAssertEqual(value.routes.map(\.origin), [tailscale.origin, home.origin])
        value.mergeLearnedRoutes([], from: tailscale.origin)
        XCTAssertEqual(value.routes, [tailscale])
    }

    func testSourceRemovalCascadesThroughLearnedCyclesAndPreservesAnotherHost() throws {
        var value = try routed([relay, tailscale])
        value.mergeLearnedRoutes([home], from: relay.origin)
        let otherHome = ConnectionRoute(kind: .home, origin: "https://192.168.1.21:3778")
        value.mergeLearnedRoutes([otherHome], from: home.origin)
        value.mergeLearnedRoutes([home], from: otherHome.origin)
        value = try reload(value)
        var saved = SavedConnections(legacy: value)
        saved.save(try connection("laptop"))
        try saved.select("studio")
        saved.removeRoute(origin: home.origin, from: "studio")
        XCTAssertEqual(saved.connections[0].routes.count, 4, "Learned routes are managed through their source")
        saved.removeRoute(origin: relay.origin, from: "studio")
        XCTAssertEqual(saved.connections[0].routes, [tailscale])
        XCTAssertEqual(saved.connections[0].origin, tailscale.origin)
        XCTAssertEqual(saved.selectedHostIDs, ["studio"])
        saved.removeRoute(origin: tailscale.origin, from: "studio")
        XCTAssertEqual(saved.connections.count, 1)
        XCTAssertEqual(saved.connections[0].credential.hostInstallationId, "laptop")
        XCTAssertTrue(saved.selectedHostIDs.isEmpty)
    }

    func testWithdrawnSourceDoesNotLeaveAnOrphanLearnedCycle() throws {
        var value = try routed([relay])
        value.mergeLearnedRoutes([home], from: relay.origin)
        value.mergeLearnedRoutes([tailscale], from: home.origin)
        value.mergeLearnedRoutes([home], from: tailscale.origin)
        value.mergeLearnedRoutes([], from: relay.origin)
        XCTAssertEqual(value.routes, [relay])
    }

    func testLearnedRouteCanBeMadeExplicitAndSurvivesSourceRemoval() throws {
        var value = try routed([relay])
        value.mergeLearnedRoutes([home], from: relay.origin)
        value.addRoute(ConnectionRoute(kind: .home, origin: home.origin, lastOK: Date(timeIntervalSince1970: 100), lastRTTms: 5))
        value.addRoute(home)
        value = try reload(value)
        XCTAssertFalse(value.routes[1].learned)
        XCTAssertEqual(value.routes[1].lastRTTms, 5)
        XCTAssertEqual(value.routes[1].lastOK, Date(timeIntervalSince1970: 100))
        value = try XCTUnwrap(value.removingRoute(origin: relay.origin))
        XCTAssertEqual(value.routes.map(\.origin), [home.origin])
        XCTAssertEqual(value.origin, home.origin)
    }

    func testOriginOnlyCredentialRefreshPreservesRoutesPinAndStorageIdentity() throws {
        var value = try routed([home, relay])
        value.mergeLearnedRoutes([tailscale], from: relay.origin)
        var saved = SavedConnections(legacy: try reload(value))
        saved.save(try connection("studio", token: "renewed", device: "new-phone", origin: home.origin))
        let restored = try reload(saved.connections[0])
        XCTAssertEqual(restored.routes, value.routes)
        XCTAssertEqual(restored.hostKeyPin, "qr-pin")
        XCTAssertEqual(restored.storageDeviceId, "storage-phone")
        XCTAssertEqual(restored.credential.sessionToken, "renewed")
        XCTAssertEqual(restored.credential.deviceId, "new-phone")
        XCTAssertEqual(restored.routes[0].kind, .home)
        XCTAssertFalse(restored.requiresPairing)
    }

    func testSavingChangedSnapshotDoesNotResurrectWithdrawnOrRemovedRoutes() throws {
        var value = try routed([relay, tailscale])
        value.mergeLearnedRoutes([home], from: relay.origin)
        var saved = SavedConnections(legacy: value)
        value.mergeLearnedRoutes([], from: relay.origin)
        saved.save(value)
        XCTAssertEqual(saved.connections[0].routes, [relay, tailscale])
        value = try XCTUnwrap(value.removingRoute(origin: relay.origin))
        saved.save(value)
        XCTAssertEqual(saved.connections[0].routes, [tailscale])
    }

    func testUnknownSourceCannotIntroduceOrRemoveRoutesAndHostStorageNeverLeaks() throws {
        var value = try routed([relay])
        value.mergeLearnedRoutes([home], from: "https://unknown.invalid")
        XCTAssertEqual(value.routes, [relay])
        XCTAssertEqual(value.removingRoute(origin: "https://unknown.invalid")?.routes, [relay])
        let other = try connection("laptop").preservingStorage(from: value)
        XCTAssertNil(other.hostKeyPin)
        XCTAssertEqual(other.storageDeviceId, "phone")
        XCTAssertEqual(other.routes.map(\.origin), ["https://laptop.invalid"])
    }

    func testInvalidRouteSnapshotsFailToDecodeInsteadOfUsingAnUnsavedAddress() throws {
        XCTAssertThrowsError(try routed([]))
        XCTAssertThrowsError(try routed([home, home]))
        XCTAssertThrowsError(try routed([home], origin: relay.origin))
        let value = try routed([home])
        let original = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(value)) as? [String: Any])
        for routes in [[], [home, home]] {
            var invalid = original
            invalid["routes"] = try JSONSerialization.jsonObject(with: JSONEncoder().encode(routes))
            XCTAssertThrowsError(try JSONDecoder().decode(SavedConnection.self, from: JSONSerialization.data(withJSONObject: invalid)))
        }
        var invalid = original
        invalid["origin"] = relay.origin
        XCTAssertThrowsError(try JSONDecoder().decode(SavedConnection.self, from: JSONSerialization.data(withJSONObject: invalid)))
        invalid = original
        invalid["routes"] = [["kind": "home", "origin": home.origin, "learned": true]]
        XCTAssertThrowsError(try JSONDecoder().decode(SavedConnection.self, from: JSONSerialization.data(withJSONObject: invalid)))
        var withoutOrigin = original
        withoutOrigin.removeValue(forKey: "origin")
        let restored = try JSONDecoder().decode(SavedConnection.self, from: JSONSerialization.data(withJSONObject: withoutOrigin))
        XCTAssertEqual(restored.origin, home.origin)
    }

    func testSingleComputerSelectionReplacesPreviousAndSurvivesReload() throws {
        var saved = SavedConnections(legacy: try connection("studio"))
        saved.save(try connection("laptop", token: "laptop-session"))
        saved.save(try connection("mini"))
        try saved.select("studio")
        try saved.select("laptop")
        saved.save(try connection("studio", token: "renewed"))
        let restored = try JSONDecoder().decode(SavedConnections.self, from: JSONEncoder().encode(saved))
        XCTAssertEqual(restored.selectedHostIDs, ["laptop"])
        XCTAssertFalse(restored.includes("studio"))
        XCTAssertFalse(restored.includes("mini"))
        XCTAssertEqual(restored.connections.first?.credential.sessionToken, "renewed")
        XCTAssertEqual(restored.connections[1].credential.sessionToken, "laptop-session")
    }

    func testRepeatedSelectionStaysSelectedAndAllIncludesNewComputers() throws {
        var saved = SavedConnections(legacy: try connection("studio"))
        saved.save(try connection("laptop"))
        XCTAssertTrue(saved.includes("laptop"))
        try saved.select("studio")
        XCTAssertFalse(saved.includes("laptop"))
        try saved.select("studio")
        XCTAssertEqual(saved.selectedHostIDs, ["studio"])
        try saved.select("studio")
        try saved.select("laptop")
        XCTAssertEqual(saved.selectedHostIDs, ["laptop"])
        saved.showAll()
        saved.save(try connection("mini"))
        XCTAssertTrue(saved.includes("mini"))
    }

    func testOldMultipleSelectionRestoresAll() throws {
        var saved = SavedConnections(legacy: try connection("studio"))
        saved.save(try connection("laptop"))
        var json = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(saved)) as? [String: Any])
        json["selectedHostIDs"] = ["studio", "laptop"]
        let restored = try JSONDecoder().decode(SavedConnections.self, from: JSONSerialization.data(withJSONObject: json))
        XCTAssertTrue(restored.selectedHostIDs.isEmpty)
    }

    func testRepairAndRemovalAffectOnlyMatchingMac() throws {
        var saved = SavedConnections(legacy: try connection("studio", origin: "https://studio.tail.ts.net:8443"))
        saved.save(try connection("laptop"))
        saved.save(try connection("studio", token: "new", device: "new-device", origin: "https://studio.tail.ts.net:8444"))
        saved = try JSONDecoder().decode(SavedConnections.self, from: JSONEncoder().encode(saved))
        XCTAssertEqual(saved.connections.first?.origin, "https://studio.tail.ts.net:8444")
        XCTAssertEqual(saved.connections.first?.storageDeviceId, "phone")
        XCTAssertEqual(saved.connections[1].origin, "https://laptop.invalid")
        XCTAssertEqual(saved.connections.count, 2)
        XCTAssertEqual(saved.connections.first?.credential.deviceId, "new-device")
        try saved.select("studio")
        saved.remove("studio")
        XCTAssertEqual(saved.connections.first?.credential.hostInstallationId, "laptop")
        XCTAssertTrue(saved.selectedHostIDs.isEmpty)
        saved.remove("laptop")
        let restored = try JSONDecoder().decode(SavedConnections.self, from: JSONEncoder().encode(saved))
        XCTAssertTrue(restored.connections.isEmpty)
        XCTAssertThrowsError(try saved.select("missing"))
    }

    // Contract: the host owns the computer's name. The list shows the name the
    // host last reported, through host refreshes and replacement pairing; a
    // nickname saved by an earlier version is ignored.
    func testHostReportedNameSurvivesReloadAndIgnoresAnOldNickname() throws {
        var saved = SavedConnections(legacy: try connection("studio"))
        saved.save(try connection("laptop"))
        saved.save(try connection("studio", token: "renewed", device: "new-phone", origin: "https://new-address.invalid"))
        let refreshed = SavedConnection(origin: saved.connections[0].origin, credential: saved.connections[0].credential, hostName: "Studio Mac")
        saved.save(refreshed)
        var stored = try JSONSerialization.jsonObject(with: JSONEncoder().encode(saved)) as! [String: Any]
        stored["connectionNames"] = ["studio": "Old nickname"]
        saved = try JSONDecoder().decode(SavedConnections.self, from: JSONSerialization.data(withJSONObject: stored))
        XCTAssertEqual(saved.name(for: saved.connections[0]), "Studio Mac")
        XCTAssertEqual(saved.connections[0].credential.sessionToken, "renewed")
        XCTAssertEqual(saved.name(for: saved.connections[1]), "laptop")
    }

}
