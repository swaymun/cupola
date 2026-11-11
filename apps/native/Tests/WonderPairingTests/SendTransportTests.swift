import XCTest
import CryptoKit
@testable import WonderPairing

/// Models only durable HTTP acceptance; execution recovery is covered by Rust fixtures.
private final class AcceptanceProtocol: URLProtocol, @unchecked Sendable {
    final class State: @unchecked Sendable {
        let lock = NSLock()
        var requests: [SendRequest] = []
        var dispatches: Set<String> = []
    }
    static let state = State()
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "send-test.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        do {
            XCTAssertEqual(request.httpMethod, "POST")
            XCTAssertEqual(request.url?.path, "/api/v1/conversations/chat/messages")
            XCTAssertEqual(request.value(forHTTPHeaderField: "Origin"), "https://send-test.invalid")
            XCTAssertEqual(request.value(forHTTPHeaderField: "Cookie"), "__Host-wonder_session=synthetic")
            XCTAssertEqual(request.value(forHTTPHeaderField: "x-wonder-csrf"), "synthetic-csrf")
            var data = request.httpBody ?? Data()
            if let stream = request.httpBodyStream {
                stream.open(); defer { stream.close() }
                var bytes = [UInt8](repeating: 0, count: 1024)
                while stream.hasBytesAvailable {
                    let count = stream.read(&bytes, maxLength: bytes.count)
                    if count <= 0 { break }; data.append(contentsOf: bytes.prefix(count))
                }
            }
            let body = try JSONDecoder().decode(SendRequest.self, from: data)
            let first = Self.state.lock.withLock {
                Self.state.requests.append(body)
                return Self.state.dispatches.insert(body.clientMessageId).inserted
            }
            if first {
                // Commit acceptance and lose the reply before the phone sees it.
                client?.urlProtocol(self, didFailWithError: URLError(.networkConnectionLost))
                return
            }
            let receipt = SendReceipt(clientMessageId: body.clientMessageId, wonderMessageId: "server-id",
                bodySha256: SHA256.hash(data: Data(body.body.utf8)).map { String(format: "%02x", $0) }.joined(),
                conversationId: "chat", deliveryState: "uncertain")
            client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 202,
                httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: try JSONEncoder().encode(receipt))
            client?.urlProtocolDidFinishLoading(self)
        } catch { client?.urlProtocol(self, didFailWithError: error) }
    }
    override func stopLoading() {}
}

final class SendTransportTests: XCTestCase {
    func testReleasingTransportInvalidatesItsSession() async throws {
        var api: PairingAPI? = PairingAPI()
        weak var owner = api
        let connection = SavedConnection(origin: "https://send-test.invalid", credential: Credential(
            sessionToken: "synthetic", deviceId: "phone", csrfToken: "synthetic-csrf", hostInstallationId: "test", expiresAtMs: nil))
        // An unresumed socket performs no network I/O, but keeps the session
        // alive so cancellation proves the owner invalidated it explicitly.
        let socket = try api!.eventSocket(connection: connection)
        api = nil
        XCTAssertNil(owner)
        for _ in 0..<100 where socket.state != .completed {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(socket.state, .completed)
        XCTAssertEqual((socket.error as? URLError)?.code, .cancelled)
    }
    func testAcceptedReplyLossThenPhoneRestartChecksSameSubmission() async throws {
        AcceptanceProtocol.state.lock.withLock {
            AcceptanceProtocol.state.requests = []; AcceptanceProtocol.state.dispatches = []
        }
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ReadStore(root: root, host: "test", device: "phone")
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [AcceptanceProtocol.self]
        let api = PairingAPI(configuration: config)
        let credential = Credential(sessionToken: "synthetic", deviceId: "phone", csrfToken: "synthetic-csrf", hostInstallationId: "test", expiresAtMs: nil)
        var intent = ComposerIntent(); intent.draft = "One action only 🌍"
        try intent.begin(device: "phone"); try store.saveComposer(intent, conversation: "chat")
        do {
            let _: SendReceipt = try await api.request("/api/v1/conversations/chat/messages", origin: "https://send-test.invalid",
                body: JSONEncoder().encode(intent.pending!.request), credential: credential)
            XCTFail("The first response should be lost")
        } catch { XCTAssertEqual((error as? URLError)?.code, .networkConnectionLost) }
        var restarted = try store.loadComposer(conversation: "chat")
        let receipt: SendReceipt = try await api.request("/api/v1/conversations/chat/messages", origin: "https://send-test.invalid",
            body: JSONEncoder().encode(restarted.pending!.request), credential: credential)
        try restarted.accept(receipt, conversation: "chat")
        try store.saveComposer(restarted, conversation: "chat")
        XCTAssertEqual(receipt.deliveryState, "uncertain")
        let (requests, dispatches) = AcceptanceProtocol.state.lock.withLock { (AcceptanceProtocol.state.requests, AcceptanceProtocol.state.dispatches) }
        XCTAssertEqual(requests.count, 2)
        XCTAssertEqual(dispatches.count, 1)
        XCTAssertEqual(requests[0].body, requests[1].body)
        XCTAssertEqual(requests[0].clientMessageId, requests[1].clientMessageId)
    }
}
