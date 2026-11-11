import Foundation

/// Where a tapped web link opens, chosen once in Settings for the whole app:
/// in Cupola's own link viewer (the default; ✕ returns to the chat), in
/// Safari, or in the browser chosen in iOS Settings. Anything that is not a web
/// page (mail, phone, Settings, another app) goes to the system unchanged.
public enum LinkOpening: String, CaseIterable, Sendable {
    case inApp
    case safari
    case defaultBrowser = "default"

    public static let storageKey = "wonder.links.open"
    public static let standard = LinkOpening.inApp

    /// The saved choice; nothing saved, or a value this version doesn't know, opens in the app.
    public init(stored: String?) {
        self = stored.flatMap(LinkOpening.init(rawValue:)) ?? .standard
    }

    public enum Destination: Equatable, Sendable {
        /// Cupola's link viewer, over the screen the link was on.
        case inApp(URL)
        /// Safari's own address for the page, then `fallback` in the default
        /// browser if Safari doesn't take it.
        case safari(URL, fallback: URL)
        /// The system's handler: the default browser for a web page.
        case system(URL)
    }

    public func destination(for url: URL) -> Destination {
        guard let scheme = url.scheme?.lowercased(), scheme == "http" || scheme == "https" else { return .system(url) }
        switch self {
        case .inApp: return .inApp(url)
        case .safari: return Self.safariURL(url).map { .safari($0, fallback: url) } ?? .system(url)
        case .defaultBrowser: return .system(url)
        }
    }

    /// `x-safari-https://…`: Safari registers these schemes and opens them even
    /// when another browser is the default. Apple doesn't document them, so
    /// callers fall back to the default browser.
    static func safariURL(_ url: URL) -> URL? {
        guard var components = URLComponents(url: url, resolvingAgainstBaseURL: false),
              let scheme = components.scheme?.lowercased() else { return nil }
        components.scheme = "x-safari-" + scheme
        return components.url
    }
}
