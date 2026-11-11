import XCTest
@testable import WonderPairing

final class SecretsTests: XCTestCase {
    func testNamesOnlyInventoryAndDeletionStayScoped() throws {
        let json = #"{"secrets":[{"secretRef":"secretRef:fixture-vault","name":"TEST_TOKEN","projectId":"fixture-project","projectName":"Weekend project","uses":[{"conversationId":"fixture-chat","provider":"claude","title":"Release checks"}]}]}"#
        let list = try JSONDecoder().decode(SecretList.self, from: Data(json.utf8))
        XCTAssertEqual(list.secrets.count, 1)
        let secret = try XCTUnwrap(list.secrets.first)
        XCTAssertEqual(secret.name, "TEST_TOKEN")
        XCTAssertEqual(secret.uses.first?.provider, "claude")
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: secret.deletionBody()) as? [String: String])
        XCTAssertEqual(body, ["projectId": "fixture-project", "secretRef": "secretRef:fixture-vault"])
        XCTAssertNil(body["value"])
    }
}
