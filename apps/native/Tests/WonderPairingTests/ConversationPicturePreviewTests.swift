import Foundation
import XCTest
@testable import WonderPairing

/// A Mac answering conversation picture requests: `?maxPixels=` gets a
/// phone-sized JPEG naming its own hash, unless the test asks otherwise.
private final class PictureProtocol: URLProtocol, @unchecked Sendable {
    static let original = Data([137, 80, 78, 71, 13, 10, 26, 10]) + Data(repeating: 7, count: 4000)
    static let preview = Data([255, 216, 255, 224]) + Data(repeating: 9, count: 300)
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "picture-test.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let url = request.url!
        let file = url.pathComponents.last ?? ""
        let sized = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == "maxPixels" }?.value == "1080"
        switch (file, sized) {
        case ("gone", _):
            reply(410, Data("This picture is no longer on your computer.".utf8),
                  ["Content-Type": "text/plain", PairingAPI.errorCodeHeader: PairingAPI.mediaSourceGone])
        case (_, false), ("small", true):
            reply(200, Self.original, ["Content-Type": "image/png"])
        case ("forged", true):
            reply(200, Self.preview, ["Content-Type": "image/jpeg",
                                      PairingAPI.previewSha256Header: String(repeating: "0", count: 64)])
        case ("not-jpeg", true):
            reply(200, Self.original, ["Content-Type": "image/jpeg",
                                       PairingAPI.previewSha256Header: ConversationFile.digest(Self.original)])
        default:
            reply(200, Self.preview, ["Content-Type": "image/jpeg",
                                      PairingAPI.previewSha256Header: ConversationFile.digest(Self.preview)])
        }
    }
    override func stopLoading() {}
    private func reply(_ status: Int, _ body: Data, _ headers: [String: String]) {
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: status,
            httpVersion: "HTTP/1.1", headerFields: headers)!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: body)
        client?.urlProtocolDidFinishLoading(self)
    }
}

final class ConversationPicturePreviewTests: XCTestCase {
    private let api: PairingAPI = {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [PictureProtocol.self]
        return PairingAPI(configuration: configuration)
    }()
    private let connection = SavedConnection(origin: "https://picture-test.invalid", credential: Credential(
        sessionToken: "synthetic", deviceId: "phone", csrfToken: "csrf", hostInstallationId: "host", expiresAtMs: nil))
    private let file = ConversationFile(id: "f", name: "Screenshot.png", mimeType: "image/png",
        byteSize: PictureProtocol.original.count, sha256: ConversationFile.digest(PictureProtocol.original),
        state: "available", updatedAt: "now", width: 2400, height: 1600)

    private func preview(_ id: String) async throws -> Data {
        try await api.downloadPreview("/api/v1/conversations/c/files/\(id)", connection: connection, file: file, maxPixels: 1080)
    }

    /// The thumbnail is the Mac's JPEG, checked against the hash it names;
    /// a picture already small (or an older Mac) comes back as the original,
    /// checked against the conversation's own hash; the full picture still
    /// downloads in full.
    func testPreviewsAreVerifiedAgainstTheirOwnHashOrTheOriginals() async throws {
        let shrunk = try await preview("f")
        XCTAssertEqual(shrunk, PictureProtocol.preview)
        let small = try await preview("small")
        XCTAssertEqual(small, PictureProtocol.original)
        let full = try await api.download("/api/v1/conversations/c/files/f", connection: connection, file: file)
        XCTAssertEqual(full, PictureProtocol.original)
        for id in ["forged", "not-jpeg"] {
            do { _ = try await preview(id); XCTFail("\(id) must not be shown") }
            catch FileFailure.integrity {}
        }
    }

    /// A picture the Mac kept by reference whose original is gone says so,
    /// for the thumbnail and the full picture alike.
    func testGonePicturesAreToldApartFromFailures() async throws {
        do { _ = try await preview("gone"); XCTFail("gone") } catch FileFailure.gone {}
        do {
            _ = try await api.download("/api/v1/conversations/c/files/gone", connection: connection, file: file)
            XCTFail("gone")
        } catch FileFailure.gone {}
    }

    func testPreviewsOfOtherFilesAreRefused() {
        let pdf = ConversationFile(id: "p", name: "a.pdf", mimeType: "application/pdf", byteSize: 1, sha256: "x",
                                   state: "available", updatedAt: "now")
        XCTAssertThrowsError(try ConversationFile.verifyPreview(PictureProtocol.preview, mime: "image/jpeg",
            sha256: ConversationFile.digest(PictureProtocol.preview), of: pdf))
        XCTAssertNoThrow(try ConversationFile.verifyPreview(PictureProtocol.preview, mime: "image/jpeg",
            sha256: ConversationFile.digest(PictureProtocol.preview), of: file))
    }
}
