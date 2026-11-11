import Foundation
import XCTest
@testable import WonderComputerUseCore

final class ControlPreferencesTests: XCTestCase {
    private let fileManager = FileManager.default

    private func directory() throws -> URL {
        let directory = fileManager.homeDirectoryForCurrentUser
            .appendingPathComponent(".wonder-control-preference-tests-\(UUID().uuidString)", isDirectory: true)
        try fileManager.createDirectory(
            at: directory,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: NSNumber(value: 0o700)]
        )
        return directory
    }

    private func preferencesFile(in directory: URL) -> URL {
        directory.appendingPathComponent(ControlPreferencesStore.fileName)
    }

    private func setPermissions(_ permissions: Int, at url: URL) throws {
        try fileManager.setAttributes(
            [.posixPermissions: NSNumber(value: permissions)],
            ofItemAtPath: url.path
        )
    }

    private func writeRaw(_ value: String, to url: URL) throws {
        try Data(value.utf8).write(to: url)
        try setPermissions(0o600, at: url)
    }

    func testMissingPreferenceDefaultsToDisabled() throws {
        let directory = try directory()
        defer { try? fileManager.removeItem(at: directory) }
        let store = ControlPreferencesStore(serviceDirectory: directory)

        XCTAssertEqual(store.state, .disabled)
        XCTAssertFalse(store.isEnabled)
    }

    func testEnablePersistsAcrossStoreReloadAndDisablePersists() throws {
        let directory = try directory()
        defer { try? fileManager.removeItem(at: directory) }
        let store = ControlPreferencesStore(serviceDirectory: directory)

        try store.setAllowControlFromPairedDevices(true)
        XCTAssertEqual(ControlPreferencesStore(serviceDirectory: directory).state, .enabled)

        try store.setAllowControlFromPairedDevices(false)
        XCTAssertEqual(ControlPreferencesStore(serviceDirectory: directory).state, .disabled)
    }

    func testMalformedPreferenceFailsClosed() throws {
        let directory = try directory()
        defer { try? fileManager.removeItem(at: directory) }
        let store = ControlPreferencesStore(serviceDirectory: directory)
        try writeRaw(#"{"version":2,"allowControlFromPairedDevices":true}"#, to: preferencesFile(in: directory))

        XCTAssertEqual(store.state, .unavailable)
        XCTAssertFalse(store.isEnabled)
    }

    func testMissingOrSymlinkedPreferenceAndDirectoryFailClosed() throws {
        let directory = try directory()
        defer { try? fileManager.removeItem(at: directory) }
        let store = ControlPreferencesStore(serviceDirectory: directory)
        let target = directory.appendingPathComponent("target.json")
        try writeRaw(#"{"version":1,"allowControlFromPairedDevices":true}"#, to: target)
        try fileManager.createSymbolicLink(at: preferencesFile(in: directory), withDestinationURL: target)
        XCTAssertEqual(store.state, .unavailable)
        XCTAssertFalse(store.isEnabled)

        let linkedDirectory = directory.deletingLastPathComponent().appendingPathComponent(UUID().uuidString)
        try fileManager.createSymbolicLink(at: linkedDirectory, withDestinationURL: directory)
        defer { try? fileManager.removeItem(at: linkedDirectory) }
        let linkedStore = ControlPreferencesStore(serviceDirectory: linkedDirectory)
        XCTAssertEqual(linkedStore.state, .unavailable)
        XCTAssertFalse(linkedStore.isEnabled)
    }

    func testUnsafeDirectoryOrFilePermissionsFailClosed() throws {
        let directory = try directory()
        defer { try? fileManager.removeItem(at: directory) }
        let store = ControlPreferencesStore(serviceDirectory: directory)
        try store.setAllowControlFromPairedDevices(true)

        try setPermissions(0o755, at: directory)
        XCTAssertEqual(store.state, .unavailable)
        XCTAssertFalse(store.isEnabled)

        try setPermissions(0o700, at: directory)
        try setPermissions(0o644, at: preferencesFile(in: directory))
        XCTAssertEqual(store.state, .unavailable)
        XCTAssertFalse(store.isEnabled)
    }

    func testHelperConsentAndActiveLeaseFailClosedWhenPreferenceIsDisabled() throws {
        var enabled = false
        let gate = ControlLeaseGate(
            consentProvider: { _ in .allowOnce },
            enabledProvider: { enabled }
        )
        let identity = ControlLeaseIdentity(
            leaseID: "lease", requestID: "request", sessionID: "session",
            generation: 1, geometryRevision: 2, sourceID: "display:1"
        )

        XCTAssertEqual(gate.requestConsent(identity), .denied(replayed: false))
        enabled = true
        XCTAssertEqual(gate.requestConsent(identity), .allowOnce(replayed: false))
        XCTAssertEqual(
            gate.activate(identity, now: Date(timeIntervalSince1970: 10), lifetime: 10),
            .activated(replayed: false)
        )
        enabled = false
        XCTAssertFalse(gate.heartbeat(identity, now: Date(timeIntervalSince1970: 11), lifetime: 10))
        XCTAssertEqual(
            gate.deliver(identity, sequence: 1, payload: Data("input".utf8)) { XCTFail("disabled control must not deliver input"); return true },
            .rejected("control_disabled")
        )
        XCTAssertNil(gate.activeIdentity())
    }

    func testFreshHelperGateAfterRestartUsesPersistedOptInWithoutInteractivePrompt() throws {
        let directory = try directory()
        defer { try? fileManager.removeItem(at: directory) }
        try ControlPreferencesStore(serviceDirectory: directory)
            .setAllowControlFromPairedDevices(true)

        // This is the helper's non-interactive local policy: the persisted
        // preference is the owner decision, so no modal/user decision is used.
        let restartedStore = ControlPreferencesStore(serviceDirectory: directory)
        let gate = ControlLeaseGate(
            consentProvider: { _ in .allowOnce },
            enabledProvider: { restartedStore.isEnabled }
        )
        let freshIdentity = ControlLeaseIdentity(
            leaseID: "fresh-lease", requestID: "fresh-request", sessionID: "fresh-session",
            generation: 2, geometryRevision: 3, sourceID: "display:1"
        )

        XCTAssertEqual(gate.requestConsent(freshIdentity), .allowOnce(replayed: false))
        XCTAssertEqual(
            gate.activate(freshIdentity, now: Date(timeIntervalSince1970: 10), lifetime: 10),
            .activated(replayed: false)
        )

        try ControlPreferencesStore(serviceDirectory: directory)
            .setAllowControlFromPairedDevices(false)
        let disabledIdentity = ControlLeaseIdentity(
            leaseID: "disabled-lease", requestID: "disabled-request", sessionID: "disabled-session",
            generation: 1, geometryRevision: 1, sourceID: "display:1"
        )
        XCTAssertEqual(gate.requestConsent(disabledIdentity), .denied(replayed: false))
        XCTAssertNil(gate.activeIdentity())
    }
}
