import XCTest
@testable import WonderPairing

/// Echo the actual transport body, including URLSession's body stream form.
private final class StopBodyProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "stop-test.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        var data = request.httpBody ?? Data()
        if let stream = request.httpBodyStream {
            stream.open()
            defer { stream.close() }
            var buffer = [UInt8](repeating: 0, count: 1024)
            while stream.hasBytesAvailable {
                let count = stream.read(&buffer, maxLength: buffer.count)
                guard count > 0 else { break }
                data.append(contentsOf: buffer.prefix(count))
            }
        }
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 200,
            httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: data.isEmpty ? Data("{}".utf8) : data)
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

final class StopActionTests: XCTestCase {
    // The real Stop caller sends {} through PairingAPI once for each tap.
    // Retries may retain the generated body; they must never become new taps.
    func testStopTapsSendDistinctIDsAndExplicitReplaysKeepTheirID() async throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        struct Contract: Decodable {
            let path: String
            let first: [String: String]
            let second: [String: String]
        }
        let contract = try JSONDecoder().decode(Contract.self,
            from: Data(contentsOf: root.appendingPathComponent("tests/contracts/native/stop-actions.json")))
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StopBodyProtocol.self]
        let api = PairingAPI(configuration: configuration)
        let first: [String: String] = try await api.request(contract.path, origin: "https://stop-test.invalid", body: Data("{}".utf8))
        let second: [String: String] = try await api.request(contract.path, origin: "https://stop-test.invalid", body: Data("{}".utf8))
        XCTAssertNotNil(UUID(uuidString: try XCTUnwrap(first["ownerActionId"])))
        XCTAssertNotNil(UUID(uuidString: try XCTUnwrap(second["ownerActionId"])))
        XCTAssertNotEqual(first, second)
        for body in [first, first, contract.first, contract.first, contract.second] {
            let replay: [String: String] = try await api.request(contract.path, origin: "https://stop-test.invalid",
                body: JSONEncoder().encode(body))
            XCTAssertEqual(replay, body)
        }
        for (path, method) in [(contract.path, "GET"), ("/api/v1/conversations/c/messages", "POST"),
                               ("/api/v1/conversations/c/turns/t/background/interrupt", "POST")] {
            let other: [String: String] = try await api.request(path, origin: "https://stop-test.invalid", body: Data("{}".utf8), method: method)
            XCTAssertTrue(other.isEmpty)
        }
    }
}
