import AppKit
import ApplicationServices

/// Whether keyboard focus on the Mac is in a place that accepts typing, so the
/// phone can raise its keyboard while it controls the Mac. Only this yes/no
/// leaves the Mac; a field's contents and labels are never read.
public enum TextInputFocus {
    static let textRoles: Set<String> = [
        kAXTextFieldRole as String, kAXTextAreaRole as String, kAXComboBoxRole as String,
    ]

    /// Search and password fields are text fields with a subrole. A read-only
    /// text view (a log, a label rendered as text) has a value that cannot be set.
    public static func acceptsTyping(role: String?, valueSettable: Bool?) -> Bool {
        guard let role, textRoles.contains(role) else { return false }
        return valueSettable ?? true
    }

    /// The shared window typing must reach.
    public struct ShownWindow: Equatable, Sendable {
        public let processID: pid_t
        public let windowID: UInt32

        public init(processID: pid_t, windowID: UInt32) {
            self.processID = processID
            self.windowID = windowID
        }
    }

    /// Typing reaches only the window being shown: a field in another window
    /// of the same app does not count, one in a sheet on that window does.
    /// `shownWindow` is `nil` when a whole display is shared.
    static func acceptsTyping(role: String?, valueSettable: Bool?,
                              in window: SystemDialogWindowRaiser.FocusedWindow?, shownWindow: UInt32?) -> Bool {
        if let shownWindow, window?.accepts(shownWindow) != true { return false }
        return acceptsTyping(role: role, valueSettable: valueSettable)
    }

    /// Reads the frontmost app's focused element and the window it is in.
    /// Each Accessibility call is bounded so a busy app cannot stall the caller.
    public static func current(in shownWindow: ShownWindow?, timeout: Float = 0.25) -> Bool {
        guard AXIsProcessTrusted(),
              let frontmost = NSWorkspace.shared.frontmostApplication?.processIdentifier,
              shownWindow.map({ $0.processID == frontmost }) ?? true else { return false }
        let app = AXUIElementCreateApplication(frontmost)
        AXUIElementSetMessagingTimeout(app, timeout)
        guard let element = SystemDialogWindowRaiser.element(app, kAXFocusedUIElementAttribute) else { return false }
        AXUIElementSetMessagingTimeout(element, timeout)
        var window: SystemDialogWindowRaiser.FocusedWindow?
        if shownWindow != nil {
            // An element that does not name its window is in the focused one.
            window = (SystemDialogWindowRaiser.element(element, kAXWindowAttribute)
                ?? SystemDialogWindowRaiser.element(app, kAXFocusedWindowAttribute))
                .flatMap { SystemDialogWindowRaiser.inputWindow($0, timeout: timeout) }
        }
        var role: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, kAXRoleAttribute as CFString, &role) == .success else { return false }
        var settable = DarwinBoolean(false)
        let settableKnown = AXUIElementIsAttributeSettable(element, kAXValueAttribute as CFString, &settable) == .success
        return acceptsTyping(role: role as? String, valueSettable: settableKnown ? settable.boolValue : nil,
                             in: window, shownWindow: shownWindow?.windowID)
    }
}

/// Decides when the phone hears about typing focus: once for each control
/// lease or view, then only on change, and a final "no" when control ends
/// while a field had focus so the phone can lower its keyboard.
public struct TextFocusChanges: Sendable {
    private var last: (identity: ControlLeaseIdentity, textInput: Bool)?

    public init() {}

    /// One poll. Accessibility reads can take a while, so the lease is read
    /// again after `sample`: an answer read while control ended or changed
    /// belongs to no lease and is dropped; the next poll reports the change.
    public mutating func poll(activeIdentity: () -> ControlLeaseIdentity?, sample: () -> Bool)
        -> (identity: ControlLeaseIdentity, textInput: Bool)? {
        let identity = activeIdentity()
        let textInput = identity != nil && sample()
        guard activeIdentity() == identity else { return nil }
        return observe(identity, textInput: textInput)
    }

    public mutating func observe(_ identity: ControlLeaseIdentity?, textInput: Bool)
        -> (identity: ControlLeaseIdentity, textInput: Bool)? {
        guard let identity else {
            defer { last = nil }
            guard let last, last.textInput else { return nil }
            return (last.identity, false)
        }
        if let last, last.identity == identity, last.textInput == textInput { return nil }
        last = (identity, textInput)
        return (identity, textInput)
    }
}
