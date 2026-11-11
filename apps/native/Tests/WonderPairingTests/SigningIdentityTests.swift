import XCTest
import CryptoKit
@testable import WonderPairing

private final class IdentityMemory: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Data?
    private var log: [String] = []
    init(_ value: Data?) { self.value = value }
    func read() -> Data? { lock.withLock { value } }
    func save(_ data: Data) { lock.withLock { log.append("save"); value = data } }
    func record(_ event: String) { lock.withLock { log.append(event) } }
    var events: [String] { lock.withLock { log } }
}

private actor IdentityGate {
    var entered = false
    private var release: CheckedContinuation<Void, Never>?
    func wait() async {
        entered = true
        await withCheckedContinuation { release = $0 }
    }
    func open() { release?.resume(); release = nil }
}

@MainActor final class SigningIdentityTests: XCTestCase {
    private nonisolated static func wrap(_ key: P256.Signing.PrivateKey) -> EnrollmentSigningIdentity {
        EnrollmentSigningIdentity(publicKey: key.publicKey, representation: key.rawRepresentation,
                                  sign: { try key.signature(for: $0) })
    }
    private func fixture(_ storage: IdentityMemory, failure: NSError = NSError(domain: "CryptoTokenKit", code: -10),
                         signingFailure: Bool = false, saveFailure: Bool = false) -> SigningIdentity {
        let replacement = P256.Signing.PrivateKey()
        return SigningIdentity(read: { storage.read() }, save: {
            if saveFailure { storage.record("save-failed"); throw SigningIdentityFailure.keychain(-25291) }
            storage.save($0)
        }, restore: { data in
            if data == Data("old-invalid-key".utf8) {
                if signingFailure {
                    return EnrollmentSigningIdentity(publicKey: replacement.publicKey, representation: data, sign: { _ in throw failure })
                }
                throw failure
            }
            return try Self.wrap(P256.Signing.PrivateKey(rawRepresentation: data))
        }, create: {
            storage.record("create")
            return Self.wrap(replacement)
        })
    }

    func testOnlyMissingAndExactPermanentDeviceErrorRequirePairing() {
        let invalid = NSError(domain: "CryptoTokenKit", code: -10)
        XCTAssertTrue(SigningIdentityFailure.requiresPairing(invalid))
        XCTAssertTrue(SigningIdentityFailure.requiresPairing(NSError(domain: "wrapper", code: 3, userInfo: [NSUnderlyingErrorKey: invalid])))
        XCTAssertTrue(SigningIdentityFailure.requiresPairing(SigningIdentityFailure.missing))
        for code in [-2, -3, -4, -5, -6, -7, -8, -9] {
            XCTAssertFalse(SigningIdentityFailure.requiresPairing(NSError(domain: "CryptoTokenKit", code: code)))
        }
        XCTAssertFalse(SigningIdentityFailure.requiresPairing(NSError(domain: "other", code: -10)))
        XCTAssertFalse(SigningIdentityFailure.requiresPairing(SigningIdentityFailure.keychain(-10)))
    }

    func testHealthyKeyAndOrdinarySigningNeverCreateOrMarkConnections() async throws {
        let key = P256.Signing.PrivateKey()
        let storage = IdentityMemory(key.rawRepresentation)
        let identity = fixture(storage)
        let prepared = try await identity.prepareForEnrollment { storage.record("mark") }
        XCTAssertEqual(prepared.representation, key.rawRepresentation)
        _ = try await identity.sign(Data("normal signing".utf8))
        XCTAssertTrue(storage.events.isEmpty)
    }

    func testInvalidBlobAndFirstSigningFailureRecoverBeforeEnrollment() async throws {
        for signingFailure in [false, true] {
            let storage = IdentityMemory(Data("old-invalid-key".utf8))
            let identity = fixture(storage, signingFailure: signingFailure)
            let prepared = try await identity.prepareForEnrollment { storage.record("mark") }
            XCTAssertEqual(storage.events, ["create", "mark", "save"])
            XCTAssertEqual(storage.read(), prepared.representation)
            let challenge = Data("server challenge".utf8)
            let signature = try await identity.sign(challenge, using: prepared)
            XCTAssertFalse(signature.isEmpty)
            // Retain the same enrollment key even if storage later changes.
            storage.save(P256.Signing.PrivateKey().rawRepresentation)
            let encoded = try await identity.sign(challenge, using: prepared)
            let raw = encoded.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
            let signatureData = try XCTUnwrap(Data(base64Encoded: raw + String(repeating: "=", count: (4 - raw.count % 4) % 4)))
            let original = try P256.Signing.PrivateKey(rawRepresentation: prepared.representation)
            XCTAssertTrue(original.publicKey.isValidSignature(try P256.Signing.ECDSASignature(rawRepresentation: signatureData), for: challenge))
        }
    }

    func testNormalSigningDoesNotRepairMissingOrInvalidKeys() async {
        for data in [nil, Data("old-invalid-key".utf8)] {
            let storage = IdentityMemory(data)
            let identity = fixture(storage)
            do { _ = try await identity.sign(Data()); XCTFail("Expected identity failure") }
            catch { XCTAssertTrue(SigningIdentityFailure.requiresPairing(error)) }
            XCTAssertTrue(storage.events.isEmpty)
            XCTAssertEqual(storage.read(), data)
        }
    }

    func testTransientAndCorruptKeyFailuresDoNotRotate() async {
        for code in [-2, -3, -4, -5, -9] {
            let storage = IdentityMemory(Data("old-invalid-key".utf8))
            let identity = fixture(storage, failure: NSError(domain: "CryptoTokenKit", code: code))
            do { _ = try await identity.prepareForEnrollment { storage.record("mark") }; XCTFail("Expected original failure") }
            catch { XCTAssertEqual((error as NSError).code, code) }
            XCTAssertTrue(storage.events.isEmpty)
        }
    }

    func testReadAndMarkerFailuresNeverOverwriteSavedIdentity() async {
        let original = Data("old-invalid-key".utf8)
        let storage = IdentityMemory(original)
        let identity = fixture(storage)
        do {
            _ = try await identity.prepareForEnrollment {
                storage.record("mark-failed")
                throw SigningIdentityFailure.keychain(-25291)
            }
            XCTFail("Expected marker failure")
        } catch { }
        XCTAssertEqual(storage.events, ["create", "mark-failed"])
        XCTAssertEqual(storage.read(), original)
        do {
            _ = try await identity.prepareForEnrollment { storage.record("mark-retry") }
            XCTAssertNotEqual(storage.read(), original)
        } catch { XCTFail("A failed marker must not leave preparation stuck: \(error)") }
        let readFailure = SigningIdentity(read: { throw SigningIdentityFailure.keychain(-25308) },
                                          save: { _ in XCTFail("Must not save") },
                                          restore: { _ in XCTFail("Must not restore"); throw SigningIdentityFailure.missing },
                                          create: { XCTFail("Must not create"); throw SigningIdentityFailure.missing })
        do { _ = try await readFailure.prepareForEnrollment { XCTFail("Must not mark") }; XCTFail("Expected locked failure") }
        catch { XCTAssertFalse(SigningIdentityFailure.requiresPairing(error)) }
    }

    func testFailedReplacementSaveKeepsOldBlobAndAllowsRetry() async throws {
        let original = Data("old-invalid-key".utf8)
        let storage = IdentityMemory(original)
        let identity = fixture(storage, saveFailure: true)
        do { _ = try await identity.prepareForEnrollment { storage.record("mark") }; XCTFail("Expected save failure") }
        catch { }
        XCTAssertEqual(storage.events, ["create", "mark", "save-failed"])
        XCTAssertEqual(storage.read(), original)
        let retry = fixture(storage)
        _ = try await retry.prepareForEnrollment { storage.record("mark-again") }
        XCTAssertNotEqual(storage.read(), original)
    }

    func testConcurrentPairingSharesOneReplacementAndDurableMarker() async throws {
        let storage = IdentityMemory(Data("old-invalid-key".utf8))
        let identity = fixture(storage)
        let gate = IdentityGate()
        let first = Task {
            try await identity.prepareForEnrollment {
                storage.record("mark")
                await gate.wait()
            }
        }
        while !(await gate.entered) { await Task.yield() }
        let second = Task { try await identity.prepareForEnrollment { storage.record("unexpected-second-marker") } }
        await Task.yield()
        await gate.open()
        let a = try await first.value
        let b = try await second.value
        XCTAssertEqual(a.representation, b.representation)
        XCTAssertEqual(storage.events, ["create", "mark", "save"])
    }

    func testFirstIdentityCreationAlsoWaitsForSavedConnectionMarkers() async throws {
        let storage = IdentityMemory(nil)
        _ = try await fixture(storage).prepareForEnrollment { storage.record("mark") }
        XCTAssertEqual(storage.events, ["create", "mark", "save"])
    }

    func testOldEnrollmentCannotPublishOrSignAfterAnotherRecovery() async throws {
        let storage = IdentityMemory(P256.Signing.PrivateKey().rawRepresentation)
        let identity = fixture(storage)
        let old = try await identity.prepareForEnrollment { storage.record("unexpected-marker") }
        try await identity.validateCurrent(old)
        storage.save(Data("old-invalid-key".utf8))
        let gate = IdentityGate()
        let replacement = Task {
            try await identity.prepareForEnrollment { await gate.wait() }
        }
        while !(await gate.entered) { await Task.yield() }
        // The synchronous main-actor publication fence closes before the
        // marker callback finishes or the replacement is written.
        XCTAssertThrowsError(try old.checkCurrent())
        do { _ = try await identity.sign(Data(), using: old); XCTFail("Old enrollment must not sign") }
        catch { XCTAssertTrue(error is CancellationError) }
        await gate.open()
        let new = try await replacement.value
        do { try await identity.validateCurrent(old); XCTFail("Old enrollment must not publish credentials") }
        catch { XCTAssertTrue(error is CancellationError) }
        try await identity.validateCurrent(new)
        XCTAssertNoThrow(try new.checkCurrent())
    }
}
