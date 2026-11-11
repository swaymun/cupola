import XCTest
@testable import WonderComputerUseCore

final class SharedDisplayPreferenceTests: XCTestCase {
    func testChoicePersistsAcrossReadersAndClearingRestoresMainDefault() {
        let suite = "WonderSharedDisplayTests-\(UUID().uuidString)"
        let writerDefaults = UserDefaults(suiteName: suite)!
        defer { writerDefaults.removePersistentDomain(forName: suite) }
        let writer = SharedDisplayPreference(defaults: writerDefaults)
        XCTAssertNil(writer.preferredIdentifier)

        XCTAssertTrue(writer.setPreferredIdentifier("123:456:789"))
        let reader = SharedDisplayPreference(defaults: UserDefaults(suiteName: suite)!)
        XCTAssertEqual(reader.preferredIdentifier, "123:456:789")

        XCTAssertTrue(writer.setPreferredIdentifier(nil))
        XCTAssertNil(SharedDisplayPreference(defaults: UserDefaults(suiteName: suite)!).preferredIdentifier)
    }

    func testMalformedChoiceDoesNotBecomeASelectedDisplay() {
        let suite = "WonderSharedDisplayTests-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        defaults.set("not a display", forKey: SharedDisplayPreference.key)
        XCTAssertNil(SharedDisplayPreference(defaults: defaults).preferredIdentifier)
    }
}
