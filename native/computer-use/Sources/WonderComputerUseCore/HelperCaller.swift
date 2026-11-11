import Darwin
import Foundation

/// Which process may start the computer-use helper. The helper holds Cupola's
/// Screen Recording and Accessibility grants, so only the Cupola host may run
/// it: the bundled `Contents/MacOS/cupola` that sits beside it in the same app.
/// Another program of the same user that launches the helper directly, or from
/// a copy of it, is refused before it can read the screen or send input.
public enum HelperCaller {
    /// The host executable that belongs to a helper at
    /// `<app>/Contents/Helpers/<name>`, or nil when the helper is not inside
    /// an app bundle's `Helpers` folder.
    public static func hostExecutable(forHelperAt helper: URL) -> URL? {
        let helpers = helper.resolvingSymlinksInPath().deletingLastPathComponent()
        let contents = helpers.deletingLastPathComponent()
        guard helpers.lastPathComponent == "Helpers", contents.lastPathComponent == "Contents" else { return nil }
        return contents.appendingPathComponent("MacOS/cupola")
    }

    /// True only when the helper is bundled and `parent` is its own host.
    public static func permits(parentExecutable parent: URL?, helperAt helper: URL) -> Bool {
        guard let parent, let host = hostExecutable(forHelperAt: helper) else { return false }
        return parent.resolvingSymlinksInPath().path == host.resolvingSymlinksInPath().path
    }

    /// The executable image of a running process, or nil if it cannot be read.
    public static func executable(ofProcess pid: pid_t) -> URL? {
        var buffer = [CChar](repeating: 0, count: 4 * 1024)
        guard proc_pidpath(pid, &buffer, UInt32(buffer.count)) > 0 else { return nil }
        return URL(fileURLWithPath: String(cString: buffer))
    }

    /// Whether this process was started by its Cupola host.
    public static func currentProcessIsHostLaunched() -> Bool {
        guard let helper = Bundle.main.executableURL ?? CommandLine.arguments.first.map({ URL(fileURLWithPath: $0) }) else { return false }
        return permits(parentExecutable: executable(ofProcess: getppid()), helperAt: helper)
    }
}
