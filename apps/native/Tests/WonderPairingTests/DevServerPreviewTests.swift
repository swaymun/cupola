import XCTest
@testable import WonderPairing

// Contract: preview pages map only to the Mac's proxy for their own port and
// Project, carry the session in headers (never the URL or the page), keep
// Wonder's response headers away from the page, and the globe pill follows
// detected servers with a grace period.
final class DevServerPreviewTests: XCTestCase {
    private let connection = SavedConnection(origin: "https://mac.example.ts.net",
        credential: Credential(sessionToken: "token", deviceId: "phone", csrfToken: "binding",
                               hostInstallationId: "host", expiresAtMs: nil))

    func testSharedFixtureDecodes() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("packages/protocol/fixtures/dev-server-list-v1.json"))
        let list = try JSONDecoder().decode(DevServerList.self, from: data)
        XCTAssertEqual(list.servers.map(\.port), [3000, 5173])
        // A page without a title is named by its port, never by its program.
        XCTAssertEqual(list.servers.map(\.name), ["Recipes · Next.js", "Port 5173"])
        XCTAssertEqual(try DevServerList.path(projectId: "p-1"), "/api/v1/projects/p%2D1/preview/servers")
        XCTAssertEqual(try DevServerList.path(projectId: "p-1", conversationId: "c-1"),
                       "/api/v1/projects/p%2D1/preview/servers?conversation=c%2D1")
        XCTAssertEqual(try DevServerList.path(projectId: "p-1", conversationId: "c-1", port: 8000),
                       "/api/v1/projects/p%2D1/preview/servers?conversation=c%2D1&port=8000")
    }

    func testPageAddressesMapToTheirPortOnly() throws {
        let page = try XCTUnwrap(DevServerPreview.url(port: 5173, path: "/assets/app.js?v=1"))
        XCTAssertEqual(page.absoluteString, "wonder-preview://port-5173/assets/app.js?v=1")
        XCTAssertEqual(DevServerPreview.port(of: page), 5173)
        XCTAssertEqual(DevServerPreview.hostPath(for: page, projectId: "p1"), "/api/v1/projects/p1/preview/5173/assets/app.js?v=1")
        XCTAssertEqual(DevServerPreview.hostPath(for: URL(string: "wonder-preview://port-5173")!, projectId: "p1"),
                       "/api/v1/projects/p1/preview/5173/")
        XCTAssertEqual(DevServerPreview.hostPath(for: URL(string: "wonder-preview://port-5173/@vite/client")!, projectId: "p1"),
                       "/api/v1/projects/p1/preview/5173/@vite/client")
        for refused in [
            "wonder-preview://port-5173/a/%2e%2e/%2e%2e/conversations",
            "wonder-preview://port-5173/a%2f..%2fb",
            "wonder-preview://port-5173/a%5Cb",
            "wonder-preview://port-0/",
            "wonder-preview://port-70000/",
            "wonder-preview://port-5173:80/",
            "wonder-preview://user@port-5173/",
            "wonder-preview://evil.example/",
            "https://port-5173/",
        ] {
            XCTAssertNil(DevServerPreview.hostPath(for: URL(string: refused)!, projectId: "p1"), refused)
        }
        XCTAssertNil(DevServerPreview.hostPath(for: page, projectId: ""))
    }

    func testRequestsCarryTheSessionInHeadersAndOnlyAllowedPageHeaders() throws {
        var page = URLRequest(url: URL(string: "wonder-preview://port-3000/api/login")!)
        page.httpMethod = "POST"
        page.httpBody = Data("{}".utf8)
        page.setValue("application/json", forHTTPHeaderField: "Content-Type")
        page.setValue("Desktop UA", forHTTPHeaderField: "User-Agent")
        page.setValue("wonder-preview://port-3000", forHTTPHeaderField: "Origin")
        page.setValue("wonder-preview://port-3000/", forHTTPHeaderField: "Referer")
        page.setValue("Bearer page", forHTTPHeaderField: "Authorization")
        let request = try XCTUnwrap(DevServerPreview.request(for: page, projectId: "p1", conversationId: "c1",
                                                             connection: connection, cookies: "theme=dark"))
        XCTAssertEqual(request.url?.absoluteString, "https://mac.example.ts.net/api/v1/projects/p1/preview/3000/api/login")
        XCTAssertFalse(request.url!.absoluteString.contains("token"))
        XCTAssertEqual(request.httpMethod, "POST")
        XCTAssertEqual(request.httpBody, Data("{}".utf8))
        XCTAssertEqual(request.value(forHTTPHeaderField: "Cookie"), "__Host-wonder_session=token")
        XCTAssertEqual(request.value(forHTTPHeaderField: "x-wonder-csrf"), "binding")
        XCTAssertEqual(request.value(forHTTPHeaderField: "Origin"), "https://mac.example.ts.net")
        XCTAssertEqual(request.value(forHTTPHeaderField: "User-Agent"), "Desktop UA")
        XCTAssertEqual(request.value(forHTTPHeaderField: "Content-Type"), "application/json")
        XCTAssertEqual(request.value(forHTTPHeaderField: DevServerPreview.cookieHeader), "theme=dark")
        XCTAssertEqual(request.value(forHTTPHeaderField: DevServerPreview.conversationHeader), "c1")
        XCTAssertNil(request.value(forHTTPHeaderField: "Referer"))
        XCTAssertNil(request.value(forHTTPHeaderField: "Authorization"))
        XCTAssertFalse(request.httpShouldHandleCookies)

        let socket = try XCTUnwrap(DevServerPreview.socketRequest(page: page.url!, path: "/?token=abc", protocols: ["vite-hmr", "a,b"],
                                                                   projectId: "p1", conversationId: "c1",
                                                                   connection: connection, cookies: nil))
        XCTAssertEqual(socket.value(forHTTPHeaderField: DevServerPreview.conversationHeader), "c1")
        XCTAssertEqual(socket.url?.absoluteString, "wss://mac.example.ts.net/api/v1/projects/p1/preview/3000/?token=abc")
        XCTAssertEqual(socket.value(forHTTPHeaderField: "Sec-WebSocket-Protocol"), "vite-hmr")
        XCTAssertNil(DevServerPreview.socketRequest(page: page.url!, path: "/a/%2e%2e/b", protocols: [], projectId: "p1",
                                                    connection: connection, cookies: nil))
    }

    func testPagesSeeTheDevServersHeadersAndRedirectAdvice() throws {
        let url = URL(string: "https://mac.example.ts.net/x")!
        let response = try XCTUnwrap(HTTPURLResponse(url: url, statusCode: 302, httpVersion: nil, headerFields: [
            "Content-Type": "text/html", "Content-Security-Policy": "default-src 'self'; connect-src 'self'",
            "X-Frame-Options": "DENY", DevServerPreview.securityPolicyHeader: "script-src 'self'",
            DevServerPreview.upstreamHeader: "1", DevServerPreview.locationHeader: "/login",
            DevServerPreview.setCookieHeader: "a=1",
        ]))
        XCTAssertTrue(DevServerPreview.isFromDevServer(response))
        XCTAssertEqual(DevServerPreview.pageHeaders(from: response),
                       ["content-type": "text/html", "content-security-policy": "script-src 'self'"])
        XCTAssertEqual(DevServerPreview.redirect(from: response), .path("/login"))
        let away = try XCTUnwrap(HTTPURLResponse(url: url, statusCode: 502, httpVersion: nil, headerFields: [
            DevServerPreview.externalLocationHeader: "https://accounts.example.com/",
        ]))
        XCTAssertFalse(DevServerPreview.isFromDevServer(away))
        XCTAssertEqual(DevServerPreview.redirect(from: away), .external(URL(string: "https://accounts.example.com/")!))
        let protocolRelative = try XCTUnwrap(HTTPURLResponse(url: url, statusCode: 302, httpVersion: nil, headerFields: [
            DevServerPreview.locationHeader: "//evil.example/",
        ]))
        XCTAssertNil(DevServerPreview.redirect(from: protocolRelative))
        let document = String(decoding: DevServerPreview.redirectDocument(to: "/a\"</script>"), as: UTF8.self)
        // The target cannot close the script early: only the document's own tag ends it.
        XCTAssertEqual(document.components(separatedBy: "</script>").count, 2)
        XCTAssertTrue(document.hasSuffix("</script>"))
    }

    func testSafariIsOfferedOnlyForPublicPages() {
        XCTAssertTrue(DevServerPreview.isPublicWebURL(URL(string: "https://example.com/docs")))
        XCTAssertTrue(DevServerPreview.isPublicWebURL(URL(string: "http://93.184.216.34/")))
        for local in ["wonder-preview://port-5173/", "http://localhost:5173/", "http://127.0.0.1:3000/", "http://192.168.1.4/",
                      "http://10.0.0.2/", "http://172.20.1.1/", "http://100.101.102.103/", "https://mac.example-tailnet.ts.net/",
                      "http://devbox/", "http://printer.local/", "http://[::1]:3000/", "http://[fd00::1]/"] {
            XCTAssertFalse(DevServerPreview.isPublicWebURL(URL(string: local)), local)
        }
        XCTAssertFalse(DevServerPreview.isPublicWebURL(nil))
    }

    func testCookieJarKeepsEachPortsCookiesAndHonoursRemoval() {
        var jar = PreviewCookieJar()
        jar.store("session=abc; Path=/; HttpOnly, theme=dark; Expires=Wed, 21 Oct 2026 07:28:00 GMT", port: 3000)
        XCTAssertEqual(jar.header(port: 3000), "session=abc; theme=dark")
        XCTAssertNil(jar.header(port: 5173))
        jar.store("session=; Max-Age=0", port: 3000)
        XCTAssertEqual(jar.header(port: 3000), "theme=dark")
    }
}
