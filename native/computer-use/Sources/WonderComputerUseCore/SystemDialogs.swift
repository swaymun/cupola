import CoreGraphics
import Foundation
import AppKit
import ApplicationServices

@_silgen_name("_AXUIElementGetWindow")
private func dialogWindowNumber(_ element: AXUIElement, _ number: UnsafeMutablePointer<CGWindowID>) -> AXError

/// A macOS dialog that waits for the person at the Mac: a privacy permission
/// prompt, a keychain or administrator password, or a "downloaded from the
/// internet" confirmation. Wonder only reports it; it never clicks one.
public struct SystemDialog: Equatable, Sendable {
    public enum Kind: String, Sendable {
        /// Automation, Files and Folders, Screen Recording and other privacy prompts.
        case permission
        /// "… would like to control this computer using accessibility features."
        case accessibility
        /// A keychain or administrator password.
        case password
        /// Opening a downloaded app or file.
        case openConfirmation
    }

    public let kind: Kind
    public let owner: String
    public let windowID: UInt32
    public let x: Double
    public let y: Double
    public let width: Double
    public let height: Double

    public var json: [String: Any] {
        ["kind": kind.rawValue, "owner": owner, "windowId": windowID,
         "x": x, "y": y, "width": width, "height": height]
    }
}

public enum SystemDialogDetector {
    /// The processes that draw these dialogs, by their window owner name. Window
    /// owner and bounds are readable without Screen Recording; titles are not
    /// needed. Measured on macOS 26.6 (`ps`): all four run in the login session.
    static let owners: [String: SystemDialog.Kind] = [
        "UserNotificationCenter": .permission,
        "universalAccessAuthWarn": .accessibility,
        "SecurityAgent": .password,
        "CoreServicesUIAgent": .openConfirmation,
    ]

    /// Ignores menu-bar items and other slivers these agents may own.
    static let minimumWidth: Double = 160
    static let minimumHeight: Double = 80

    /// Dialogs among `CGWindowListCopyWindowInfo` entries, ordered by window ID
    /// so an unchanged screen yields an equal result.
    public static func dialogs(in windowInfo: [[String: Any]]) -> [SystemDialog] {
        windowInfo.compactMap { info -> SystemDialog? in
            guard let owner = info[kCGWindowOwnerName as String] as? String, let kind = owners[owner],
                  (info[kCGWindowIsOnscreen as String] as? Bool) ?? true,
                  let number = (info[kCGWindowNumber as String] as? NSNumber)?.uint32Value, number > 0,
                  let boundsInfo = info[kCGWindowBounds as String] as? NSDictionary,
                  let bounds = CGRect(dictionaryRepresentation: boundsInfo as CFDictionary),
                  bounds.width >= minimumWidth, bounds.height >= minimumHeight else { return nil }
            return SystemDialog(kind: kind, owner: owner, windowID: number, x: bounds.minX, y: bounds.minY,
                                width: bounds.width, height: bounds.height)
        }.sorted { $0.windowID < $1.windowID }
    }

    /// The live list from the window server.
    public static func current() -> [SystemDialog] {
        let info = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
            as? [[String: Any]] ?? []
        return dialogs(in: info)
    }
}

/// Why input could not reach the shared window, in the reasons the daemon
/// already explains to the phone. Anything else stays a generic delivery failure.
public enum ControlWindowFailure: String, Error, Sendable {
    case notFound = "window_not_found"
    case notFocused = "window_not_focused"
    case covered = "window_covered"

    /// The reason a failed input batch reports.
    public static func deliveryReason(for error: Error) -> String {
        (error as? ControlWindowFailure)?.rawValue ?? "input_delivery_failed"
    }

    /// The shared window's current frame. Input goes to the frontmost app, so
    /// another app in front means focus moved away during control. A number
    /// now owned by another process is a different window.
    public static func frame(windowID: UInt32, expectedPID: pid_t, frontmostPID: pid_t?,
                             windows: [[String: Any]]) throws -> CGRect {
        guard let window = windows.first(where: { ($0[kCGWindowNumber as String] as? NSNumber)?.uint32Value == windowID }),
              (window[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value == expectedPID,
              let bounds = window[kCGWindowBounds as String] as? NSDictionary,
              let rect = CGRect(dictionaryRepresentation: bounds as CFDictionary),
              rect.width > 0, rect.height > 0 else { throw ControlWindowFailure.notFound }
        guard frontmostPID == expectedPID else { throw ControlWindowFailure.notFocused }
        return rect
    }

    /// A click, drag or scroll lands only where the exact window is on top.
    public static func requireInput(windowID: UInt32, at point: CGPoint, windows: [[String: Any]]) throws {
        guard ScreenCaptureSession.windowReceivesInput(windowID, at: point, windows: windows) else {
            throw ControlWindowFailure.covered
        }
    }
}

/// Runs Accessibility reads of another app on its own queue, so a busy app
/// cannot hold the helper's main run loop, which also answers heartbeats and
/// signaling. The caller waits at most `budget` for the whole read; a late
/// answer is dropped. While a read is still running, a new one is refused
/// rather than queued behind it.
final class AccessibilityWorker: @unchecked Sendable {
    private let queue: DispatchQueue
    private let lock = NSLock()
    private var busy = false

    init(label: String) {
        queue = DispatchQueue(label: label, qos: .userInitiated)
    }

    func run<T: Sendable>(budget: TimeInterval, _ work: @escaping @Sendable (_ deadline: DispatchTime) -> T) async -> T? {
        let claimed = lock.withLock {
            guard !busy else { return false }
            busy = true
            return true
        }
        guard claimed else { return nil }
        let deadline = DispatchTime.now() + budget
        return await withCheckedContinuation { (continuation: CheckedContinuation<T?, Never>) in
            let answer = OnceAnswer(continuation)
            queue.async {
                let value = work(deadline)
                self.lock.withLock { self.busy = false }
                answer.resume(value)
            }
            DispatchQueue.global(qos: .userInitiated).asyncAfter(deadline: deadline) { answer.resume(nil) }
        }
    }
}

private final class OnceAnswer<T: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<T?, Never>?

    init(_ continuation: CheckedContinuation<T?, Never>) { self.continuation = continuation }

    func resume(_ value: T?) {
        let continuation = lock.withLock {
            let continuation = self.continuation
            self.continuation = nil
            return continuation
        }
        continuation?.resume(returning: value)
    }
}

/// Explicit phone navigation raises only the observed window. It never presses
/// a dialog button, falls back to another window, or requests Accessibility.
public enum SystemDialogWindowRaiser {
    public enum Result: String, Sendable { case raised, gone, unavailable }

    static let accessibility = AccessibilityWorker(label: "com.wonder.computer-use.accessibility")
    /// Each read of the shared app waits at most this long for an answer…
    static let elementTimeout: Float = 0.25
    /// …and one whole check or raise at most this long, however many windows
    /// and sheets it reads.
    static let focusBudget: TimeInterval = 0.3
    static let raiseBudget: TimeInterval = 0.5
    /// Preparing a batch must answer well inside the daemon's 2 s wait.
    static let prepareBudget: TimeInterval = 1.2

    /// Activation is asynchronous. Yield to the main run loop while waiting,
    /// and stop if the caller releases control or selects a different window.
    @MainActor public static func prepareForInput(
        windowID: UInt32, owner: String, isValid: @MainActor () -> Bool
    ) async -> Bool {
        await prepareForInput(
            raise: {
                await accessibility.run(budget: raiseBudget) { deadline in
                    raise(windowID: windowID, owner: owner, deadline: deadline)
                } ?? .unavailable
            },
            isFocused: { await isFocused(windowID: windowID, owner: owner) },
            isValid: isValid
        )
    }

    @MainActor static func prepareForInput(
        raise: @MainActor () async -> Result,
        isFocused: @MainActor () async -> Bool,
        isValid: @MainActor () -> Bool,
        attempts: Int = 40,
        pollNanoseconds: UInt64 = 25_000_000,
        budget: TimeInterval = prepareBudget
    ) async -> Bool {
        let deadline = Date().addingTimeInterval(budget)
        // Every await can outlast the lease or the selection: check both again after each.
        func usable() -> Bool { !Task.isCancelled && isValid() && Date() < deadline }
        guard usable() else { return false }
        let focusedFirst = await isFocused()
        guard usable() else { return false }
        if focusedFirst { return true }
        guard await raise() == .raised, usable() else { return false }
        for _ in 0..<attempts {
            let focused = await isFocused()
            guard usable() else { return false }
            if focused { return true }
            do { try await Task.sleep(nanoseconds: pollNanoseconds) }
            catch { return false }
            guard usable() else { return false }
        }
        let focused = await isFocused()
        return usable() && focused
    }

    static func processID(windowID: UInt32, owner: String, windows: [[String: Any]]) -> Int32? {
        windows.first {
            ($0[kCGWindowNumber as String] as? NSNumber)?.uint32Value == windowID
                && $0[kCGWindowOwnerName as String] as? String == owner
                && ($0[kCGWindowIsOnscreen as String] as? Bool) == true
        }.flatMap { ($0[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value }
    }

    /// The app's focused window as Accessibility reports it. An open sheet is an
    /// `AXSheet` child of its window, not one of the app's windows, and holds
    /// focus while it is open.
    struct FocusedWindow: Equatable {
        let windowID: UInt32
        /// The window a focused sheet belongs to.
        let sheetParentID: UInt32?
        /// Sheets open on the focused window.
        let sheetIDs: [UInt32]

        /// A sheet and its window are one input target: clicks reach whichever
        /// is on top at that point, and keys go to the sheet while it is open.
        func accepts(_ target: UInt32) -> Bool {
            windowID == target || sheetParentID == target || sheetIDs.contains(target)
        }
    }

    /// Whether the exact window, or a sheet on it, holds focus. Read off the
    /// main run loop; a busy app that does not answer within the budget counts
    /// as not focused.
    public static func isFocused(windowID: UInt32, owner: String) async -> Bool {
        await accessibility.run(budget: focusBudget) { deadline in
            focusedNow(windowID: windowID, owner: owner, deadline: deadline)
        } ?? false
    }

    static func focusedNow(windowID: UInt32, owner: String, deadline: DispatchTime) -> Bool {
        let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
        guard let pid = processID(windowID: windowID, owner: owner, windows: windows),
              NSWorkspace.shared.frontmostApplication?.processIdentifier == pid else { return false }
        let app = AXUIElementCreateApplication(pid)
        AXUIElementSetMessagingTimeout(app, elementTimeout)
        return element(app, kAXFocusedWindowAttribute)
            .flatMap { inputWindow($0, timeout: elementTimeout, deadline: deadline) }?.accepts(windowID) == true
    }

    /// Describes `window` as an input target: its number, the window it is a
    /// sheet on and the sheets open on it. `timeout` bounds each read and
    /// `deadline` the whole description; past it, the sheets read so far stand.
    static func inputWindow(_ window: AXUIElement, timeout: Float? = nil, deadline: DispatchTime? = nil) -> FocusedWindow? {
        guard let windowID = number(window) else { return nil }
        let bounded = boundedElement(timeout)
        let parentID = role(bounded(window)) == kAXSheetRole as String
            ? element(window, kAXParentAttribute).map(bounded).flatMap(number) : nil
        return FocusedWindow(windowID: windowID, sheetParentID: parentID,
                             sheetIDs: sheets(of: window, bounded: bounded, deadline: deadline).compactMap(number))
    }

    private static func boundedElement(_ timeout: Float?) -> (AXUIElement) -> AXUIElement {
        { element in
            if let timeout { AXUIElementSetMessagingTimeout(element, timeout) }
            return element
        }
    }

    private static func passed(_ deadline: DispatchTime?) -> Bool {
        deadline.map { DispatchTime.now() >= $0 } ?? false
    }

    static func element(_ element: AXUIElement, _ attribute: String) -> AXUIElement? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, attribute as CFString, &value) == .success,
              let value, CFGetTypeID(value) == AXUIElementGetTypeID() else { return nil }
        return unsafeBitCast(value, to: AXUIElement.self)
    }

    private static func role(_ element: AXUIElement) -> String? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, kAXRoleAttribute as CFString, &value) == .success else { return nil }
        return value as? String
    }

    private static func number(_ element: AXUIElement) -> UInt32? {
        var number: CGWindowID = 0
        return dialogWindowNumber(element, &number) == .success ? number : nil
    }

    private static func sheets(of window: AXUIElement, bounded: (AXUIElement) -> AXUIElement = { $0 },
                               deadline: DispatchTime? = nil) -> [AXUIElement] {
        var value: CFTypeRef?
        guard !passed(deadline),
              AXUIElementCopyAttributeValue(window, kAXChildrenAttribute as CFString, &value) == .success,
              let children = value as? [AXUIElement] else { return [] }
        var sheets: [AXUIElement] = []
        for child in children {
            if passed(deadline) { break }
            if role(bounded(child)) == kAXSheetRole as String { sheets.append(child) }
        }
        return sheets
    }

    /// Unbounded for the one-shot `--raise-dialog` command, whose process does
    /// nothing else; bounded by `deadline` while the phone controls a window.
    public static func raise(windowID: UInt32, owner: String) -> Result {
        raise(windowID: windowID, owner: owner, deadline: nil)
    }

    static func raise(windowID: UInt32, owner: String, deadline: DispatchTime?) -> Result {
        let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
            as? [[String: Any]] ?? []
        guard let pid = processID(windowID: windowID, owner: owner, windows: windows) else { return .gone }
        guard AXIsProcessTrusted() else { return .unavailable }
        let bounded = boundedElement(deadline == nil ? nil : elementTimeout)
        let app = bounded(AXUIElementCreateApplication(pid))
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &value) == .success,
              let elements = value as? [AXUIElement] else { return .unavailable }
        // A sheet is raised with the window it belongs to.
        var target = elements.first(where: { number($0) == windowID })
        if target == nil {
            for element in elements where !passed(deadline) {
                if sheets(of: bounded(element), bounded: bounded, deadline: deadline).contains(where: { number($0) == windowID }) {
                    target = element
                    break
                }
            }
        }
        guard let window = target, !passed(deadline) else { return .unavailable }
        // Activation alone can select a different window of the same app.
        _ = NSRunningApplication(processIdentifier: pid)?.activate(options: [])
        guard AXUIElementPerformAction(window, kAXRaiseAction as CFString) == .success else { return .unavailable }
        _ = AXUIElementSetAttributeValue(app, kAXFocusedWindowAttribute as CFString, window)
        return .raised
    }
}
