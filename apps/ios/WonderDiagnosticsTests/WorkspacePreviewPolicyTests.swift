import WebKit
import Network
import ImageIO
import PencilKit
import SwiftUI
import UIKit
import UniformTypeIdentifiers
import WonderComputerView
import WonderPairing
import XCTest
@testable import Wonder

/// Observable contract: the attached PNG contains the white page, an aspect-fit
/// image under the ink and exactly the chosen crop, regardless of sheet size.
final class SketchExportTests: XCTestCase {
    private func background() -> Data {
        let format = UIGraphicsImageRendererFormat(); format.scale = 1
        return UIGraphicsImageRenderer(size: CGSize(width: 100, height: 100), format: format).pngData { context in
            UIColor.red.setFill(); context.fill(CGRect(x: 0, y: 0, width: 100, height: 100))
        }
    }

    private func ink(color: UIColor = .blue) -> Data {
        let points = [CGPoint(x: 256, y: 384), CGPoint(x: 768, y: 384)].enumerated().map { index, point in
            PKStrokePoint(location: point, timeOffset: Double(index) * 0.2, size: CGSize(width: 24, height: 24),
                          opacity: 1, force: 1, azimuth: 0, altitude: .pi / 2)
        }
        return PKDrawing(strokes: [PKStroke(ink: PKInk(.pen, color: color),
            path: PKStrokePath(controlPoints: points, creationDate: Date(timeIntervalSince1970: 0)))]).dataRepresentation()
    }

    func testExportRejectsInvalidCropDrawingAndBackgroundRatherThanFabricatingAnImage() {
        let drawing = PKDrawing().dataRepresentation()
        for crop in [CGRect(x: -0.1, y: 0, width: 0.5, height: 0.5),
                     CGRect(x: 0, y: 0, width: 2, height: 1), .zero,
                     CGRect(x: 0, y: 0, width: CGFloat.infinity, height: 1)] {
            XCTAssertThrowsError(try SketchExport.render(drawing: drawing, background: nil, crop: crop))
        }
        XCTAssertThrowsError(try SketchExport.render(drawing: Data([0]), background: nil, crop: SketchExport.wholePage))
        XCTAssertThrowsError(try SketchExport.render(drawing: drawing, background: Data([0]), crop: SketchExport.wholePage))
        XCTAssertThrowsError(try SketchExport.prepareBackground(Data([0])))
    }

    func testBackgroundNormalizesEXIFOrientationAndBoundsDecode() throws {
        let format = UIGraphicsImageRendererFormat(); format.scale = 1
        let original = try XCTUnwrap(UIGraphicsImageRenderer(size: CGSize(width: 60, height: 40), format: format).image { context in
            UIColor.red.setFill(); context.fill(CGRect(x: 0, y: 0, width: 60, height: 40))
        }.cgImage)
        let output = NSMutableData()
        let destination = try XCTUnwrap(CGImageDestinationCreateWithData(output, UTType.jpeg.identifier as CFString, 1, nil))
        CGImageDestinationAddImage(destination, original, [kCGImagePropertyOrientation: 6] as CFDictionary)
        XCTAssertTrue(CGImageDestinationFinalize(destination))
        let normalized = try SketchExport.prepareBackground(output as Data)
        let source = try XCTUnwrap(CGImageSourceCreateWithData(normalized as CFData, nil))
        let properties = try XCTUnwrap(CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any])
        XCTAssertEqual((properties[kCGImagePropertyOrientation] as? NSNumber)?.intValue ?? 1, 1)
        XCTAssertEqual((properties[kCGImagePropertyPixelWidth] as? NSNumber)?.intValue, 40)
        XCTAssertEqual((properties[kCGImagePropertyPixelHeight] as? NSNumber)?.intValue, 60)
    }

}

/// Observable contract: the companion pane divider paints only its 16 pt strip,
/// and the swap button's circle stays a small disc in its middle at any text
/// size. A ZStack sized to the button's 44 pt target painted the strip 14 pt over
/// each tool, cutting Terminal's key row (portrait) and first columns (landscape).
final class CompanionDividerPaintTests: XCTestCase {
    private struct Probe {
        let layout: CompanionPaneLayout
        let size: CGSize
        var sideBySide: Bool { layout.direction == .sideBySide }
        /// A point `across` from the strip's centre line, at `along` on it.
        func point(across: CGFloat, along: CGFloat) -> CGPoint {
            let strip = layout.divider
            return sideBySide ? CGPoint(x: strip.midX + across, y: along) : CGPoint(x: along, y: strip.midY + across)
        }
        var middle: CGFloat { sideBySide ? layout.divider.midY : layout.divider.midX }
    }

    private let probes = [
        // Portrait iPad: the tool above the chat.
        Probe(layout: CompanionPaneLayout.resolve(size: CGSize(width: 400, height: 600)), size: CGSize(width: 400, height: 600)),
        // Landscape: side by side, and swapped.
        Probe(layout: CompanionPaneLayout.resolve(size: CGSize(width: 1000, height: 600)), size: CGSize(width: 1000, height: 600)),
        Probe(layout: CompanionPaneLayout.resolve(size: CGSize(width: 1000, height: 600), flipped: true), size: CGSize(width: 1000, height: 600)),
    ]

    @MainActor private func render(_ probe: Probe, typeSize: DynamicTypeSize) throws -> (CGImage, [UInt8]) {
        let strip = probe.layout.divider
        let view = ZStack {
            Color(red: 1, green: 0, blue: 0)
            CompanionDivider(layout: probe.layout, size: probe.size, origin: .zero, flipped: false,
                             dragFraction: .constant(nil), commit: { _ in }, step: { _ in }, flip: {}, expand: {})
                .frame(width: strip.width, height: strip.height)
                .position(x: strip.midX, y: strip.midY)
        }
        .frame(width: probe.size.width, height: probe.size.height)
        .environment(\.dynamicTypeSize, typeSize)
        let renderer = ImageRenderer(content: view)
        renderer.scale = 1
        let image = try XCTUnwrap(renderer.cgImage)
        var bytes = [UInt8](repeating: 0, count: image.width * image.height * 4)
        try bytes.withUnsafeMutableBytes { buffer in
            let context = try XCTUnwrap(CGContext(data: buffer.baseAddress, width: image.width, height: image.height,
                bitsPerComponent: 8, bytesPerRow: image.width * 4, space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue | CGBitmapInfo.byteOrder32Big.rawValue))
            context.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
        }
        return (image, bytes)
    }

}

/// Observable contract: a reply's link to the computer's own address
/// (`http://localhost:5173/settings`) opens this chat's browser at that page
/// when the chat serves the port, and otherwise says where the page runs;
/// every other link keeps the app's opener. Regression: such links went to the
/// phone's own localhost and failed.
@MainActor final class BrowserPreviewLinkRoutingTests: XCTestCase {
    private let servers = [DevServer(port: 5173, processName: "node", title: "Recipes"),
                           DevServer(port: 4000, processName: "ruby")]

    func testLoopbackLinksKeepTheirPortPathQueryAndFragment() throws {
        let cases: [(String, Int, String)] = [
            ("http://localhost:5173/settings", 5173, "/settings"),
            ("https://LOCALHOST:5173", 5173, "/"),
            ("http://127.0.0.1:4000/docs/a%20b?tab=api#install", 4000, "/docs/a%20b?tab=api#install"),
            ("http://[::1]:8080/#/settings", 8080, "/#/settings"),
            ("http://localhost/", 80, "/"),
        ]
        for (text, port, path) in cases {
            let link = try XCTUnwrap(LoopbackLink(try XCTUnwrap(URL(string: text))), text)
            XCTAssertEqual(link.port, port, text)
            XCTAssertEqual(link.path, path, text)
            var opened: [LoopbackLink] = []
            var forwarded: [URL] = []
            LoopbackLinkRouting.route(URL(string: text)!, open: { opened.append($0) }, openWebLink: { forwarded.append($0) })
            XCTAssertEqual(opened, [link], text)
            XCTAssertTrue(forwarded.isEmpty, "A computer link must never go to the phone’s localhost")
            XCTAssertEqual(DevServerPreview.url(port: link.port, path: link.path)?.absoluteString,
                           "wonder-preview://port-\(port)\(path)", text)
        }
    }

    func testOtherLinksAreNotTheComputersPages() throws {
        for text in ["https://example.com/", "http://192.168.1.4:5173/", "http://my-mac.local:5173/",
                     "http://user@localhost:5173/", "mailto:someone@example.com", "http://localhost.example.com:5173/",
                     "ftp://localhost:21/", "/relative/path"] {
            let url = try XCTUnwrap(URL(string: text))
            XCTAssertNil(LoopbackLink(url), text)
            var opened: [LoopbackLink] = []
            var forwarded: [URL] = []
            LoopbackLinkRouting.route(url, open: { opened.append($0) }, openWebLink: { forwarded.append($0) })
            XCTAssertTrue(opened.isEmpty, text)
            XCTAssertEqual(forwarded, [url], "Other links keep the Settings opener")
        }
    }

    func testOnlyThisChatsServersOpenInTheBrowser() throws {
        let served = try XCTUnwrap(LoopbackLink(try XCTUnwrap(URL(string: "http://localhost:5173/settings"))))
        let elsewhere = try XCTUnwrap(LoopbackLink(try XCTUnwrap(URL(string: "http://localhost:3000/"))))
        XCTAssertTrue(served.isServed(by: servers))
        XCTAssertFalse(elsewhere.isServed(by: servers))
        XCTAssertFalse(served.isServed(by: []))
        XCTAssertEqual(LoopbackLink.notServedMessage(computer: "Studio Mac", previewAvailable: true),
                       "This page runs on Studio Mac. Start it there, then tap Preview.")
        XCTAssertEqual(LoopbackLink.notServedMessage(computer: "Studio Mac", previewAvailable: false),
                       "This page runs on Studio Mac. Open it there.")
    }

}

/// A committed navigation starts a new document whose WebSocket ids begin at 1
/// again, so the last page's sockets must be gone before the new page opens its own.
@MainActor final class PreviewSocketNavigationTests: XCTestCase {
    /// Keeps the real script message a page posts, so the bridge sees a genuine frame.
    private final class Capture: NSObject, WKScriptMessageHandler {
        var message: WKScriptMessage?
        let posted = XCTestExpectation(description: "page posted a socket open")
        func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
            self.message = message
            posted.fulfill()
        }
    }

    /// Serves the page that posts the socket open, as the preview scheme does for a real page.
    private final class PageServer: NSObject, WKURLSchemeHandler {
        func webView(_ webView: WKWebView, start task: any WKURLSchemeTask) {
            let html = "<script>window.webkit.messageHandlers.\(PreviewSocketBridge.messageName).postMessage({type: 'open', id: 1, path: '/ws', protocols: []})</script>"
            let response = URLResponse(url: task.request.url!, mimeType: "text/html", expectedContentLength: -1, textEncodingName: "utf-8")
            task.didReceive(response); task.didReceive(Data(html.utf8)); task.didFinish()
        }
        func webView(_ webView: WKWebView, stop task: any WKURLSchemeTask) {}
    }

    private func browser() throws -> BrowserPreviewModel {
        let credential = try JSONDecoder().decode(Credential.self, from: Data(#"{"sessionToken":"s","deviceId":"phone","csrfToken":"c","hostInstallationId":"studio"}"#.utf8))
        // Port 9 refuses at once; the socket still counts as open until its failure is delivered.
        let saved = try SavedConnection(routes: [ConnectionRoute(kind: .home, origin: "https://127.0.0.1:9")],
                                        credential: credential, hostKeyPin: "pin")
        return BrowserPreviewModel(endpoint: .live(projectId: "p", conversationId: "c", connection: saved))
    }

    func testACommittedNavigationEndsThePreviousPagesSockets() async throws {
        let preview = try browser()
        let capture = Capture()
        let configuration = WKWebViewConfiguration()
        configuration.userContentController.add(capture, contentWorld: .page, name: PreviewSocketBridge.messageName)
        configuration.setURLSchemeHandler(PageServer(), forURLScheme: DevServerPreview.scheme)
        let page = WKWebView(frame: .zero, configuration: configuration)
        preview.sockets.webView = page
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let window = UIWindow(windowScene: scene)
        let host = UIViewController(); window.rootViewController = host
        page.frame = CGRect(x: 0, y: 0, width: 360, height: 600)
        host.view.addSubview(page); window.makeKeyAndVisible()
        defer { preview.close(); window.isHidden = true }
        page.load(URLRequest(url: try XCTUnwrap(DevServerPreview.url(port: 5173))))
        await fulfillment(of: [capture.posted], timeout: 10)
        let open = try XCTUnwrap(capture.message)
        preview.sockets.handle(open)
        XCTAssertEqual(preview.openPageSocketCount, 1)
        preview.webView(preview.webView, didCommit: nil)
        XCTAssertEqual(preview.openPageSocketCount, 0, "The last page's sockets outlived its navigation")
        preview.sockets.handle(open)
        XCTAssertEqual(preview.openPageSocketCount, 1, "The new page's first socket reused an id the old page still held and was ignored")
    }
}

/// Observable contract: in the companion pane on the Duo, Files spends one
/// header row on its location, Back inside a folder, the folder picker and its
/// actions; at accessibility text sizes the actions move into ⋯ instead of
/// being cut short. The second row of title and actions is gone (B-265).
final class WorkspacePaneHeaderTests: XCTestCase {
    private func header(modified: Bool = false, path: String = "", rootLabel: String? = "Workspace",
                        roots: Int = 1, directory: Bool = true, large: Bool = false) -> WorkspacePaneHeader {
        WorkspacePaneHeader(modified: modified, directoryPath: path, rootLabel: rootLabel, rootCount: roots,
                            rootIsDirectory: directory, accessibilitySize: large)
    }

}

/// Contract: the actual published-page browser preserves HTML/CSS/inline JS,
/// receives live host tokens, fits changing content, and remains offline.
/// Regression: native conversion dropped grid/canvas/form structure; the old
/// offscreen runner read no theme variables and froze delayed DOM content.
@MainActor final class HTMLArtifactWebViewTests: XCTestCase {
    private func load(_ html: String, height: @escaping @MainActor (CGFloat) -> Void = { _ in }) async throws
        -> (WKWebView, HTMLArtifactWebView.Coordinator, UIWindow) {
        let loaded = expectation(description: "published HTML loaded")
        let value = HTMLArtifactWebView(html: html,
            theme: HTMLArtifactTheme(theme: .resolve(id: "cupola-light", systemIsDark: false)),
            onHeight: height, onLoad: { loaded.fulfill() }, onFailure: { XCTFail("HTML failed to load") })
        let coordinator = value.makeCoordinator()
        let view = value.makeView(coordinator: coordinator)
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let window = UIWindow(windowScene: scene)
        let controller = UIViewController(); window.rootViewController = controller
        view.frame = CGRect(x: 0, y: 0, width: 360, height: 600)
        controller.view.addSubview(view); window.makeKeyAndVisible()
        await fulfillment(of: [loaded], timeout: 15)
        return (view, coordinator, window)
    }
    private func close(_ view: WKWebView, _ coordinator: HTMLArtifactWebView.Coordinator, _ window: UIWindow) {
        HTMLArtifactWebView.dismantleUIView(view, coordinator: coordinator)
        view.removeFromSuperview(); window.isHidden = true
    }

    func testOfflinePolicyBlocksLoadsEvalNavigationAndNativeBridge() async throws {
        let listener = try NWListener(using: .tcp, on: .any)
        let ready = expectation(description: "local tripwire listening")
        let request = expectation(description: "no network request reaches the tripwire"); request.isInverted = true
        listener.stateUpdateHandler = { if case .ready = $0 { ready.fulfill() } }
        listener.newConnectionHandler = { connection in request.fulfill(); connection.cancel() }
        listener.start(queue: DispatchQueue(label: "genui-network-tripwire"))
        defer { listener.cancel() }
        await fulfillment(of: [ready], timeout: 10)
        let url = "http://127.0.0.1:\(try XCTUnwrap(listener.port).rawValue)/tripwire"
        let (view, coordinator, window) = try await load("""
        <script>window.violations=[];document.addEventListener('securitypolicyviolation',e=>violations.push(e.effectiveDirective));window.fetchBlocked=false;fetch('\(url)').catch(()=>window.fetchBlocked=true);try{eval('window.evalRan=true')}catch(e){};window.bridge=!!window.webkit?.messageHandlers?.cupolaPageSize;</script>
        <script src="\(url)"></script><img src="\(url)"><iframe src="\(url)"></iframe>
        <link rel="stylesheet" href="\(url)"><form action="\(url)"><button>Submit</button></form>
        """)
        defer { close(view, coordinator, window) }
        _ = try await view.evaluateJavaScript("document.querySelector('form').submit();window.open('\(url)');")
        await fulfillment(of: [request], timeout: 10)
        let evaluatedState = try await view.evaluateJavaScript("({blocked:fetchBlocked,evalRan:!!window.evalRan,bridge:window.bridge,violations:window.violations,url:location.href})")
        let state = try XCTUnwrap(evaluatedState as? [String: Any])
        XCTAssertEqual(state["blocked"] as? Bool, true); XCTAssertEqual(state["evalRan"] as? Bool, false)
        XCTAssertEqual(state["bridge"] as? Bool, false); XCTAssertEqual(state["url"] as? String, "about:blank")
        let violations = state["violations"] as? [String] ?? []
        XCTAssertTrue(violations.contains("connect-src")); XCTAssertTrue(violations.contains("script-src-elem") || violations.contains("script-src"))
        XCTAssertTrue(violations.contains("form-action"))
    }

}
