import Foundation

/// A native agent on a paired Mac, as `GET /api/v1/providers` reports it:
/// what is installed and signed in (the Mac's own status check) and what
/// Wonder runs.
public struct MacProviderStatus: Decodable, Equatable, Sendable, Identifiable {
    public let id: String
    public let running: Bool
    /// Earlier Macs do not report startup yet; absence is unknown.
    public let starting: Bool?
    public let incompatible: Bool
    public let updatePending: Bool
    public let detail: String?
    /// signedIn, signedOut, needsSubscription, unsupported, notInstalled or unavailable.
    public let state: String
    public let version: String
    public let account: String
    /// An agent of this provider is working, so Reconnect is refused.
    public let working: Bool
    /// Whether a newer version exists and who installs it; Codex and Claude
    /// only, and absent from earlier Macs.
    public let update: MacProviderUpdate?
    /// Chats work, but something is off: an untested newer version, or
    /// optional features turned off until Cupola is updated. Absent from earlier Macs.
    public let warning: String?

    public init(id: String, running: Bool = true, starting: Bool? = nil, incompatible: Bool = false, updatePending: Bool = false,
                detail: String? = nil, state: String, version: String = "", account: String = "", working: Bool = false,
                update: MacProviderUpdate? = nil, warning: String? = nil) {
        self.warning = warning
        self.id = id; self.running = running; self.incompatible = incompatible; self.updatePending = updatePending
        self.starting = starting
        self.detail = detail; self.state = state; self.version = version; self.account = account; self.working = working
        self.update = update
    }

    public var name: String { family?.title ?? id }
    public var family: AgentFamily? { AgentFamily(rawValue: id) }
}

/// A provider's `update` (`providerUpdate` in the HTTP contract): a newer
/// version, who installs it ("cupola", "chatgpt", "npm", "homebrew" or
/// "manual"), whether the Mac can run that installer
/// (`POST /api/v1/providers/{id}/update`) and the last run's result
/// ("idle", "updating", "ready", "succeeded", "unchanged" or "failed").
public struct MacProviderUpdate: Decodable, Equatable, Sendable {
    public let owner: String
    public let available: Bool
    /// The version the update check compared: for Claude, Cupola's runtime. Absent from earlier Macs.
    public let installedVersion: String?
    public let latestVersion: String?
    public let canUpdate: Bool
    public let cupolaUpdateNeeded: Bool
    public let status: String
    public let message: String?

    public init(owner: String, available: Bool = false, installedVersion: String? = nil, latestVersion: String? = nil,
                canUpdate: Bool = false, cupolaUpdateNeeded: Bool = false, status: String = "idle", message: String? = nil) {
        self.installedVersion = installedVersion
        self.owner = owner; self.available = available; self.latestVersion = latestVersion; self.canUpdate = canUpdate
        self.cupolaUpdateNeeded = cupolaUpdateNeeded; self.status = status; self.message = message
    }
}

public struct MacProvidersResponse: Decodable, Sendable {
    public let providers: [MacProviderStatus]
    /// Cupola on the Mac: its version, the newest one and the update command.
    /// A Mac that sends none is older than this app needs.
    public let host: MacHostCupola?

    public init(providers: [MacProviderStatus], host: MacHostCupola? = nil) {
        self.providers = providers; self.host = host
    }
}

/// Cupola itself on the Mac (`host` in the providers response).
public struct MacHostCupola: Decodable, Equatable, Sendable {
    public let version: String
    public let latestVersion: String?
    public let available: Bool
    /// What the owner runs on the Mac to update Cupola.
    public let command: String?

    public init(version: String, latestVersion: String? = nil, available: Bool = false, command: String? = nil) {
        self.version = version; self.latestVersion = latestVersion; self.available = available; self.command = command
    }

    /// The Homebrew command for a Mac that does not report one.
    public static let defaultCommand = "brew upgrade --cask cupola"

    /// "Cupola 1.0.256 · Latest 1.0.300", or "· Up to date" when it is the newest.
    public var versions: String {
        var text = "Cupola \(version)"
        if let latestVersion, available { text += " · Latest \(latestVersion)" }
        else if latestVersion != nil { text += " · Up to date" }
        return text
    }

    /// The Mac's own words for an update, or nil when Cupola is current.
    public func updateNote(macName: String) -> String? {
        available ? "Update Cupola on \(macName): \(command ?? Self.defaultCommand)" : nil
    }

    /// What the app says about a Mac that sends no `host`: it is older than
    /// this app needs. The command is always the Homebrew one.
    public static func olderMacNote(macName: String) -> String {
        "Update Cupola on \(macName): \(defaultCommand)"
    }
}

/// One provider row on the phone, in the words the host's wonder providers command
/// uses. The phone offers only Reconnect, Update (run the installer that owns
/// the provider) or Update now (switch to an installed update); signing in
/// and out stay on the Mac.
public struct MacProviderRow: Equatable, Sendable {
    /// `update` switches to an update already installed (a reconnect);
    /// `install` runs the provider's installer on the Mac.
    public enum Action: Equatable, Sendable { case reconnect, update, install, checkAgain }
    public let status: String
    public let detail: String?
    public let needsAttention: Bool
    public let action: Action?
    /// Why the action can't run now, shown under the row.
    public let blocked: String?
    /// A newer version, the installer's progress or its last result.
    public let updateNote: String?
    /// The installer is running on the Mac; the phone checks until it ends.
    public let updating: Bool
    /// The last update failed; `updateNote` says why.
    public let updateFailed: Bool
    /// "Version 0.166.0 · Latest 0.170.0", "Version 0.166.0 · Up to date", or just the version.
    public let versions: String?
    /// Chats work; something is off (see `MacProviderStatus.warning`).
    public let warning: String?

    public init(_ provider: MacProviderStatus, macName: String) {
        let name = provider.name
        let version = provider.version.isEmpty ? nil : "Version \(provider.version)"
        let codex = provider.id == "codex" // provider identity: setup wording
        var status: String, detail: String?, attention = true, action: Action?
        switch provider.state {
        case "signedIn" where provider.incompatible:
            status = "Signed in, but not connected"
            detail = "Cupola is still using an earlier check of \(name). Reconnect to use the installed version."
            action = .reconnect
        case "signedIn" where provider.starting == true && !provider.running,
             "installed" where provider.starting == true && !provider.running:
            status = "Connecting…"
            detail = version
            attention = false
        case "signedIn" where !codex && !provider.running:
            status = "Signed in, but not running"
            detail = "Cupola couldn’t keep \(name) running."
            action = .reconnect
        case "signedIn" where provider.updatePending:
            status = "Update ready"
            detail = "\(name) \(provider.version.isEmpty ? "" : provider.version + " ")is installed. Cupola switches to it when agents finish."
            attention = false
            action = .update
        case "installed" where provider.family == .opencode && !provider.running: // provider identity: setup wording
            status = "Installed"
            detail = "Reconnect to start OpenCode on \(macName)."
            action = .reconnect
        case "installed" where provider.family == .opencode: // provider identity: setup wording
            // OpenCode has no Wonder sign-in; its models come from its own setup.
            status = "Installed"
            detail = version
            attention = false
        case "notInstalled" where provider.family == .opencode: // provider identity: setup wording
            status = "Not installed"
            detail = "To use OpenCode in Projects, install it on \(macName): in Terminal, run curl -fsSL https://opencode.ai/install | bash."
        case "signedIn":
            status = provider.account.isEmpty ? "Signed in" : "Signed in with \(provider.account)"
            detail = version
            attention = false
        case "signedOut" where provider.family == .cursor: // provider identity: native sign-in
            status = "Not signed in"
            detail = "Sign in to Cursor on \(macName) with agent login."
            action = .checkAgain
        case "notInstalled" where provider.family == .cursor: // provider identity: setup wording
            status = "Not installed"
            detail = "To use Cursor in Projects, install Cursor Agent on \(macName): in Terminal, run curl https://cursor.com/install -fsS | bash, then agent login."
        case "signedOut":
            status = "Not signed in"
            detail = "Sign in to \(name) on \(macName) with cupola providers login \(provider.id)."
            action = .checkAgain
        case "needsSubscription":
            status = "Needs a Claude subscription"
            detail = "Cupola uses a Claude Pro, Max, Team or Enterprise plan. Sign in with that account on \(macName)."
        case "unsupported" where codex:
            // The Mac knows which side is behind (an older Codex, or a Cupola
            // that hasn't caught up); an earlier Mac sends no advice.
            status = "This Codex version isn’t supported"
            detail = (version.map { $0 + ". " } ?? "")
                + (provider.detail ?? "Update ChatGPT on \(macName). Cupola starts the new version on its own.")
        case "unsupported":
            status = "\(name) runtime needs an update"
            detail = "Install the latest Cupola on \(macName)."
        case "notInstalled" where codex:
            status = "Not installed"
            detail = "Codex is not installed on \(macName). Run cupola doctor on that computer for installation steps."
        case "notInstalled":
            status = "Not installed"
            detail = "Cupola’s Claude runtime is missing. Reinstall Cupola on \(macName)."
        default:
            status = "Status unavailable"
            detail = "Cupola couldn’t read \(name)’s status on \(macName)."
            action = .checkAgain
        }
        var note: String?, updating = false, failed = false
        // Updates concern a provider that runs; any other row's own action comes first.
        if let update = provider.update, action == nil,
           ["signedIn", "installed"].contains(provider.state), !provider.updatePending {
            let newer = update.latestVersion.map { "\(name) \($0)" } ?? "A newer \(name)"
            switch update.status {
            case "updating":
                note = "Updating \(name) on \(macName)…"
                updating = true
            case "failed":
                note = update.message ?? "\(name) wasn’t updated. Cupola keeps using the current version."
                failed = true
                if update.canUpdate { action = .install }
            case "succeeded", "unchanged", "ready":
                note = update.message
            default:
                guard update.available else { break }
                if update.cupolaUpdateNeeded {
                    note = "\(newer) needs a newer Cupola. Update Cupola on \(macName)."
                } else if update.canUpdate {
                    note = "Update available: \(newer)."
                    action = .install
                } else if update.owner == "chatgpt" {
                    note = "Update available: \(newer). ChatGPT installs it on its own."
                } else {
                    note = "Update available: \(newer). Update it on \(macName) with the app or tool that installed it."
                }
            }
        }
        // The installed version has its own line; a detail that only repeats it goes.
        if detail == version { detail = nil }
        var versions = version
        if let update = provider.update, let latest = update.latestVersion {
            let installed = update.installedVersion ?? provider.version
            if !installed.isEmpty { versions = "Version \(installed)" + (update.available ? " · Latest \(latest)" : " · Up to date") }
        }
        self.versions = versions
        self.warning = ["signedIn", "installed"].contains(provider.state) ? provider.warning : nil
        self.status = status
        self.detail = detail
        self.needsAttention = attention
        self.action = action
        self.updateNote = note
        self.updating = updating
        self.updateFailed = failed
        self.blocked = (action == .reconnect || action == .update) && provider.working
            ? "An agent is working. Try again after it finishes." : nil
    }

    /// The action's button title.
    public static func title(_ action: Action) -> String {
        switch action {
        case .reconnect: "Reconnect"
        case .update: "Update now"
        case .install: "Update"
        case .checkAgain: "Check again"
        }
    }
}

/// The Claude provider's Claude app setting (`GET/PUT /api/v1/settings/claude-app`),
/// off by default. On: the Mac adds the Claude chats Cupola starts to the
/// Claude app and closes the app's copy of a chat before sending to it. Off:
/// Cupola never opens or stops the Claude app; a chat open there waits until
/// it's closed there or the owner takes over with Send.
public struct ClaudeAppSettings: Codable, Equatable, Sendable {
    public static let path = "/api/v1/settings/claude-app"
    public var enabled: Bool
    /// The Claude app is installed on the Mac; without it the setting does nothing.
    public var appInstalled: Bool

    public init(enabled: Bool, appInstalled: Bool) {
        self.enabled = enabled; self.appInstalled = appInstalled
    }

    public static let title = "Add chats to the Claude app"

    /// Under the switch: what it does, and how to continue a chat on the
    /// computer without it.
    public func explanation(computer: String) -> String {
        let resume = "To continue a chat in Terminal, copy its claude --resume command from the chat’s details (Continue on computer)."
        guard appInstalled else { return "The Claude app isn’t installed on \(computer). \(resume)" }
        return "Cupola adds the Claude chats it starts to the Claude app on \(computer) and closes a chat there before sending to it. "
            + "Off, Cupola never opens or closes the Claude app; a chat open there waits until you close it or take over. \(resume)"
    }

    public static func body(enabled: Bool) throws -> Data {
        try JSONEncoder().encode(["enabled": enabled])
    }
}

extension Array where Element == ProjectFamilyAvailability {
    /// Whether agent pickers list `family`: the Mac listed it and did not find
    /// it uninstalled. A signed-out, recovering or unchecked provider stays
    /// listed (as unavailable when it can't be chosen); an uninstalled one is
    /// left out, and Settings shows how to install it.
    public func lists(_ family: AgentFamily) -> Bool {
        first { $0.family == family }.map { $0.installed != false } ?? false
    }

    /// The Mac's last check found `family` not installed.
    public func isNotInstalled(_ family: AgentFamily) -> Bool {
        first { $0.family == family }?.installed == false
    }
}
