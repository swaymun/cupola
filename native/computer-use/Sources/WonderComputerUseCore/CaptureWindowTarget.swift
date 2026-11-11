import Foundation

/// A phone request to share one app's window, as the computer-use agent names
/// it: `app:<appIdentifier>` or `app:<appIdentifier>#<windowNumber>`.
///
/// `appIdentifier` is usually a bundle identifier (`com.apple.TextEdit`), but
/// may be a display name (`TextEdit`) or a process path
/// (`/Applications/TextEdit.app/Contents/MacOS/TextEdit`). `windowNumber` is the
/// native CGWindowID. An opaque identifier from another API must not be used.
public struct CaptureWindowTarget: Equatable, Sendable {
    public static let prefix = "app:"
    public static let maximumSourceIDBytes = 160

    public let appIdentifier: String
    public let windowNumber: UInt32?

    public init?(appIdentifier: String, windowNumber: UInt32? = nil) {
        let trimmed = appIdentifier.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty, trimmed == appIdentifier,
              !appIdentifier.contains("#"),
              !appIdentifier.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) })
        else { return nil }
        self.appIdentifier = appIdentifier
        self.windowNumber = windowNumber
        guard sourceID.utf8.count <= Self.maximumSourceIDBytes else { return nil }
    }

    /// Returns nil for anything that is not a well-formed app target, including
    /// `display:` and `window:` ids.
    public init?(sourceID: String) {
        guard sourceID.utf8.count <= Self.maximumSourceIDBytes,
              sourceID.hasPrefix(Self.prefix) else { return nil }
        let body = sourceID.dropFirst(Self.prefix.count)
        let parts = body.split(separator: "#", maxSplits: 1, omittingEmptySubsequences: false)
        var windowNumber: UInt32?
        if parts.count == 2 {
            let digits = parts[1]
            guard !digits.isEmpty, digits.allSatisfy({ $0.isASCII && $0.isNumber }),
                  let value = UInt32(digits) else { return nil }
            windowNumber = value
        }
        self.init(appIdentifier: String(parts[0]), windowNumber: windowNumber)
    }

    public var sourceID: String {
        if let windowNumber { return "\(Self.prefix)\(appIdentifier)#\(windowNumber)" }
        return "\(Self.prefix)\(appIdentifier)"
    }
}

/// The platform-independent facts about one shareable window that resolution
/// needs. `zOrder` is the front-to-back position (lower is more frontmost).
public struct CaptureWindowCandidate: Equatable, Sendable {
    public let windowID: UInt32
    public let bundleIdentifier: String?
    public let applicationName: String?
    public let processID: Int32?
    public let title: String?
    public let width: Double
    public let height: Double
    public let isOnScreen: Bool
    public let windowLayer: Int
    public let zOrder: Int?
    public let opacity: Double
    public let isRegularApplication: Bool

    public init(
        windowID: UInt32,
        bundleIdentifier: String? = nil,
        applicationName: String? = nil,
        processID: Int32? = nil,
        title: String? = nil,
        width: Double,
        height: Double,
        isOnScreen: Bool = true,
        windowLayer: Int = 0,
        zOrder: Int? = nil,
        opacity: Double = 1,
        isRegularApplication: Bool = true
    ) {
        self.windowID = windowID
        self.bundleIdentifier = bundleIdentifier
        self.applicationName = applicationName
        self.processID = processID
        self.title = title
        self.width = width
        self.height = height
        self.isOnScreen = isOnScreen
        self.windowLayer = windowLayer
        self.zOrder = zOrder
        self.opacity = opacity
        self.isRegularApplication = isRegularApplication
    }

}

public enum CaptureWindowResolver {
    public static let minimumWidth: Double = 80
    public static let minimumHeight: Double = 60

    /// Picks the window to capture for `target`, or nil when the app has no
    /// eligible window. On-screen windows of at least 80x60 points qualify:
    /// ordinary windows use the normal layer; an exact system-dialog target
    /// also permits its elevated layer.
    public static func resolve(_ target: CaptureWindowTarget, candidates: [CaptureWindowCandidate]) -> UInt32? {
        let eligible = candidates.filter {
            // System dialogs use elevated layers (the recorded notification is
            // layer 8). Only an exact, observed dialog target may capture one.
            let exactDialog = target.windowNumber == $0.windowID
                && target.appIdentifier == $0.applicationName
                && SystemDialogDetector.owners[$0.applicationName ?? ""] != nil
            return $0.isOnScreen && ($0.windowLayer == 0 || exactDialog)
                && $0.width >= minimumWidth && $0.height >= minimumHeight
        }
        let names = matchNames(for: target.appIdentifier)

        if let windowNumber = target.windowNumber {
            // An explicit window must never fall back to another window when it
            // closes or belongs to a different app.
            return eligible.first { $0.windowID == windowNumber && matches($0, names: names) }?.windowID
        }

        return eligible
            .filter { matches($0, names: names) }
            .min(by: isPreferred)?
            .windowID
    }

    /// Lowercased identifiers the target may be known by: the raw value, and
    /// for a path, its `.app` bundle name and executable name.
    static func matchNames(for appIdentifier: String) -> Set<String> {
        var names: Set<String> = [appIdentifier.lowercased()]
        guard appIdentifier.contains("/") else { return names }
        let components = appIdentifier.split(separator: "/").map(String.init)
        if let last = components.last, !last.isEmpty {
            names.insert(last.lowercased())
            if last.lowercased().hasSuffix(".app") { names.insert(String(last.dropLast(4)).lowercased()) }
        }
        if let bundle = components.last(where: { $0.lowercased().hasSuffix(".app") }) {
            names.insert(String(bundle.dropLast(4)).lowercased())
        }
        return names
    }

    static func matches(_ candidate: CaptureWindowCandidate, names: Set<String>) -> Bool {
        if let bundle = candidate.bundleIdentifier?.lowercased(), !bundle.isEmpty, names.contains(bundle) { return true }
        if let name = candidate.applicationName?.lowercased(), !name.isEmpty, names.contains(name) { return true }
        return false
    }

    /// Frontmost first, then titled, then largest, then lowest id.
    static func isPreferred(_ lhs: CaptureWindowCandidate, _ rhs: CaptureWindowCandidate) -> Bool {
        let lhsZ = lhs.zOrder ?? Int.max
        let rhsZ = rhs.zOrder ?? Int.max
        if lhsZ != rhsZ { return lhsZ < rhsZ }
        let lhsTitled = !(lhs.title ?? "").isEmpty
        let rhsTitled = !(rhs.title ?? "").isEmpty
        if lhsTitled != rhsTitled { return lhsTitled }
        let lhsArea = lhs.width * lhs.height
        let rhsArea = rhs.width * rhs.height
        if lhsArea != rhsArea { return lhsArea > rhsArea }
        return lhs.windowID < rhs.windowID
    }
}
