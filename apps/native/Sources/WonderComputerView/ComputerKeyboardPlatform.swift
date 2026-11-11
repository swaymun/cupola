import Foundation

/// Which operating system's shortcut keys a computer's host uses.
public enum ComputerKeyboardPlatform: String, CaseIterable, Sendable {
    case macos, windows, linux

    /// Nil is the legacy Mac host; an unrecognized platform gets the ordinary
    /// (Linux-style) keyboard, never Mac shortcuts.
    public init(host: String?) {
        self = Self(rawValue: (host ?? "macos").lowercased()) ?? .linux
    }
}
