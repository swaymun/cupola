import CoreGraphics
import Foundation

/// A local display choice shared by Wonder Settings and its signed capture helper.
/// A missing or disconnected choice falls back to the Mac's current main display.
public struct SharedDisplayPreference {
    public static let domain = "com.saimun.wonder"
    public static let key = "capture.preferredDisplay"

    private let defaults: UserDefaults

    public init(defaults: UserDefaults = UserDefaults(suiteName: Self.domain) ?? .standard) {
        self.defaults = defaults
    }

    public var preferredIdentifier: String? {
        guard let value = defaults.string(forKey: Self.key),
              value.count <= 48,
              value.split(separator: ":").count == 3,
              value.split(separator: ":").allSatisfy({ UInt32($0) != nil }) else { return nil }
        return value
    }

    @discardableResult
    public func setPreferredIdentifier(_ identifier: String?) -> Bool {
        if let identifier {
            defaults.set(identifier, forKey: Self.key)
        } else {
            defaults.removeObject(forKey: Self.key)
        }
        return defaults.synchronize()
    }

    public static func identifier(for displayID: CGDirectDisplayID) -> String? {
        let vendor = CGDisplayVendorNumber(displayID)
        let model = CGDisplayModelNumber(displayID)
        let serial = CGDisplaySerialNumber(displayID)
        guard vendor != 0 || model != 0 || serial != 0 else { return nil }
        return "\(vendor):\(model):\(serial)"
    }

    public func preferredDisplayID(in availableIDs: [CGDirectDisplayID]) -> CGDirectDisplayID? {
        guard let preferredIdentifier else { return nil }
        let matches = availableIDs.filter { Self.identifier(for: $0) == preferredIdentifier }
        return matches.count == 1 ? matches[0] : nil
    }
}
