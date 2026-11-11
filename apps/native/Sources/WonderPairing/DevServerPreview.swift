import Foundation

// MARK: - Wire contract (packages/protocol/schemas/wonder-http-v1.json `devServerList`)

/// A web page a thread's process is serving on the Mac.
public struct DevServer: Codable, Hashable, Identifiable, Sendable {
    public let port: Int
    public let processName: String
    public let title: String?
    public var id: Int { port }

    public init(port: Int, processName: String, title: String? = nil) {
        self.port = port
        self.processName = processName
        self.title = title
    }

    /// The page title, else the port. The program's name stays out of the UI:
    /// a dev server is known by its page, not by `node` or `workerd`.
    public var name: String { title ?? "Port \(port)" }
}

public struct DevServerList: Codable, Hashable, Sendable {
    public let servers: [DevServer]
    public init(servers: [DevServer]) { self.servers = servers }

    /// The servers started in one thread's folder (`conversationId`), or in
    /// any of the Project's folders without one. With `port` (a link's), the
    /// answer lists only that server when it runs, even if the picker hides it.
    public static func path(projectId: String, conversationId: String? = nil, port: Int? = nil) throws -> String {
        let base = "/api/v1/projects/\(try DevServerPreview.escaped(projectId))/preview/servers"
        var query: [String] = []
        if let conversationId { query.append("conversation=" + (try DevServerPreview.escaped(conversationId))) }
        if let port { query.append("port=\(port)") }
        return query.isEmpty ? base : base + "?" + query.joined(separator: "&")
    }
}

/// What the composer's globe pill knows about a thread's web pages. The pill
/// stays briefly after the last one stops, so a quick restart (the agent
/// rebuilding) does not make it jump in and out of the row, then goes.
public struct DevServerPresence: Hashable, Sendable {
    public static let recentWindow: TimeInterval = 30
    public private(set) var servers: [DevServer] = []
    public private(set) var lastSeen: Date?

    public init() {}

    /// `lastSeen` matters only once the servers are gone, so it is set when
    /// they disappear: polling an unchanged list leaves the value equal (and
    /// the conversation is not re-rendered for it).
    public mutating func update(_ servers: [DevServer], at now: Date) {
        if servers.isEmpty, !self.servers.isEmpty { lastSeen = now }
        self.servers = servers
    }

    public func showsPill(at now: Date) -> Bool {
        if !servers.isEmpty { return true }
        guard let lastSeen else { return false }
        return now.timeIntervalSince(lastSeen) < Self.recentWindow
    }
}

/// Mobile renders as this phone does; desktop asks for the desktop site at a
/// laptop width and zooms it to fit the screen.
public enum PreviewViewport: String, CaseIterable, Sendable {
    case mobile, desktop

    public static let desktopWidth = 1280
    /// Safari on a Mac, which sites treat as a desktop browser.
    public static let desktopUserAgent =
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15"

    public var userAgent: String? { self == .desktop ? Self.desktopUserAgent : nil }

    /// Runs at document start: keeps every viewport tag at the desktop width,
    /// including the page's own `width=device-width` as the parser reaches it,
    /// so a responsive page lays out as on a laptop and WebKit scales it to
    /// the screen's width.
    public var viewportScript: String? {
        guard self == .desktop else { return nil }
        return """
        (() => {
          const content = 'width=\(Self.desktopWidth)';
          const apply = () => {
            const root = document.head || document.documentElement;
            if (!root) return;
            let tags = root.ownerDocument.querySelectorAll('meta[name="viewport"]');
            if (tags.length === 0) {
              const meta = document.createElement('meta'); meta.name = 'viewport'; root.appendChild(meta);
              tags = [meta];
            }
            tags.forEach((meta) => { if (meta.getAttribute('content') !== content) meta.setAttribute('content', content); });
          };
          new MutationObserver(apply).observe(document, { subtree: true, childList: true, attributes: true, attributeFilter: ['content'] });
          apply();
        })();
        """
    }
}

/// Redirect advice from the Mac's proxy; it never follows redirects itself.
public enum PreviewRedirect: Equatable, Sendable {
    /// Another page on the same dev server, as a path on the preview's origin.
    case path(String)
    /// Another web site, which opens as an ordinary page.
    case external(URL)
}

/// Pages load through a custom URL scheme, `wonder-preview://port-5173/...`,
/// whose handler sends every request through the paired Mac's authenticated
/// proxy. Root-relative URLs (`/assets/app.js`) resolve on that origin, each
/// port is its own origin, and the session never appears in a URL or to the page.
public enum DevServerPreview {
    public static let scheme = "wonder-preview"
    static let hostPrefix = "port-"

    // Headers shared with the Mac's proxy (crates/wonderd/src/dev_preview.rs).
    public static let upstreamHeader = "x-wonder-preview-upstream"
    public static let locationHeader = "x-wonder-preview-location"
    public static let externalLocationHeader = "x-wonder-preview-external-location"
    public static let cookieHeader = "x-wonder-preview-cookie"
    public static let setCookieHeader = "x-wonder-preview-set-cookie"
    public static let securityPolicyHeader = "x-wonder-preview-content-security-policy"
    /// Names the thread whose folder the page's server runs in.
    public static let conversationHeader = "x-wonder-preview-conversation"

    static let forwardedRequestHeaders = [
        "Accept", "Accept-Language", "Cache-Control", "Content-Type", "If-Match", "If-Modified-Since",
        "If-None-Match", "If-Range", "Pragma", "Range", "User-Agent", "X-Requested-With",
    ]
    static let forwardedResponseHeaders = [
        "accept-ranges", "content-disposition", "content-language", "content-range", "content-type",
        "etag", "last-modified", "vary",
    ]

    static func escaped(_ projectId: String) throws -> String {
        guard !projectId.isEmpty, projectId.utf8.count <= 128,
              let escaped = projectId.addingPercentEncoding(withAllowedCharacters: .alphanumerics) else {
            throw PairingFailure.invalidLink
        }
        return escaped
    }

    /// The page address for a dev server's path.
    public static func url(port: Int, path: String = "/") -> URL? {
        guard (1...65535).contains(port) else { return nil }
        let path = path.hasPrefix("/") ? path : "/" + path
        return URL(string: "\(scheme)://\(hostPrefix)\(port)\(path)")
    }

    /// The dev server port a page address belongs to.
    public static func port(of url: URL) -> Int? {
        guard url.scheme?.lowercased() == scheme, let host = url.host?.lowercased(),
              host.hasPrefix(hostPrefix), let port = Int(host.dropFirst(hostPrefix.count)),
              (1...65535).contains(port), url.port == nil, url.user == nil else { return nil }
        return port
    }

    /// The Mac's proxy path for a page address, or nil for an address that is
    /// not a preview page or could leave the dev server's root.
    public static func hostPath(for url: URL, projectId: String) -> String? {
        guard let port = port(of: url),
              let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
              let project = try? escaped(projectId) else { return nil }
        let encoded = components.percentEncodedPath.isEmpty ? "/" : components.percentEncodedPath
        let lower = encoded.lowercased()
        guard encoded.hasPrefix("/"), !encoded.contains("\\"), !lower.contains("%2f"), !lower.contains("%5c"),
              let decoded = encoded.removingPercentEncoding,
              !decoded.contains("\\"), !decoded.contains("\0"),
              !decoded.split(separator: "/", omittingEmptySubsequences: false).contains(where: { $0 == ".." || $0 == "." })
        else { return nil }
        var path = "/api/v1/projects/\(project)/preview/\(port)\(encoded)"
        if let query = components.percentEncodedQuery, !query.isEmpty { path += "?" + query }
        return path
    }

    /// The authenticated request to the Mac for a page's request. The page's
    /// own headers pass only from an allowlist; its cookies travel separately.
    public static func request(for page: URLRequest, projectId: String, conversationId: String? = nil,
                               connection: SavedConnection, cookies: String?, timeout: TimeInterval = 60) -> URLRequest? {
        guard let url = page.url, let path = hostPath(for: url, projectId: projectId),
              let origin = try? PairingLink.origin(connection.origin),
              let target = URL(string: origin + path) else { return nil }
        var request = URLRequest(url: target, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: timeout)
        let method = (page.httpMethod ?? "GET").uppercased()
        guard !["CONNECT", "TRACE"].contains(method) else { return nil }
        request.httpMethod = method
        request.httpBody = page.httpBody
        request.httpShouldHandleCookies = false
        for name in forwardedRequestHeaders {
            if let value = page.value(forHTTPHeaderField: name) { request.setValue(value, forHTTPHeaderField: name) }
        }
        request.setValue(origin, forHTTPHeaderField: "Origin")
        request.setValue("__Host-wonder_session=\(connection.credential.sessionToken)", forHTTPHeaderField: "Cookie")
        request.setValue(connection.credential.csrfToken, forHTTPHeaderField: "x-wonder-csrf")
        if let cookies, !cookies.isEmpty { request.setValue(cookies, forHTTPHeaderField: cookieHeader) }
        if let conversationId { request.setValue(conversationId, forHTTPHeaderField: conversationHeader) }
        return request
    }

    /// The live-reload socket a page opens, as the Mac's proxy address.
    public static func socketRequest(page: URL, path: String, protocols: [String], projectId: String,
                                     conversationId: String? = nil, connection: SavedConnection,
                                     cookies: String?) -> URLRequest? {
        guard let port = port(of: page), path.hasPrefix("/"),
              let pageURL = URL(string: "\(scheme)://\(hostPrefix)\(port)\(path)"),
              let proxied = hostPath(for: pageURL, projectId: projectId),
              let origin = try? PairingLink.origin(connection.origin),
              var components = URLComponents(string: origin + proxied) else { return nil }
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        guard let url = components.url else { return nil }
        var request = URLRequest(url: url, timeoutInterval: 30)
        request.httpShouldHandleCookies = false
        request.setValue(origin, forHTTPHeaderField: "Origin")
        request.setValue("__Host-wonder_session=\(connection.credential.sessionToken)", forHTTPHeaderField: "Cookie")
        let protocols = protocols.filter { !$0.isEmpty && $0.utf8.count <= 128 && !$0.contains(",") }.prefix(8)
        if !protocols.isEmpty { request.setValue(protocols.joined(separator: ", "), forHTTPHeaderField: "Sec-WebSocket-Protocol") }
        if let cookies, !cookies.isEmpty { request.setValue(cookies, forHTTPHeaderField: cookieHeader) }
        if let conversationId { request.setValue(conversationId, forHTTPHeaderField: conversationHeader) }
        return request
    }

    /// Whether the dev server answered (as opposed to Wonder refusing or
    /// failing the request: a session that ended, a server that stopped).
    public static func isFromDevServer(_ response: HTTPURLResponse) -> Bool {
        response.value(forHTTPHeaderField: upstreamHeader) == "1"
    }

    /// The headers a page sees: the dev server's own, with its security policy
    /// restored, never the Wonder API's headers.
    public static func pageHeaders(from response: HTTPURLResponse) -> [String: String] {
        var headers: [String: String] = [:]
        for name in forwardedResponseHeaders {
            if let value = response.value(forHTTPHeaderField: name) { headers[name] = value }
        }
        if let policy = response.value(forHTTPHeaderField: securityPolicyHeader) {
            headers["content-security-policy"] = policy
        }
        return headers
    }

    public static func redirect(from response: HTTPURLResponse) -> PreviewRedirect? {
        if let path = response.value(forHTTPHeaderField: locationHeader), path.hasPrefix("/"), !path.hasPrefix("//") {
            return .path(path)
        }
        if let external = response.value(forHTTPHeaderField: externalLocationHeader).flatMap(URL.init(string:)),
           ["http", "https"].contains(external.scheme?.lowercased() ?? "") {
            return .external(external)
        }
        return nil
    }

    /// A small page that moves the web view to `target`; used for a top-level
    /// page whose dev server redirected, so the address bar and relative links
    /// follow the new page.
    public static func redirectDocument(to target: String) -> Data {
        let literal = (try? JSONEncoder().encode(target)).flatMap { String(data: $0, encoding: .utf8) } ?? "\"/\""
        return Data("<!doctype html><meta charset=\"utf-8\"><script>location.replace(\(literal))</script>".utf8)
    }

    /// A page on the public web, which Safari can open too. A dev server's
    /// preview, `localhost` and private addresses are only reachable here.
    public static func isPublicWebURL(_ url: URL?) -> Bool {
        guard let url, ["http", "https"].contains(url.scheme?.lowercased() ?? ""),
              let host = url.host?.lowercased(), !host.isEmpty else { return false }
        if host == "localhost" || host.hasSuffix(".localhost") || host.hasSuffix(".local") || host.hasSuffix(".internal")
            || host.hasSuffix(".ts.net") || !host.contains(".") && !host.contains(":") { return false }
        let octets = host.split(separator: ".").compactMap { UInt8($0) }
        if octets.count == 4 {
            switch (octets[0], octets[1]) {
            case (10, _), (127, _), (0, _), (169, 254), (192, 168): return false
            case (172, 16...31), (100, 64...127): return false
            default: return true
            }
        }
        let bare = host.trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        if bare.contains(":") {
            return !(bare == "::1" || bare == "::" || bare.hasPrefix("fe80") || bare.hasPrefix("fc") || bare.hasPrefix("fd"))
        }
        return true
    }
}

/// The dev page's cookies for one preview session, kept in memory and sent
/// only to the dev server they came from. Attributes other than expiry are
/// ignored: every cookie applies to the whole preview origin.
public struct PreviewCookieJar: Sendable {
    private var cookies: [Int: [String: String]] = [:]
    public static let maxPerPort = 64
    public init() {}

    public mutating func store(_ header: String, port: Int) {
        // Several Set-Cookie values arrive joined by ", "; split on commas that
        // start a new `name=` pair, not inside an Expires date.
        for line in Self.split(header) {
            let parts = line.split(separator: ";", omittingEmptySubsequences: false).map { $0.trimmingCharacters(in: .whitespaces) }
            guard let pair = parts.first, let equals = pair.firstIndex(of: "=") else { continue }
            let name = String(pair[..<equals]).trimmingCharacters(in: .whitespaces)
            let value = String(pair[pair.index(after: equals)...])
            guard !name.isEmpty, name.utf8.count + value.utf8.count <= 4096 else { continue }
            let expired = parts.dropFirst().contains { attribute in
                let lower = attribute.lowercased()
                if lower.hasPrefix("max-age="), let age = Int(lower.dropFirst(8)) { return age <= 0 }
                return false
            }
            if expired || value.isEmpty && parts.dropFirst().contains(where: { $0.lowercased().hasPrefix("expires=") }) {
                cookies[port]?.removeValue(forKey: name)
            } else if cookies[port, default: [:]].count < Self.maxPerPort || cookies[port]?[name] != nil {
                cookies[port, default: [:]][name] = value
            }
        }
    }

    public func header(port: Int) -> String? {
        guard let jar = cookies[port], !jar.isEmpty else { return nil }
        return jar.keys.sorted().map { "\($0)=\(jar[$0]!)" }.joined(separator: "; ")
    }

    static func split(_ header: String) -> [String] {
        var lines: [String] = []
        var current = ""
        let pieces = header.components(separatedBy: ",")
        for piece in pieces {
            let trimmed = piece.trimmingCharacters(in: .whitespaces)
            let startsCookie = trimmed.range(of: #"^[!#$%&'*+\-.^_`|~0-9A-Za-z]+="#, options: .regularExpression) != nil
            if current.isEmpty { current = trimmed }
            else if startsCookie { lines.append(current); current = trimmed }
            else { current += "," + piece }
        }
        if !current.isEmpty { lines.append(current) }
        return lines
    }
}
