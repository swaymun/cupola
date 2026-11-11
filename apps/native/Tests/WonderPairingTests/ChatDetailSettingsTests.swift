import XCTest
@testable import WonderPairing

/// The phone reads chat detail from each Mac (`/api/v1/settings/chat-detail`),
/// caches that answer for offline display and writes changes back there.
final class ChatDetailSettingsTests: XCTestCase {
    func testReadsTheHostAnswerPerProvider() throws {
        let host = #"{"providers":[{"family":"codex","detail":"thinking"},{"family":"claude","detail":"conversation"},{"family":"opencode","detail":"everything"}]}"#
        let settings = try JSONDecoder().decode(ChatDetailSettings.self, from: Data(host.utf8))
        XCTAssertEqual(settings.detail(for: .claude), .conversation)
        XCTAssertEqual(settings.detail(for: .opencode), .everything)
        XCTAssertEqual(settings.detail(for: .codex), .thinking)
        // A conversation without a known provider shows everything.
        XCTAssertEqual(settings.detail(for: nil), .everything)
    }

    func testANewerHostsUnknownProviderOrLevelShowsEverythingWithoutLosingTheRest() throws {
        let host = #"{"providers":[{"family":"future","detail":"conversation"},{"family":"codex","detail":"summary"},{"family":"claude","detail":"conversation"}]}"#
        let settings = try JSONDecoder().decode(ChatDetailSettings.self, from: Data(host.utf8))
        XCTAssertEqual(settings.providers, [.init(family: .claude, detail: .conversation)])
        XCTAssertEqual(settings.detail(for: .codex), .everything)
    }

    func testTheOfflineCacheKeepsTheLastAnswer() throws {
        let answer = ChatDetailSettings(providers: [.init(family: .opencode, detail: .conversation)])
        let cached = try JSONDecoder().decode(ChatDetailSettings.self, from: JSONEncoder().encode(answer))
        XCTAssertEqual(cached, answer)
    }

    func testAChangeIsWrittenToTheProvidersHostRoute() throws {
        XCTAssertEqual(ChatDetailSettings.path(.claude), "/api/v1/settings/chat-detail/claude")
        let body = try JSONSerialization.jsonObject(with: ChatDetailSettings.request(.conversation)) as? [String: String]
        XCTAssertEqual(body, ["detail": "conversation"])
        let thinking = try JSONSerialization.jsonObject(with: ChatDetailSettings.request(.thinking)) as? [String: String]
        XCTAssertEqual(thinking, ["detail": "thinking"])
    }

    func testAChatsOwnChoiceBeatsItsProviderAndNullClearsIt() throws {
        let host = #"{"providers":[{"family":"codex","detail":"thinking"}],"chats":{"quiet":"conversation","odd":"future"}}"#
        let settings = try JSONDecoder().decode(ChatDetailSettings.self, from: Data(host.utf8))
        XCTAssertEqual(settings.detail(for: .codex, chat: "quiet"), .conversation)
        XCTAssertEqual(settings.detail(for: .codex, chat: "odd"), .thinking)
        XCTAssertEqual(settings.detail(for: .codex, chat: nil), .thinking)
        XCTAssertEqual(settings.setting(chat: "quiet", to: nil).chats, [:])
        // An older host sends no chats; the cache keeps the ones it has.
        let older = try JSONDecoder().decode(ChatDetailSettings.self, from: Data(#"{"providers":[]}"#.utf8))
        XCTAssertEqual(older.chats, [:])
        XCTAssertEqual(try JSONDecoder().decode(ChatDetailSettings.self, from: JSONEncoder().encode(settings)), settings)
        XCTAssertEqual(ChatDetailSettings.path(chat: "c1"), "/api/v1/conversations/c1/chat-detail")
        XCTAssertEqual(String(decoding: ChatDetailSettings.request(chat: nil), as: UTF8.self), #"{"detail":null}"#)
        XCTAssertEqual(String(decoding: ChatDetailSettings.request(chat: .thinking), as: UTF8.self), #"{"detail":"thinking"}"#)
    }
}
