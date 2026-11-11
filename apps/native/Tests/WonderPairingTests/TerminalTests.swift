import XCTest
@testable import WonderPairing

final class TerminalTests: XCTestCase {
    private func frame(_ sequence: UInt64, _ text: String) -> Data {
        withUnsafeBytes(of: sequence.bigEndian) { Data($0) } + Data(text.utf8)
    }

    func testHostFramesDecode() throws {
        XCTAssertEqual(try TerminalServerMessage.binary(frame(300, "ls\r\n")), .output(sequence: 300, bytes: Array("ls\r\n".utf8)))
        XCTAssertThrowsError(try TerminalServerMessage.binary(Data([1, 2, 3])))
        XCTAssertEqual(try TerminalServerMessage.text(#"{"type":"ready","sequence":12,"reset":false,"cols":80,"rows":24,"exited":false,"exitCode":null}"#),
                       .ready(sequence: 12, reset: false, cols: 80, rows: 24, exited: false, exitCode: nil))
        XCTAssertEqual(try TerminalServerMessage.text(#"{"type":"reset","sequence":4096}"#), .reset(sequence: 4096))
        XCTAssertEqual(try TerminalServerMessage.text(#"{"type":"exit","exitCode":130}"#), .exit(code: 130))
        XCTAssertEqual(try TerminalServerMessage.text(#"{"type":"closed"}"#), .closed)
        XCTAssertThrowsError(try TerminalServerMessage.text(#"{"type":"surprise"}"#))
    }

    /// A reconnect resumes from what was drawn: overlapping output is trimmed
    /// and already-drawn output is dropped, so nothing appears twice.
    func testReconnectReplayNeverDrawsABytesTwice() {
        var cursor = TerminalStreamCursor()
        XCTAssertEqual(cursor.accept(.ready(sequence: 0, reset: true, cols: 80, rows: 24, exited: false, exitCode: nil)), .clear)
        XCTAssertEqual(cursor.accept(.output(sequence: 0, bytes: Array("hello ".utf8))), .feed(Array("hello ".utf8)))
        XCTAssertEqual(cursor.held, 6)
        // The view reattaches holding 6; the host agrees and sends the rest.
        XCTAssertEqual(cursor.accept(.ready(sequence: 6, reset: false, cols: 80, rows: 24, exited: false, exitCode: nil)), .none)
        XCTAssertEqual(cursor.accept(.output(sequence: 3, bytes: Array("lo world".utf8))), .feed(Array("world".utf8)))
        XCTAssertEqual(cursor.accept(.output(sequence: 0, bytes: Array("hello world".utf8))), .none)
        XCTAssertEqual(cursor.held, 11)
    }

    /// An offset near UInt64.max from the socket must not trap the cursor.
    func testOutputOffsetPastTheEndOfTheStreamIsDropped() {
        var cursor = TerminalStreamCursor(held: 10)
        XCTAssertEqual(cursor.accept(.output(sequence: UInt64.max - 1, bytes: [0x41, 0x42, 0x43])), .none)
        XCTAssertEqual(cursor.held, 10)
        var fresh = TerminalStreamCursor()
        XCTAssertEqual(fresh.accept(.output(sequence: UInt64.max, bytes: [0x41])), .none)
        XCTAssertNil(fresh.held)
    }

    func testTruncatedScrollbackClearsBeforeReplay() {
        var cursor = TerminalStreamCursor(held: 10)
        XCTAssertEqual(cursor.accept(.ready(sequence: 5_000, reset: true, cols: 80, rows: 24, exited: false, exitCode: nil)), .clear)
        XCTAssertEqual(cursor.accept(.output(sequence: 5_000, bytes: [0x41])), .feed([0x41]))
        XCTAssertEqual(cursor.accept(.reset(sequence: 9_000)), .clear)
        XCTAssertEqual(cursor.held, 9_000)
    }

    func testAttachCarriesProofAndHeldOffset() throws {
        let attach = TerminalAttach(deviceId: "d", csrfToken: "c", challengeId: "ch", signature: "s", sequence: 42, cols: 90, rows: 30)
        let json = try JSONSerialization.jsonObject(with: JSONEncoder().encode(attach)) as? [String: Any]
        XCTAssertEqual(json?["sequence"] as? Int, 42)
        XCTAssertEqual(json?["challengeId"] as? String, "ch")
        XCTAssertEqual(json?["cols"] as? Int, 90)
        XCTAssertEqual(OpenTerminalRequest(cols: 1, rows: 9_999), OpenTerminalRequest(cols: 2, rows: 300))
    }

    func testPasteBracketsOnlyWhenTheProgramAsks() {
        XCTAssertEqual(TerminalPaste.bytes("ls\r\necho hi", bracketed: false), Array("ls\recho hi".utf8))
        XCTAssertEqual(TerminalPaste.bytes("a\nb", bracketed: true), Array("\u{1b}[200~a\nb\u{1b}[201~".utf8))
        // An embedded end marker cannot close the bracket and run the rest.
        XCTAssertEqual(TerminalPaste.bytes("x\u{1b}[201~rm -rf", bracketed: true), Array("\u{1b}[200~x[201~rm -rf\u{1b}[201~".utf8))
    }

    func testSocketURLRejectsUnexpectedTerminalIDs() throws {
        let saved = SavedConnection(origin: "https://mac.example.ts.net",
                                    credential: Credential(sessionToken: "t", deviceId: "d", csrfToken: "c", hostInstallationId: "h", expiresAtMs: nil))
        let api = PairingAPI()
        let task = try api.terminalSocket(connection: saved, terminalID: "term_0123abcd")
        XCTAssertEqual(task.originalRequest?.url?.absoluteString, "wss://mac.example.ts.net/api/v1/terminals/term_0123abcd/socket")
        XCTAssertEqual(task.originalRequest?.value(forHTTPHeaderField: "Cookie"), "__Host-wonder_session=t")
        task.cancel()
        XCTAssertThrowsError(try api.terminalSocket(connection: saved, terminalID: "term_../../events"))
        XCTAssertThrowsError(try api.terminalSocket(connection: saved, terminalID: "other"))
    }

    func testProjectsResponseReadsTerminalVersion() throws {
        let body = #"{"projects":[],"families":[],"modesVersion":1,"terminalVersion":1}"#
        XCTAssertEqual(try JSONDecoder().decode(ProjectsResponse.self, from: Data(body.utf8)).terminalVersion, 1)
        let older = #"{"projects":[],"families":[]}"#
        XCTAssertNil(try JSONDecoder().decode(ProjectsResponse.self, from: Data(older.utf8)).terminalVersion)
    }
}
