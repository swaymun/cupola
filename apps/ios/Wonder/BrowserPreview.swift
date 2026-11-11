import SwiftUI
import UIKit
import WebKit
import WonderPairing

/// Where preview pages come from: the paired Mac's proxy for one Project
/// thread's folder, or (Diagnostics only) a fixture page served by the same
/// scheme handler.
enum PreviewEndpoint {
    case live(projectId: String, conversationId: String, connection: SavedConnection)
    case fixture
}

/// The composer globe lists pages served by this Project folder on the computer.
struct DevServerMenu: View {
    let servers: [DevServer]
    let refresh: () async -> Void
    let open: (DevServer) -> Void

    static func title(_ server: DevServer) -> String {
        server.title == nil ? server.name : "\(server.name) · \(String(server.port))"
    }

    var body: some View {
        Menu {
            if servers.isEmpty { Text("No pages running") }
            ForEach(servers) { server in
                Button(Self.title(server)) { open(server) }
                    .accessibilityIdentifier("preview-page-\(server.port)")
            }
            Divider()
            Button { Task { await refresh() } } label: {
                Label("Check for pages", systemImage: "arrow.triangle.2.circlepath")
            }
            .accessibilityIdentifier("preview-check-pages")
        } label: {
            Label("Preview", systemImage: "globe")
                .labelStyle(.iconOnly)
                .modifier(ComposerStatusPill(isActive: false))
        }
        .accessibilityIdentifier("preview-pages-menu")
        .accessibilityLabel(servers.count == 1 ? "Preview, 1 page running" : "Preview, \(servers.count) pages running")
        .accessibilityHint("Choose a page to open full screen")
    }
}

// MARK: - Loading pages through the Mac

/// Serves `wonder-preview://port-N/...` by sending each request through the
/// paired Mac's authenticated proxy and streaming the answer into WebKit.
/// Root-relative URLs resolve on that origin; the session stays in this
/// handler's headers, out of the page's reach and out of every URL.
@MainActor final class PreviewSchemeHandler: NSObject, WKURLSchemeHandler {
    private enum Mode { case waiting, forwarding, collecting(Int) }
    private final class Load {
        let task: any WKURLSchemeTask
        let isMainFrame: Bool
        var redirects = 0
        var dataTask: URLSessionDataTask?
        var mode = Mode.waiting
        var errorBody = Data()
        init(task: any WKURLSchemeTask, isMainFrame: Bool) { self.task = task; self.isMainFrame = isMainFrame }
    }

    private let endpoint: PreviewEndpoint
    private var loads: [ObjectIdentifier: Load] = [:]
    private var byTask: [Int: ObjectIdentifier] = [:]
    private let delegate = PreviewSessionDelegate()
    private var session: URLSession?
    private(set) var cookies = PreviewCookieJar()
    /// The page the main frame is navigating to, so its failure is explained natively.
    var mainFrameURL: URL?
    /// The main frame's failure in plain words, and whether the Mac said the
    /// page's server is no longer running for this chat (404).
    var onPageFailure: ((String, Bool) -> Void)?

    init(endpoint: PreviewEndpoint) {
        self.endpoint = endpoint
        super.init()
        delegate.owner = self
        if case .live(_, _, let connection) = endpoint { delegate.trust.update(connection) }
    }

    private func urlSession() -> URLSession {
        if let session { return session }
        let configuration = URLSessionConfiguration.ephemeral
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.httpCookieAcceptPolicy = .never
        configuration.httpShouldSetCookies = false
        configuration.timeoutIntervalForRequest = 60
        configuration.httpMaximumConnectionsPerHost = 6
        let session = URLSession(configuration: configuration, delegate: delegate, delegateQueue: .main)
        self.session = session
        return session
    }

    /// Ends every load and releases the session; the handler serves nothing after.
    func close() {
        for load in loads.values { load.dataTask?.cancel() }
        loads = [:]
        byTask = [:]
        session?.invalidateAndCancel()
        session = nil
        onPageFailure = nil
    }

    func webView(_ webView: WKWebView, start urlSchemeTask: any WKURLSchemeTask) {
        let url = urlSchemeTask.request.url
        let isMain = url != nil && (url == mainFrameURL || urlSchemeTask.request.mainDocumentURL == url)
        let id = ObjectIdentifier(urlSchemeTask)
        let load = Load(task: urlSchemeTask, isMainFrame: isMain)
        loads[id] = load
        start(load, id: id, request: urlSchemeTask.request)
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: any WKURLSchemeTask) {
        guard let load = loads.removeValue(forKey: ObjectIdentifier(urlSchemeTask)) else { return }
        if let dataTask = load.dataTask {
            byTask[dataTask.taskIdentifier] = nil
            dataTask.cancel()
        }
    }

    private func start(_ load: Load, id: ObjectIdentifier, request page: URLRequest) {
        guard let url = page.url, let port = DevServerPreview.port(of: url) else {
            fail(id, message: "This page address is not allowed.")
            return
        }
        switch endpoint {
        case .fixture:
            #if WONDER_DIAGNOSTICS
            let reply = DiagnosticDevServerFixture.respond(to: page)
            guard let response = HTTPURLResponse(url: url, statusCode: reply.status, httpVersion: "HTTP/1.1",
                                                 headerFields: reply.headers) else { fail(id, message: nil); return }
            if reply.status >= 400 && load.isMainFrame {
                onPageFailure?(String(decoding: reply.body, as: UTF8.self), reply.status == 404)
                fail(id, message: nil)
                return
            }
            load.task.didReceive(response)
            load.task.didReceive(reply.body)
            load.task.didFinish()
            loads[id] = nil
            #else
            fail(id, message: nil)
            #endif
        case .live(let projectId, let conversationId, let connection):
            guard let request = DevServerPreview.request(for: page, projectId: projectId, conversationId: conversationId,
                                                         connection: connection, cookies: cookies.header(port: port)) else {
                fail(id, message: "This page address is not allowed.")
                return
            }
            let dataTask = urlSession().dataTask(with: request)
            load.dataTask = dataTask
            load.mode = .waiting
            load.errorBody = Data()
            byTask[dataTask.taskIdentifier] = id
            dataTask.resume()
        }
    }

    fileprivate func received(_ response: URLResponse, task taskID: Int) -> URLSession.ResponseDisposition {
        guard let id = byTask[taskID], let load = loads[id], let http = response as? HTTPURLResponse,
              let url = load.task.request.url, let port = DevServerPreview.port(of: url) else { return .cancel }
        if let redirect = DevServerPreview.redirect(from: http) {
            byTask[taskID] = nil
            switch redirect {
            case .path(let path):
                if load.isMainFrame {
                    respond(id, document: DevServerPreview.redirectDocument(to: path), url: url)
                } else if load.redirects < 5, let next = DevServerPreview.url(port: port, path: path) {
                    load.redirects += 1
                    var request = load.task.request
                    request.url = next
                    if http.statusCode == 303 { request.httpMethod = "GET"; request.httpBody = nil }
                    start(load, id: id, request: request)
                } else {
                    fail(id, message: nil)
                }
            case .external(let target):
                if load.isMainFrame {
                    respond(id, document: DevServerPreview.redirectDocument(to: target.absoluteString), url: url)
                } else {
                    fail(id, message: nil)
                }
            }
            return .cancel
        }
        guard DevServerPreview.isFromDevServer(http) else {
            load.mode = .collecting(http.statusCode)
            return .allow
        }
        if let set = http.value(forHTTPHeaderField: DevServerPreview.setCookieHeader) { cookies.store(set, port: port) }
        guard let page = HTTPURLResponse(url: url, statusCode: http.statusCode, httpVersion: "HTTP/1.1",
                                         headerFields: DevServerPreview.pageHeaders(from: http)) else { return .cancel }
        load.mode = .forwarding
        load.task.didReceive(page)
        return .allow
    }

    fileprivate func received(_ data: Data, task taskID: Int) {
        guard let id = byTask[taskID], let load = loads[id] else { return }
        switch load.mode {
        case .forwarding: load.task.didReceive(data)
        case .collecting: if load.errorBody.count < 2048 { load.errorBody.append(data.prefix(2048 - load.errorBody.count)) }
        case .waiting: break
        }
    }

    fileprivate func completed(task taskID: Int, error: Error?) {
        guard let id = byTask.removeValue(forKey: taskID), let load = loads[id] else { return }
        switch load.mode {
        case .forwarding:
            loads[id] = nil
            if let error { load.task.didFailWithError(error) } else { load.task.didFinish() }
        case .collecting(let status):
            let message = Self.failureMessage(status: status, body: load.errorBody)
            if load.isMainFrame { onPageFailure?(message, status == 404) }
            fail(id, message: nil)
        case .waiting:
            if load.isMainFrame, let error { onPageFailure?(Self.failureMessage(error), false) }
            fail(id, message: nil)
        }
    }

    private func respond(_ id: ObjectIdentifier, document: Data, url: URL) {
        guard let load = loads.removeValue(forKey: id),
              let response = HTTPURLResponse(url: url, statusCode: 200, httpVersion: "HTTP/1.1",
                                             headerFields: ["content-type": "text/html; charset=utf-8"]) else { return }
        load.task.didReceive(response)
        load.task.didReceive(document)
        load.task.didFinish()
    }

    private func fail(_ id: ObjectIdentifier, message: String?) {
        guard let load = loads.removeValue(forKey: id) else { return }
        if let message, load.isMainFrame { onPageFailure?(message, false) }
        load.task.didFailWithError(URLError(.resourceUnavailable))
    }

    /// Wonder's own refusals, in plain words. The Mac words most of them.
    static func failureMessage(status: Int, body: Data) -> String {
        if status == 401 { return "Cupola needs to reconnect to your computer. Close the browser, then open it again." }
        let text = String(decoding: body, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        if !text.isEmpty, text.count <= 240, !text.contains("<") { return text }
        return "The page could not load from your computer."
    }

    static func failureMessage(_ error: Error) -> String {
        if let error = error as? URLError,
           [.notConnectedToInternet, .networkConnectionLost, .cannotConnectToHost, .cannotFindHost, .timedOut].contains(error.code) {
            return "Your computer can’t be reached right now. Check its connection."
        }
        return "The page could not load from your computer."
    }
}

/// Session callbacks arrive on the main queue (`delegateQueue: .main`).
private final class PreviewSessionDelegate: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    let trust = HomeNetworkTrust()
    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping @Sendable (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        trust.urlSession(session, didReceive: challenge, completionHandler: completionHandler)
    }

    weak var owner: PreviewSchemeHandler?

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse,
                    completionHandler: @escaping @Sendable (URLSession.ResponseDisposition) -> Void) {
        let id = dataTask.taskIdentifier
        let disposition = MainActor.assumeIsolated { owner?.received(response, task: id) ?? .cancel }
        completionHandler(disposition)
    }
    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        let id = dataTask.taskIdentifier
        MainActor.assumeIsolated { owner?.received(data, task: id) }
    }
    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        let id = task.taskIdentifier
        MainActor.assumeIsolated { owner?.completed(task: id, error: error) }
    }
    // The Mac reports redirects in its own header; URLSession never follows one.
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping @Sendable (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}

// MARK: - Live reload sockets

/// Pages open WebSockets for hot reload (Vite, Next.js). A script replaces the
/// page's `WebSocket` for its own dev server's address and this bridge opens
/// the socket through the Mac's proxy with the session in headers.
@MainActor final class PreviewSocketBridge: NSObject {
    static let messageName = "wonderPreviewSocket"
    static let maxSockets = 8
    static let maxMessage = 16 * 1024 * 1024

    private struct Socket { let task: URLSessionWebSocketTask; let frame: WKFrameInfo }
    private let endpoint: PreviewEndpoint
    private let cookies: (Int) -> String?
    weak var webView: WKWebView?
    private var sockets: [Int: Socket] = [:]
    private var pageIDs: [Int: Int] = [:]
    private let delegate = PreviewSocketDelegate()
    private var session: URLSession?

    init(endpoint: PreviewEndpoint, cookies: @escaping (Int) -> String?) {
        self.endpoint = endpoint
        self.cookies = cookies
        super.init()
        delegate.owner = self
        if case .live(_, _, let connection) = endpoint { delegate.trust.update(connection) }
    }

    /// Page sockets held open now; a test observes the reset on navigation.
    var openPageSocketCount: Int { sockets.count }

    func close() {
        closePageSockets()
        session?.invalidateAndCancel()
        session = nil
    }

    /// A new document starts its socket ids at 1 again and its old sockets are
    /// gone with it, so a committed navigation must not leave the last page's
    /// sockets holding those ids (the new page's `open` would be ignored).
    func closePageSockets() {
        for socket in sockets.values { socket.task.cancel(with: .goingAway, reason: nil) }
        sockets = [:]
        pageIDs = [:]
    }

    func handle(_ message: WKScriptMessage) {
        guard let body = message.body as? [String: Any], let type = body["type"] as? String,
              let id = body["id"] as? Int, id > 0 else { return }
        switch type {
        case "open":
            let path = body["path"] as? String ?? "/"
            let protocols = (body["protocols"] as? [Any])?.compactMap { $0 as? String } ?? []
            open(id, path: path, protocols: protocols, frame: message.frameInfo)
        case "send":
            guard let socket = sockets[id] else { return }
            if let text = body["text"] as? String, text.utf8.count <= Self.maxMessage {
                socket.task.send(.string(text)) { _ in }
            } else if let encoded = body["binary"] as? String, encoded.count <= Self.maxMessage * 4 / 3 + 4,
                      let data = Data(base64Encoded: encoded) {
                socket.task.send(.data(data)) { _ in }
            }
        case "close":
            guard let socket = sockets.removeValue(forKey: id) else { return }
            pageIDs[socket.task.taskIdentifier] = nil
            socket.task.cancel(with: .normalClosure, reason: nil)
            deliver(id, frame: socket.frame, type: "close", detail: ["code": 1000])
        default:
            break
        }
    }

    private func open(_ id: Int, path: String, protocols: [String], frame: WKFrameInfo) {
        guard sockets[id] == nil else { return }
        guard case .live(let projectId, let conversationId, let connection) = endpoint, sockets.count < Self.maxSockets,
              let page = frame.request.url, let port = DevServerPreview.port(of: page),
              let request = DevServerPreview.socketRequest(page: page, path: path, protocols: protocols, projectId: projectId,
                                                           conversationId: conversationId, connection: connection,
                                                           cookies: cookies(port)) else {
            deliver(id, frame: frame, type: "close", detail: ["code": 1006])
            return
        }
        if session == nil {
            session = URLSession(configuration: .ephemeral, delegate: delegate, delegateQueue: .main)
        }
        guard let task = session?.webSocketTask(with: request) else { return }
        task.maximumMessageSize = Self.maxMessage
        sockets[id] = Socket(task: task, frame: frame)
        pageIDs[task.taskIdentifier] = id
        task.resume()
        receive(id, task)
    }

    private func receive(_ id: Int, _ task: URLSessionWebSocketTask) {
        task.receive { [weak self] result in
            Task { @MainActor in self?.received(id, task: task, result) }
        }
    }

    private func received(_ id: Int, task: URLSessionWebSocketTask, _ result: Result<URLSessionWebSocketTask.Message, Error>) {
        guard let socket = sockets[id], socket.task === task else { return }
        switch result {
        case .success(.string(let text)):
            deliver(id, frame: socket.frame, type: "message", detail: ["text": text])
            receive(id, task)
        case .success(.data(let data)):
            deliver(id, frame: socket.frame, type: "message", detail: ["binary": data.base64EncodedString()])
            receive(id, task)
        case .success:
            receive(id, task)
        case .failure:
            sockets[id] = nil
            pageIDs[task.taskIdentifier] = nil
            let code = task.closeCode == .invalid ? 1006 : task.closeCode.rawValue
            deliver(id, frame: socket.frame, type: "close", detail: ["code": code])
        }
    }

    fileprivate func opened(task taskID: Int, protocol chosen: String?) {
        guard let id = pageIDs[taskID], let socket = sockets[id] else { return }
        deliver(id, frame: socket.frame, type: "open", detail: ["protocol": chosen ?? ""])
    }

    private func deliver(_ id: Int, frame: WKFrameInfo, type: String, detail: [String: Any]) {
        guard let webView, let data = try? JSONSerialization.data(withJSONObject: detail),
              let json = String(data: data, encoding: .utf8) else { return }
        let script = "window.__wonderPreviewSocket && window.__wonderPreviewSocket(\(id), '\(type)', \(json))"
        webView.evaluateJavaScript(script, in: frame, in: .page) { _ in }
    }

    /// Replaces `WebSocket` for addresses on the page's own dev server.
    static let script = """
    (() => {
      if (location.protocol !== 'wonder-preview:' || window.__wonderPreviewSocket) return;
      const bridge = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.\(messageName);
      if (!bridge) return;
      const Native = window.WebSocket;
      const sockets = new Map();
      let nextID = 1;
      const local = new Set([location.hostname, 'localhost', '127.0.0.1', '[::1]', '0.0.0.0']);
      const toBase64 = (buffer) => { let text = ''; const bytes = new Uint8Array(buffer); for (let i = 0; i < bytes.length; i += 0x8000) text += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000)); return btoa(text); };
      const fromBase64 = (text) => { const raw = atob(text); const bytes = new Uint8Array(raw.length); for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i); return bytes.buffer; };
      class PreviewSocket extends EventTarget {
        constructor(url, protocols) {
          super();
          const target = new URL(String(url), location.href);
          if (!local.has(target.hostname)) return new Native(url, protocols);
          this.url = target.href; this.readyState = 0; this.protocol = ''; this.extensions = '';
          this.bufferedAmount = 0; this.binaryType = 'blob';
          this.onopen = null; this.onmessage = null; this.onerror = null; this.onclose = null;
          this._id = nextID++;
          sockets.set(this._id, this);
          const list = protocols === undefined ? [] : [].concat(protocols).map(String);
          bridge.postMessage({ type: 'open', id: this._id, path: (target.pathname || '/') + target.search, protocols: list });
        }
        send(data) {
          if (this.readyState === 0) throw new DOMException('Still connecting', 'InvalidStateError');
          if (this.readyState !== 1) return;
          const id = this._id;
          if (typeof data === 'string') bridge.postMessage({ type: 'send', id, text: data });
          else if (data instanceof Blob) data.arrayBuffer().then((buffer) => bridge.postMessage({ type: 'send', id, binary: toBase64(buffer) }));
          else if (data instanceof ArrayBuffer) bridge.postMessage({ type: 'send', id, binary: toBase64(data) });
          else if (ArrayBuffer.isView(data)) bridge.postMessage({ type: 'send', id, binary: toBase64(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength)) });
          else bridge.postMessage({ type: 'send', id, text: String(data) });
        }
        close() {
          if (this.readyState >= 2) return;
          this.readyState = 2;
          bridge.postMessage({ type: 'close', id: this._id });
        }
        _fire(event) {
          this.dispatchEvent(event);
          const handler = this['on' + event.type];
          if (typeof handler === 'function') handler.call(this, event);
        }
      }
      [['CONNECTING', 0], ['OPEN', 1], ['CLOSING', 2], ['CLOSED', 3]].forEach(([name, value]) => { PreviewSocket[name] = value; PreviewSocket.prototype[name] = value; });
      window.WebSocket = PreviewSocket;
      Object.defineProperty(window, '__wonderPreviewSocket', { value: (id, type, detail) => {
        const socket = sockets.get(id);
        if (!socket) return;
        if (type === 'open') {
          socket.readyState = 1; socket.protocol = detail.protocol || '';
          socket._fire(new Event('open'));
        } else if (type === 'message') {
          let data = detail.text;
          if (detail.binary !== undefined) { const buffer = fromBase64(detail.binary); data = socket.binaryType === 'arraybuffer' ? buffer : new Blob([buffer]); }
          socket._fire(new MessageEvent('message', { data }));
        } else if (type === 'close') {
          const clean = detail.code === 1000;
          if (!clean && socket.readyState !== 2) socket._fire(new Event('error'));
          socket.readyState = 3; sockets.delete(id);
          socket._fire(new CloseEvent('close', { code: detail.code || 1006, wasClean: clean }));
        }
      } });
    })();
    """
}

private final class PreviewSocketDelegate: NSObject, URLSessionWebSocketDelegate, @unchecked Sendable {
    let trust = HomeNetworkTrust()
    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping @Sendable (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        trust.urlSession(session, didReceive: challenge, completionHandler: completionHandler)
    }

    weak var owner: PreviewSocketBridge?
    func urlSession(_ session: URLSession, webSocketTask: URLSessionWebSocketTask, didOpenWithProtocol chosen: String?) {
        let id = webSocketTask.taskIdentifier
        MainActor.assumeIsolated { owner?.opened(task: id, protocol: chosen) }
    }
}

/// `WKUserContentController` keeps its handlers alive; this keeps the bridge weak.
private final class WeakScriptHandler: NSObject, WKScriptMessageHandler {
    weak var bridge: PreviewSocketBridge?
    init(_ bridge: PreviewSocketBridge) { self.bridge = bridge }
    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        bridge?.handle(message)
    }
}

// MARK: - The preview browser

@MainActor final class BrowserPreviewModel: NSObject, ObservableObject, WKNavigationDelegate, WKUIDelegate {
    enum Phase: Equatable { case loading, ready, failed(String) }

    @Published private(set) var phase = Phase.loading
    @Published private(set) var progress: Double = 0
    @Published private(set) var title = "Page"
    var openWebLink: ((URL) -> Void)? {
        didSet {
            if let openWebLink, let pendingWebLink {
                self.pendingWebLink = nil
                openWebLink(pendingWebLink)
            }
        }
    }
    private var pendingWebLink: URL?
    @Published private(set) var port: Int?
    @Published private(set) var viewport = PreviewViewport.mobile
    /// The Mac said the open page's server no longer runs for this chat.
    @Published private(set) var pageGone = false
    #if WONDER_DIAGNOSTICS
    /// The loaded page's title and layout width, for scenarios: the simulator's
    /// accessibility tree does not reach into web content.
    @Published private(set) var pageProbe: String?
    /// What only a loaded script and stylesheet produce: the fixture's status
    /// line (its script writes the layout width; the page counts its loads)
    /// and computed styles the stylesheet sets, so a page whose assets failed
    /// still has a title and a width but not these.
    @Published private(set) var pageProbeDetail: String?
    #endif

    let webView: WKWebView
    private let handler: PreviewSchemeHandler
    let sockets: PreviewSocketBridge
    private var observations: [NSKeyValueObservation] = []
    private var pageFailure: (message: String, gone: Bool)?
    private var closed = false

    init(endpoint: PreviewEndpoint) {
        let handler = PreviewSchemeHandler(endpoint: endpoint)
        let sockets = PreviewSocketBridge(endpoint: endpoint) { [weak handler] port in handler?.cookies.header(port: port) }
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.setURLSchemeHandler(handler, forURLScheme: DevServerPreview.scheme)
        configuration.allowsInlineMediaPlayback = true
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false
        configuration.userContentController.add(WeakScriptHandler(sockets), contentWorld: .page, name: PreviewSocketBridge.messageName)
        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.allowsBackForwardNavigationGestures = true
        webView.accessibilityIdentifier = "browser-preview-web"
        #if WONDER_DIAGNOSTICS
        webView.isInspectable = true
        #endif
        self.handler = handler
        self.sockets = sockets
        self.webView = webView
        super.init()
        sockets.webView = webView
        webView.navigationDelegate = self
        webView.uiDelegate = self
        handler.onPageFailure = { [weak self] message, gone in self?.pageFailure = (message, gone) }
        installScripts()
        observations = [
            webView.observe(\.title, options: [.new]) { [weak self] view, _ in
                MainActor.assumeIsolated { self?.title = view.title ?? "Page" }
            },
            webView.observe(\.estimatedProgress, options: [.new]) { [weak self] view, _ in
                MainActor.assumeIsolated { self?.progress = view.estimatedProgress }
            },
        ]
    }

    private func installScripts() {
        let controller = webView.configuration.userContentController
        controller.removeAllUserScripts()
        controller.addUserScript(WKUserScript(source: PreviewSocketBridge.script, injectionTime: .atDocumentStart,
                                              forMainFrameOnly: false, in: .page))
        if let viewportScript = viewport.viewportScript {
            controller.addUserScript(WKUserScript(source: viewportScript, injectionTime: .atDocumentStart,
                                                  forMainFrameOnly: true, in: .defaultClient))
        }
        webView.customUserAgent = viewport.userAgent
    }

    func open(port: Int, path: String = "/") {
        guard !closed, let url = DevServerPreview.url(port: port, path: path) else { return }
        self.port = port
        load(url)
    }

    private func load(_ url: URL) {
        pageFailure = nil
        if pageGone { pageGone = false }
        phase = .loading
        handler.mainFrameURL = url
        webView.load(URLRequest(url: url))
    }

    func setViewport(_ viewport: PreviewViewport) {
        guard viewport != self.viewport, !closed else { return }
        self.viewport = viewport
        installScripts()
        reload()
    }

    /// Reloads the page; after a failure, retries the page that failed.
    func reload() {
        guard !closed else { return }
        if case .failed = phase, let url = handler.mainFrameURL ?? port.flatMap({ DevServerPreview.url(port: $0) }) {
            load(url)
        } else if webView.url == nil, let url = port.flatMap({ DevServerPreview.url(port: $0) }) {
            load(url)
        } else {
            pageFailure = nil
            if pageGone { pageGone = false }
            phase = .loading
            webView.reload()
        }
    }

    /// The page as shown, as PNG, for the person to add to their message.
    func snapshot() async -> Data? {
        guard !closed, phase == .ready else { return nil }
        let configuration = WKSnapshotConfiguration()
        configuration.afterScreenUpdates = true
        let image = try? await webView.takeSnapshot(configuration: configuration)
        guard !closed, !Task.isCancelled else { return nil }
        return image?.pngData()
    }

    /// Stops loading and releases connections; the model shows nothing after.
    func close() {
        guard !closed else { return }
        closed = true
        observations = [
            webView.observe(\.title, options: [.new]) { [weak self] view, _ in
                MainActor.assumeIsolated { self?.title = view.title ?? "Page" }
            },]
        webView.stopLoading()
        webView.navigationDelegate = nil
        webView.uiDelegate = nil
        webView.configuration.userContentController.removeScriptMessageHandler(forName: PreviewSocketBridge.messageName, contentWorld: .page)
        webView.configuration.userContentController.removeAllUserScripts()
        handler.close()
        sockets.close()
        webView.loadHTMLString("", baseURL: nil)
    }

    // MARK: WKNavigationDelegate

    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, preferences: WKWebpagePreferences,
                 decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy, WKWebpagePreferences) -> Void) {
        // Always mobile content mode: WebKit's desktop mode (an iPad's default) ignores
        // viewport tags, so Desktop's width=1280 laid out at the browser's width instead.
        // Desktop comes from the Mac user agent and that viewport (PreviewViewport).
        preferences.preferredContentMode = .mobile
        guard let url = action.request.url, let scheme = url.scheme?.lowercased() else {
            decisionHandler(.cancel, preferences)
            return
        }
        if action.targetFrame?.isMainFrame != false, DevServerPreview.isPublicWebURL(url) {
            decisionHandler(.cancel, preferences)
            if let openWebLink { openWebLink(url) } else { pendingWebLink = url }
            return
        }
        switch scheme {
        case DevServerPreview.scheme, "http", "https", "about", "blob", "data":
            // A link that opens a new window loads here instead.
            if action.targetFrame == nil {
                decisionHandler(.cancel, preferences)
                if scheme == DevServerPreview.scheme || scheme == "http" || scheme == "https" { load(url) }
                return
            }
            if action.targetFrame?.isMainFrame == true, scheme == DevServerPreview.scheme {
                handler.mainFrameURL = url
                if let port = DevServerPreview.port(of: url), port != self.port { self.port = port }
            }
            decisionHandler(.allow, preferences)
        default:
            // Mail, phone and app links are not part of the preview.
            decisionHandler(.cancel, preferences)
        }
    }

    var openPageSocketCount: Int { sockets.openPageSocketCount }

    func webView(_ webView: WKWebView, didCommit navigation: WKNavigation!) {
        sockets.closePageSockets()
    }

    func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
        pageFailure = nil
        if phase != .loading { phase = .loading }
        #if WONDER_DIAGNOSTICS
        // A probe describes only the page that finished loading.
        pageProbe = nil; pageProbeDetail = nil
        #endif
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        phase = .ready
        #if WONDER_DIAGNOSTICS
        // The layout viewport, not the zoomed visible width (which rounds, e.g. 1279 in a short panel).
        webView.evaluateJavaScript("document.title + ' · ' + document.documentElement.clientWidth + ' px wide'") { [weak self] result, _ in
            MainActor.assumeIsolated { self?.pageProbe = result as? String }
        }
        webView.evaluateJavaScript(Self.pageProbeDetailScript) { [weak self] result, _ in
            MainActor.assumeIsolated { self?.pageProbeDetail = result as? String }
        }
        #endif
    }

    #if WONDER_DIAGNOSTICS
    private static let pageProbeDetailScript = """
    (() => {
      const text = id => { const node = document.getElementById(id); return node ? node.textContent : 'none'; };
      const style = (selector, property) => {
        const node = document.querySelector(selector);
        return node ? getComputedStyle(node).getPropertyValue(property) : 'none';
      };
      return 'script: ' + text('layout') + '; ' + text('served')
        + '; style: header ' + style('header', 'background-color') + ', filters ' + style('aside', 'display');
    })()
    """

    /// Follows the page's Saved link as a tap would, so Back has a page to return
    /// from (the simulator's accessibility tree does not reach into the page).
    func followSavedLink() {
        webView.evaluateJavaScript("document.getElementById('saved-link')?.click()")
    }
    func followWebLink() {
        webView.evaluateJavaScript("document.getElementById('web-link')?.click()")
    }
    #endif

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
        failed(error)
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        failed(error)
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        phase = .failed("The page stopped responding.")
    }

    private func failed(_ error: Error) {
        let error = error as NSError
        if error.domain == NSURLErrorDomain, error.code == NSURLErrorCancelled { return }
        // A page that replaced itself (the redirect document) is not a failure.
        if error.domain == "WebKitErrorDomain", error.code == 102 { return }
        let gone = pageFailure?.gone == true
        if pageGone != gone { pageGone = gone }
        phase = .failed(pageFailure?.message ?? PreviewSchemeHandler.failureMessage(error))
    }

    // MARK: WKUIDelegate

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if let url = action.request.url { load(url) }
        return nil
    }
}

/// Hosts the full-screen page's one web view across size changes.
private struct PreviewWebView: UIViewRepresentable {
    let webView: WKWebView
    func makeUIView(context: Context) -> UIView {
        let container = UIView()
        container.clipsToBounds = true
        attach(to: container)
        return container
    }
    func updateUIView(_ container: UIView, context: Context) {
        if webView.superview !== container { attach(to: container) }
    }
    private func attach(to container: UIView) {
        webView.removeFromSuperview()
        webView.frame = container.bounds
        webView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        container.addSubview(webView)
    }
}

/// A transient full-screen page. Closing releases the proxy, sockets and WebKit.
@MainActor final class BrowserPresentation: ObservableObject {
    @Published private(set) var browser: BrowserPreviewModel?
    func open(_ endpoint: PreviewEndpoint, at link: LoopbackLink) {
        if browser == nil { browser = BrowserPreviewModel(endpoint: endpoint) }
        browser?.open(port: link.port, path: link.path)
    }
    func close() {
        browser?.close()
        browser = nil
    }
}

struct FullScreenPreview: View {
    @ObservedObject var model: BrowserPreviewModel
    let close: () -> Void
    var stageScreenshot: ((Data) async -> BrowserScreenshotStaging)?
    @State private var notice: String?
    @State private var sharing: PreviewImage?
    @State private var capturing = false
    @State private var shareFailure = false
    @Environment(\.openURL) private var openURL

    var body: some View {
        NavigationStack {
            ZStack(alignment: .top) {
                PreviewWebView(webView: model.webView)
                if model.phase == .loading {
                    ProgressView(value: max(0.05, model.progress)).progressViewStyle(.linear)
                }
                if case .failed(let message) = model.phase {
                    ContentUnavailableView {
                        Label(model.pageGone ? "This page stopped responding" : "Page didn’t load", systemImage: "exclamationmark.triangle")
                    } description: { Text(message) } actions: {
                        Button("Try again") { model.reload() }.accessibilityIdentifier("browser-preview-retry")
                    }
                    .background(.background)
                    .accessibilityIdentifier("browser-preview-failure")
                }
                #if WONDER_DIAGNOSTICS
                if model.phase == .ready, let probe = model.pageProbe {
                    Color.clear.frame(width: 1, height: 1).accessibilityElement()
                        .accessibilityLabel(probe).accessibilityValue(model.pageProbeDetail ?? "")
                        .accessibilityIdentifier("browser-preview-page-probe")
                }
                #endif
            }
            .wonderPage()
            .navigationTitle(model.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Button("Reload", systemImage: "arrow.clockwise") { model.reload() }
                        .accessibilityIdentifier("browser-preview-reload")
                    Menu {
                        Button("Share screenshot", systemImage: "square.and.arrow.up") {
                            capturing = true
                            Task {
                                defer { capturing = false }
                                if let data = await model.snapshot(), let image = UIImage(data: data) {
                                    sharing = PreviewImage(image: image)
                                } else { shareFailure = true }
                            }
                        }
                        Button(model.viewport == .mobile ? "Request desktop site" : "Request mobile site", systemImage: "desktopcomputer") {
                            model.setViewport(model.viewport == .mobile ? .desktop : .mobile)
                        }
                        .accessibilityIdentifier("browser-preview-viewport")
                        if let stageScreenshot {
                            Button("Show the agent", systemImage: "camera.viewfinder") {
                                capturing = true
                                Task {
                                    defer { capturing = false }
                                    let result: BrowserScreenshotStaging
                                    if let data = await model.snapshot() { result = await stageScreenshot(data) }
                                    else { result = .notCaptured }
                                    notice = result.notice
                                    AccessibilityNotification.Announcement(result.notice).post()
                                }
                            }
                            .accessibilityIdentifier("browser-preview-show-agent")
                        }
                    } label: { Label("Share", systemImage: "square.and.arrow.up") }
                    .disabled(capturing || model.phase != .ready)
                    .accessibilityIdentifier("browser-share")
                    // The same ✕ as a link's viewer (`LinkViewer`): one way out of a full-screen page.
                    Button("Close", systemImage: "xmark", action: close)
                        .labelStyle(.iconOnly)
                        .accessibilityIdentifier("browser-done")
                }
            }
        }
        .accessibilityIdentifier("browser-full-screen")
        .accessibilityAction(.escape) { close() }
        .onAppear { model.openWebLink = { openURL($0) } }
        .sheet(item: $sharing) { PreviewShare(image: $0.image) }
        .overlay(alignment: .top) {
            if let notice {
                TemporaryNotice(key: notice) { Text(notice).padding().background(.regularMaterial, in: Capsule()).padding(.top, 60) }
            }
        }
        .alert("The page couldn’t be captured", isPresented: $shareFailure) {
            Button("OK", role: .cancel) {}
        } message: { Text("Wait for it to finish loading, then try again.") }
        #if WONDER_DIAGNOSTICS
        .background { DiagnosticDriverSurface(surface: .init(priority: 50, state: {
            ["browser": ["fullScreen": true, "port": model.port as Any,
                          "url": model.webView.url?.absoluteString ?? "", "title": model.title]]
        }, controls: {
            var controls = [DiagnosticTestDriver.Control(id: "browser-full-screen", label: "Page"),
                            .init(id: "browser-done", label: "Close", tap: close),
                            .init(id: "browser-preview-reload", label: "Reload", tap: { model.reload() })]
            if let probe = model.pageProbe {
                controls.append(.init(id: "browser-preview-page-probe", label: probe, value: model.pageProbeDetail ?? ""))
                controls.append(.init(id: "browser-preview-follow-link", label: "Follow the page's Saved link", tap: { model.followSavedLink() }))
                controls.append(.init(id: "browser-preview-follow-web-link", label: "Follow the page’s web link", tap: { model.followWebLink() }))
            }
            if case .failed(let message) = model.phase {
                controls.append(.init(id: "browser-preview-failure", label: model.pageGone ? "This page stopped responding" : "Page didn’t load", value: message))
                controls.append(.init(id: "browser-preview-retry", label: "Try again", tap: { model.reload() }))
            }
            return controls
        }, actions: ["dismiss": { _ in close() }])).frame(width: 0, height: 0).accessibilityHidden(true) }
        #endif
    }
}

private struct PreviewImage: Identifiable {
    let id = UUID()
    let image: UIImage
}
private struct PreviewShare: UIViewControllerRepresentable {
    let image: UIImage
    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: [image], applicationActivities: nil)
    }
    func updateUIViewController(_ controller: UIActivityViewController, context: Context) {}
}

/// What became of Show the agent's screenshot, so the notice names the step
/// that failed: taking the picture, room in the message, or adding it.
enum BrowserScreenshotStaging: Equatable, Sendable {
    case added
    /// The page couldn't be captured (still loading, or the view went away).
    case notCaptured
    /// The message already has its four attachments.
    case attachmentsFull
    /// The draft refused it; the reason is the composer's own when it gave one.
    case notAdded(String?)

    var notice: String {
        switch self {
        case .added: "Screenshot added to your message"
        case .notCaptured: "The page couldn’t be captured. Wait for it to finish loading, then try again."
        case .attachmentsFull: "Remove an attachment to add the screenshot. You can attach up to four files."
        case .notAdded(let reason): reason ?? "The screenshot couldn’t be added to your message. Try again."
        }
    }
}

// MARK: - Web links

/// Opens every web link handed to SwiftUI's `openURL` below the app root
/// (links in messages, a preview's public page, a sign-in page) where Settings
/// says (`LinkOpening`): Cupola's own link viewer (`LinkViewer`) by default. The choice is
/// read when a link is tapped, so changing it re-renders nothing.
struct LinkOpeningHost: ViewModifier {
    private struct Page: Identifiable { let id = UUID(); let url: URL }
    @State private var page: Page?
    func body(content: Content) -> some View {
        content.environment(\.openURL, OpenURLAction { url in
            let destination = LinkOpening(stored: UserDefaults.standard.string(forKey: LinkOpening.storageKey)).destination(for: url)
            #if WONDER_DIAGNOSTICS
            if DiagnosticTestDriver.requested {
                let opening: (String, URL) = switch destination {
                case .inApp(let page): ("inApp", page)
                case .safari(let safari, _): ("safari", safari)
                case .system(let target): ("system", target)
                }
                MainActor.assumeIsolated {
                    DiagnosticTestDriver.shared.openedLinks.append(["url": opening.1.absoluteString, "opening": opening.0])
                }
            }
            #endif
            switch destination {
            case .inApp(let page):
                self.page = Page(url: page)
                return .handled
            case .safari(let safari, let fallback):
                UIApplication.shared.open(safari) { opened in
                    if !opened { UIApplication.shared.open(fallback) }
                }
                return .handled
            case .system(let target):
                return .systemAction(target)
            }
        })
        .fullScreenCover(item: $page) { page in
            LinkViewer(url: page.url, close: { self.page = nil })
        }
    }
}

// MARK: - In-Cupola link viewer

/// A public web page in Cupola's own browser: the same ✕ close as the page
/// preview, a title, and one menu. Pages load in a private session (nothing
/// is kept after ✕) and only http(s) pages are followed here; mail and phone
/// links go to the system.
@MainActor final class LinkViewerModel: NSObject, ObservableObject, WKNavigationDelegate, WKUIDelegate {
    enum Phase: Equatable { case loading, ready, failed(String) }
    @Published private(set) var phase = Phase.loading
    @Published private(set) var progress: Double = 0
    @Published private(set) var title: String
    @Published private(set) var canGoBack = false
    let webView: WKWebView
    let url: URL
    private var observations: [NSKeyValueObservation] = []

    init(url: URL) {
        self.url = url
        title = url.host ?? "Link"
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.allowsInlineMediaPlayback = true
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false
        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.allowsBackForwardNavigationGestures = true
        webView.accessibilityIdentifier = "link-viewer-web"
        #if WONDER_DIAGNOSTICS
        webView.isInspectable = true
        #endif
        self.webView = webView
        super.init()
        webView.navigationDelegate = self
        webView.uiDelegate = self
        observations = [
            webView.observe(\.title, options: [.new]) { [weak self] view, _ in
                MainActor.assumeIsolated { if let title = view.title, !title.isEmpty { self?.title = title } }
            },
            webView.observe(\.estimatedProgress, options: [.new]) { [weak self] view, _ in
                MainActor.assumeIsolated { self?.progress = view.estimatedProgress }
            },
            webView.observe(\.canGoBack, options: [.new]) { [weak self] view, _ in
                MainActor.assumeIsolated { self?.canGoBack = view.canGoBack }
            },
        ]
        webView.load(URLRequest(url: url))
    }

    var currentURL: URL { webView.url ?? url }

    func reload() {
        if case .failed = phase, webView.url == nil { phase = .loading; webView.load(URLRequest(url: url)) }
        else { phase = .loading; webView.reload() }
    }

    func goBack() { webView.goBack() }

    /// Stops loading when the viewer closes so nothing keeps running behind the chat.
    func stop() {
        observations = []
        webView.stopLoading()
        webView.navigationDelegate = nil
        webView.uiDelegate = nil
    }

    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                 decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void) {
        guard let url = action.request.url, let scheme = url.scheme?.lowercased() else { decisionHandler(.cancel); return }
        switch scheme {
        case "http", "https", "about", "blob", "data":
            decisionHandler(.allow)
        case "mailto", "tel", "sms":
            decisionHandler(.cancel)
            if action.navigationType == .linkActivated { UIApplication.shared.open(url) }
        default:
            decisionHandler(.cancel)
        }
    }

    func webView(_ webView: WKWebView, decidePolicyFor response: WKNavigationResponse,
                 decisionHandler: @escaping @MainActor @Sendable (WKNavigationResponsePolicy) -> Void) {
        // A file the page can't show (a download) goes to the system rather than a blank page.
        // Only the page itself is handed over; an embedded frame serving a file is dropped without leaving the app.
        if response.canShowMIMEType { decisionHandler(.allow); return }
        decisionHandler(.cancel)
        if response.isForMainFrame, let url = response.response.url { UIApplication.shared.open(url) }
    }

    func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) { phase = .loading }
    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) { phase = .ready }
    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) { failed(error) }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) { failed(error) }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) { phase = .failed("The page stopped responding.") }

    private func failed(_ error: Error) {
        let error = error as NSError
        if error.domain == NSURLErrorDomain, error.code == NSURLErrorCancelled { return }
        // A navigation that became a download or another handler's page is not a failure.
        if error.domain == "WebKitErrorDomain", error.code == 102 || error.code == 101 { return }
        phase = .failed(error.localizedDescription)
    }

    /// A link that opens a new window loads here instead.
    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if action.targetFrame == nil, let url = action.request.url { webView.load(URLRequest(url: url)) }
        return nil
    }
}

struct LinkViewer: View {
    @StateObject private var model: LinkViewerModel
    let close: () -> Void
    @State private var notice: String?

    init(url: URL, close: @escaping () -> Void) {
        _model = StateObject(wrappedValue: LinkViewerModel(url: url))
        self.close = close
    }

    var body: some View {
        NavigationStack {
            ZStack(alignment: .top) {
                PreviewWebView(webView: model.webView)
                    // The page runs under the header and to the bottom edge; WebKit insets its content for both.
                    .ignoresSafeArea(.container, edges: [.top, .bottom])
                if model.phase == .loading {
                    ProgressView(value: max(0.05, model.progress)).progressViewStyle(.linear)
                }
                if case .failed(let message) = model.phase {
                    ContentUnavailableView {
                        Label("Page didn’t load", systemImage: "exclamationmark.triangle")
                    } description: { Text(message) } actions: {
                        Button("Try again") { model.reload() }.accessibilityIdentifier("link-viewer-retry")
                    }
                    .background(.background)
                    .accessibilityIdentifier("link-viewer-failure")
                }
            }
            .wonderPage()
            .navigationTitle(model.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Menu {
                        if model.canGoBack {
                            Button("Back", systemImage: "chevron.backward") { model.goBack() }
                        }
                        Button("Reload", systemImage: "arrow.clockwise") { model.reload() }
                            .accessibilityIdentifier("link-viewer-reload")
                        Divider()
                        Button("Copy link", systemImage: "doc.on.doc") {
                            UIPasteboard.general.url = model.currentURL
                            notice = "Link copied"
                            AccessibilityNotification.Announcement("Link copied").post()
                        }
                        Button("Open in browser", systemImage: "safari") { UIApplication.shared.open(model.currentURL) }
                    } label: { Label("More", systemImage: "ellipsis.circle") }
                    .accessibilityIdentifier("link-viewer-menu")
                    Button("Close", systemImage: "xmark", action: close)
                        .labelStyle(.iconOnly)
                        .accessibilityIdentifier("link-viewer-close")
                }
            }
        }
        .accessibilityIdentifier("link-viewer")
        .accessibilityAction(.escape) { close() }
        .onDisappear { model.stop() }
        .overlay(alignment: .top) {
            if let notice {
                TemporaryNotice(key: notice) { Text(notice).padding().background(.regularMaterial, in: Capsule()).padding(.top, 60) }
            }
        }
        #if WONDER_DIAGNOSTICS
        .background { DiagnosticDriverSurface(surface: .init(priority: 60, state: {
            ["browser": ["kind": "link", "fullScreen": true, "url": model.currentURL.absoluteString, "title": model.title]]
        }, controls: {
            [.init(id: "link-viewer-close", label: "Close", tap: close),
             .init(id: "link-viewer-reload", label: "Reload", tap: { model.reload() })]
        }, actions: ["dismiss": { _ in close() }])).frame(width: 0, height: 0).accessibilityHidden(true) }
        #endif
    }
}

/// A link an agent wrote to a page on its computer, such as
/// `http://localhost:5173/settings`. On the phone `localhost` is the phone
/// itself, so in a Project chat the link opens in the browser through the
/// computer instead (`LoopbackLinkRouting`).
struct LoopbackLink: Equatable {
    let port: Int
    /// The path with its query and fragment, as the browser loads it.
    let path: String

    private static let hosts: Set<String> = ["localhost", "127.0.0.1", "::1"]

    init?(_ url: URL) {
        guard let scheme = url.scheme?.lowercased(), scheme == "http" || scheme == "https",
              url.user == nil, url.password == nil,
              let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
              let host = components.host?.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "[]")),
              Self.hosts.contains(host) else { return nil }
        let port = components.port ?? (scheme == "https" ? 443 : 80)
        var path = components.percentEncodedPath.isEmpty ? "/" : components.percentEncodedPath
        if let query = components.percentEncodedQuery { path += "?" + query }
        if let fragment = components.percentEncodedFragment { path += "#" + fragment }
        guard DevServerPreview.url(port: port, path: path) != nil else { return nil }
        self.port = port
        self.path = path
    }

    /// Whether one of this chat's servers answers on the link's port.
    func isServed(by servers: [DevServer]) -> Bool { servers.contains { $0.port == port } }

    /// What the chat says when the link's page isn't served for this chat.
    /// `previewAvailable` is false when the computer can't show its pages here.
    static func notServedMessage(computer: String, previewAvailable: Bool) -> String {
        previewAvailable ? "This page runs on \(computer). Start it there, then tap Preview."
            : "This page runs on \(computer). Open it there."
    }
}

/// Sends links to the computer's own pages (`LoopbackLink`) to `open`; every
/// other link goes on to the app's opener (`LinkOpeningHost`). Without
/// `open` (not a Project chat on a paired computer) links open as before.
struct LoopbackLinkRouting: ViewModifier {
    let open: (@MainActor (LoopbackLink) -> Void)?
    /// The app's link opener, read above the override so the override never calls itself.
    @Environment(\.openURL) private var openWebLink

    @MainActor static func route(_ url: URL, open: ((LoopbackLink) -> Void)?, openWebLink: (URL) -> Void) {
        guard let open, let link = LoopbackLink(url) else { openWebLink(url); return }
        #if WONDER_DIAGNOSTICS
        if DiagnosticTestDriver.requested {
            DiagnosticTestDriver.shared.openedLinks.append(["url": url.absoluteString, "opening": "computer"])
        }
        #endif
        open(link)
    }

    func body(content: Content) -> some View {
        content.environment(\.openURL, OpenURLAction { [openWebLink, open] url in
            MainActor.assumeIsolated { Self.route(url, open: open, openWebLink: { openWebLink($0) }) }
            return .handled
        })
    }
}

extension LinkOpening {
    var title: String {
        switch self {
        case .inApp: "In Cupola"
        case .safari: "In Safari"
        case .defaultBrowser: "In default browser"
        }
    }
}
