import AppKit
import CoreServices
import Foundation

/// Whether Wonder may send Apple events to an app, checked without showing the
/// macOS Automation prompt, so the phone can be told in plain words first.
public enum AutomationPermission: String, Sendable {
    case granted
    /// macOS would ask the person at the Mac before allowing it.
    case needsConsent
    /// The person declined it in System Settings > Privacy & Security > Automation.
    case denied
    /// The app is not running; nothing can be checked or controlled.
    case notRunning
    case unknown

    init(status: OSStatus) {
        switch Int(status) {
        case Int(noErr): self = .granted
        case Int(errAEEventWouldRequireUserConsent): self = .needsConsent
        case Int(errAEEventNotPermitted): self = .denied
        case Int(procNotFound): self = .notRunning
        default: self = .unknown
        }
    }

    /// `askIfNeeded` shows the macOS prompt; only an explicit request from the
    /// person may pass true. Never launches the target app.
    public static func check(bundleIdentifier: String, askIfNeeded: Bool = false) -> Self {
        guard !NSRunningApplication.runningApplications(withBundleIdentifier: bundleIdentifier).isEmpty else { return .notRunning }
        let target = NSAppleEventDescriptor(bundleIdentifier: bundleIdentifier)
        guard let address = target.aeDesc else { return .unknown }
        return Self(status: AEDeterminePermissionToAutomateTarget(address, typeWildCard, typeWildCard, askIfNeeded))
    }
}
