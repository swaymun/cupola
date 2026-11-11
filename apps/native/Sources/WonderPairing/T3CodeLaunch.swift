import Foundation

/// T3 Code's connection, as the Mac reports it (`GET /api/v1/t3-code`). A
/// refusal decodes as `detail` alone, in the Mac's words.
public struct T3CodeStatus: Decodable, Sendable, Equatable {
    public var installed: Bool?
    public var running: Bool?
    /// False after a recent failed connection; nil when reachability is unknown.
    public var reachable: Bool?
    /// "disconnected", "waiting" (for approval in the Mac's browser), "connected" or "expired".
    public var state: String?
    public var error: String?
    /// Command approval failed; browser approval always requires an explicit choice.
    public var pairingFailure: String?
    public var detail: String?
    /// With "expired": the owner removed Cupola in T3 Code, so Cupola doesn't
    /// sign in again until the owner reconnects. Older Macs omit it.
    public var revoked: Bool?
    /// The providers Start in T3 Code runs: those whose T3 Code threads
    /// Cupola shows in Chats. Macs that omit it start none here.
    public var launchProviders: [AgentFamily]?
    public var launchCatalog: T3CodeCatalog?
    public var launchCatalogError: String?

    public init(state: String? = nil, revoked: Bool? = nil, launchProviders: [AgentFamily]? = nil) {
        self.state = state; self.revoked = revoked; self.launchProviders = launchProviders
    }

    /// The owner removed Cupola in T3 Code: Settings asks whether to reconnect.
    public var wasRemoved: Bool { state == "expired" && revoked == true }
}

/// Read from this computer's connected T3 server, never the native host catalog.
public struct T3CodeCatalog: Decodable, Sendable, Equatable {
    public struct Provider: Decodable, Sendable, Equatable {
        public let instanceId: String
        public let family: AgentFamily
        public let name: String
        public let runtimeModes: [String]
        public let omittedModels: Int?
        public let models: [AgentOptions.Model]
    }
    public let providers: [Provider]
    public var models: [AgentOptions.Model] { providers.flatMap(\.models) }
    public var families: [AgentFamily] { AgentFamily.allCases.filter { family in providers.contains { $0.family == family && !$0.models.isEmpty } } }
    public func provider(for model: String?) -> Provider? { providers.first { $0.models.contains { $0.id == model } } }
    /// Keep a bounded set of search results and the selected model. Fresh
    /// server reads remain authoritative for providers and permission modes.
    public func mergingSearch(_ search: Self, selected: String?) -> Self {
        Self(providers: providers.map { provider in
            let matches = search.providers.first { $0.instanceId == provider.instanceId }?.models ?? []
            var models = matches + provider.models.filter { old in !matches.contains { $0.id == old.id } }
            if models.count > 128 {
                let retained = models.first { $0.id == selected }
                models = Array(models.prefix(127))
                if let retained, !models.contains(where: { $0.id == retained.id }) { models.append(retained) }
            }
            return Provider(instanceId: provider.instanceId, family: provider.family, name: provider.name,
                            runtimeModes: provider.runtimeModes, omittedModels: provider.omittedModels, models: models)
        })
    }
    public func nativeModel(_ model: String?) -> String? {
        guard let model, let provider = provider(for: model) else { return nil }
        return String(model.dropFirst("t3:\(provider.instanceId):".count))
    }
    /// T3 Code's own order, safest first: Supervised, Auto-accept edits, Auto,
    /// Full access. A mode it adds later follows them.
    public static func permissionOrder(_ modes: [String]) -> [String] {
        let known = ["approval-required", "auto-accept-edits", "auto", "full-access"]
        return modes.enumerated().sorted { left, right in
            let (a, b) = (known.firstIndex(of: left.element) ?? known.count, known.firstIndex(of: right.element) ?? known.count)
            return a == b ? left.offset < right.offset : a < b
        }.map(\.element)
    }
    public static func permissionTitle(_ mode: String) -> String {
        switch mode {
        case "approval-required": "Supervised"
        case "auto-accept-edits": "Auto-accept edits"
        case "auto": "Auto"
        case "full-access": "Full access"
        default: mode
        }
    }
}

/// Settings → Connected apps → T3 Code, in words for the owner.
public enum T3CodeSettingsText {
    public static func label(_ status: T3CodeStatus, computer: String) -> String {
        if status.pairingFailure != nil { return "Couldn’t connect T3 Code" }
        if status.state == "connected", status.running == false { return "T3 Code not running" }
        if status.state == "connected", status.reachable == false { return "T3 Code not reachable" }
        return switch status.state {
        case "connected": "Connected"
        case "waiting": "Approve Cupola in your browser on \(computer)"
        case "expired" where status.revoked == true: "Removed in T3 Code"
        case "expired": "Sign-in expired"
        default: "Not connected"
        }
    }

    /// Under the label: what connecting does, and where approvals happen.
    public static func footer(_ status: T3CodeStatus, computer: String) -> String {
        let sends = "Messages to threads open in T3 Code go into them there, without stopping them."
        let approvals = "Approve T3 Code’s permission requests in T3 Code on \(computer)."
        if status.wasRemoved {
            return "You removed Cupola from T3 Code’s authorized clients on \(computer), so Cupola stopped signing in there. Until you reconnect, messages to threads open in T3 Code wait."
        }
        if status.running == false { return "Open T3 Code on \(computer), then check again." }
        if status.reachable == false { return "T3 Code isn’t answering on \(computer). Check that it’s open, then try again." }
        if status.state == "expired" { return "Authorize Cupola again in T3 Code on \(computer). Your unsent messages stay saved." }
        if status.state == "connected" { return "\(sends) \(approvals)" }
        return "\(sends) Cupola signs in to T3 Code on \(computer) and keeps that sign-in current."
    }

    /// The connect button's title for a status that isn't connected or waiting.
    public static func connectTitle(_ status: T3CodeStatus) -> String {
        if status.pairingFailure != nil { return "Try again" }
        if status.wasRemoved { return "Reconnect T3 Code" }
        return status.state == "expired" ? "Connect again" : "Connect T3 Code"
    }

    /// Asked before reconnecting after the owner removed Cupola in T3 Code.
    public static let reconnectQuestion = "Reconnect T3 Code?"

    public static func reconnectMessage(computer: String) -> String {
        "Cupola signs in to T3 Code on \(computer) again and shows up in its authorized clients. Messages to threads open there go into them again."
    }
}

/// New chat's "Start in T3 Code": the Mac starts the thread in T3 Code, in
/// the T3 project for the Project's folder, with the typed first message
/// (`POST /api/v1/projects/{id}/t3-code/threads`), on exactly the chosen
/// provider and model (or refuses, saying why). T3 Code has no retry key for
/// a launch, so a lost answer is reported and never sent again by itself.
/// With the device and the draft's request ID the Mac answers the thread's
/// Cupola conversation (`T3CodeLaunched`), which New chat opens.
public struct T3CodeLaunchRequest: Codable, Equatable, Sendable {
    public let title: String
    public let message: String
    public let folderId: String?
    /// Starts in T3 Code's plan mode; omitted otherwise.
    public let planMode: Bool?
    public let provider: AgentFamily?
    public let model: String?
    public let providerInstanceId: String?
    public let effort: String?
    public let serviceTier: String?
    public let runtimeMode: String?
    public let deviceId: String?
    /// The draft's request ID: asking again answers the same conversation.
    public let clientMessageId: String?

    public init(message: String, folderId: String?, planMode: Bool = false, provider: AgentFamily? = nil,
                model: String? = nil, deviceId: String? = nil, clientMessageId: String? = nil,
                providerInstanceId: String? = nil, effort: String? = nil, serviceTier: String? = nil, runtimeMode: String? = nil) {
        self.title = Self.title(for: message)
        self.message = message
        self.folderId = folderId
        self.planMode = planMode ? true : nil
        self.provider = provider
        self.model = provider == nil ? nil : model
        self.providerInstanceId = providerInstanceId
        self.effort = effort; self.serviceTier = serviceTier; self.runtimeMode = runtimeMode
        self.deviceId = deviceId
        self.clientMessageId = deviceId == nil ? nil : clientMessageId
    }

    /// As the Mac titles a thread from its first message: the first line with
    /// words, trimmed, at most 80 characters (and the 200 bytes the launch takes).
    public static func title(for message: String) -> String {
        let line = message.split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .first { !$0.isEmpty } ?? "New thread"
        var scalars = String.UnicodeScalarView(line.unicodeScalars.prefix(80))
        while String(scalars).utf8.count > 200 { scalars.removeLast() }
        return String(scalars).trimmingCharacters(in: .whitespaces)
    }

    public func body() throws -> Data { try JSONEncoder().encode(self) }
}

/// The Mac's answer to a launch: T3 Code's thread and, for a request with
/// its device and request ID, the Cupola conversation New chat opens. Its
/// agent's session joins that same conversation once T3 Code starts it.
public struct T3CodeLaunched: Decodable, Equatable, Sendable {
    public let threadId: String?
    public let conversationId: String?

    public init(threadId: String? = nil, conversationId: String? = nil) {
        self.threadId = threadId; self.conversationId = conversationId
    }
}

public enum T3CodeLaunch {
    /// Offered when this computer is connected to T3 Code, the Project has a
    /// folder for T3 Code to open, and T3 Code runs the chosen provider
    /// where Cupola can follow it (`launchProviders`).
    public static func isOffered(_ status: T3CodeStatus?, project: ProjectSummary?, family: AgentFamily?) -> Bool {
        guard status?.state == "connected", project?.primaryFolder != nil else { return false }
        return runs(family, status: status)
    }

    /// Whether this computer starts `family`'s threads in T3 Code.
    public static func runs(_ family: AgentFamily?, status: T3CodeStatus?) -> Bool {
        guard let family else { return false }
        return status?.launchProviders?.contains(family) == true
    }

    /// What keeps a draft that chose T3 Code from being sent now; nil when
    /// it can start there. A draft that chose T3 Code only ever starts there:
    /// while T3 Code is unavailable Send waits, and never starts it in Cupola.
    public static func blockedReason(_ draft: NewChatDraft, status: T3CodeStatus?, project: ProjectSummary?,
                                     connected: Bool, macName: String) -> String? {
        guard draft.startsInT3Code == true else { return nil }
        if !connected { return "Can’t reach \(macName). Threads start in T3 Code only while it’s connected." }
        guard let status else { return "Checking T3 Code on \(macName)…" }
        if project?.primaryFolder == nil { return "This Project has no folder for T3 Code to open." }
        if let reason = connectionProblem(status, computer: macName) { return reason }
        if !runs(draft.family, status: status) {
            let provider = draft.family?.title ?? "This provider"
            let offered = (status.launchProviders ?? []).map(\.title)
            let fix = offered.isEmpty ? "Turn off Start in T3 Code."
                : "Choose \(offered.joined(separator: " or ")), or turn off Start in T3 Code."
            return "\(provider) threads started in T3 Code don’t show in Cupola yet. \(fix)"
        }
        return unsupportedContent(draft)
    }

    /// A saved authorization and a reachable T3 process are separate facts.
    public static func connectionProblem(_ status: T3CodeStatus?, computer: String) -> String? {
        guard let status else { return "Checking T3 Code on \(computer)…" }
        if status.state == "expired" { return "T3 Code authorization expired. Authorize Cupola again on \(computer)." }
        if status.running == false { return "T3 Code isn’t running on \(computer). Open it, then check again." }
        if status.reachable == false || status.state == "unavailable" { return "T3 Code isn’t reachable on \(computer). Check that it’s open, then try again." }
        if status.state != "connected" { return "T3 Code isn’t connected on \(computer). Connect it in Settings." }
        return nil
    }

    /// The launch for this draft: its words, its folder, the chosen provider
    /// and model (which T3 Code runs exactly, or refuses), Plan mode when the
    /// provider's Plan is the composer toggle, and the sending device and the
    /// draft's request ID, so the Mac answers the thread's conversation.
    public static func request(for draft: NewChatDraft, body: String, project: ProjectSummary?,
                               planToggle: Bool, deviceID: String? = nil, catalog: T3CodeCatalog? = nil) -> T3CodeLaunchRequest {
        T3CodeLaunchRequest(message: body, folderId: draft.folderId ?? project?.primaryFolder?.id,
                            planMode: planToggle && draft.planMode == true, provider: draft.family,
                            model: catalog?.nativeModel(draft.model) ?? draft.model, deviceId: deviceID, clientMessageId: draft.requestID,
                            providerInstanceId: catalog?.provider(for: draft.model)?.instanceId,
                            effort: draft.effort, serviceTier: draft.serviceTier, runtimeMode: draft.t3RuntimeMode)
    }

    /// What keeps this draft from starting in T3 Code, which takes only the
    /// message's words; nil when it can.
    public static func unsupportedContent(_ draft: NewChatDraft) -> String? {
        if draft.attachmentCount > 0 { return "T3 Code starts with your words only. Remove attachments and comments, or start in Cupola." }
        if !(draft.codexSkills ?? []).isEmpty { return "T3 Code starts with your words only. Remove skills, or start in Cupola." }
        return nil
    }

    /// Why the launch failed, in words for the owner: the Mac's own reason
    /// when it gave one; an unanswered request may have started the thread.
    public static func failureMessage(_ error: Error, macName: String) -> String {
        if case PairingFailure.hostMessage(_, let message) = error { return message }
        if case PairingFailure.response(let status) = error, [401, 403].contains(status) {
            return "Access has ended. Reconnect to \(macName) in Settings."
        }
        if case PairingFailure.response(404) = error {
            return "\(macName) couldn’t find this Project or its folder. Refresh the Project, then try again."
        }
        return "\(macName) didn’t confirm the new thread, and T3 Code may have started it. Check T3 Code on \(macName) before sending again."
    }

    /// The launch's answer was lost, so T3 Code may have started the thread:
    /// the Mac timed out waiting for T3 Code (504), or no reason arrived
    /// (as `failureMessage` says). A reason from the Mac means it didn't start. Sending again could
    /// start a second thread, so it waits for the owner to confirm.
    public static func mayHaveStarted(_ error: Error) -> Bool {
        switch error {
        case PairingFailure.hostMessage(let status, _): status == 504
        case PairingFailure.response(let status): ![401, 403, 404].contains(status)
        default: true
        }
    }

    /// Why sending this draft must ask first: its last launch may have
    /// started in T3 Code. Nil once it starts or T3 Code is turned off. The
    /// saved draft keeps the mark (NewChatDraftStore.save) until the launch
    /// starts; "Send again anyway" (`confirmResend`) only skips the question once.
    public static func unconfirmedLaunch(_ draft: NewChatDraft) -> String? {
        draft.startsInT3Code == true ? draft.t3LaunchUnconfirmed : nil
    }

    /// Marks a launch whose answer was lost on the draft saved now, not the
    /// one sent: New chat may have saved newer words for it (`current` or
    /// its destination's `saved` draft, same request) or moved on to another
    /// draft (`current`), which gets only the mark, never the sent words.
    public static func markingUnconfirmed(_ launched: NewChatDraft, message: String,
                                          current: NewChatDraft?, saved: NewChatDraft?) -> NewChatDraft {
        var marked = [current, saved].compactMap { $0 }.first { $0.requestID == launched.requestID } ?? current ?? launched
        marked.t3LaunchUnconfirmed = message
        return marked
    }

    /// The owner chose to send again anyway; a lost answer marks it again.
    public static func confirmResend(_ draft: inout NewChatDraft) { draft.t3LaunchUnconfirmed = nil }

    /// Heads a launch that may have started, above the Mac's guidance.
    public static let unconfirmedTitle = "Not sure it started in T3 Code"

    /// Asks before sending a launch that may have started once already.
    public static func resendWarning(macName: String) -> String {
        "T3 Code may already have started this thread. Check T3 Code on \(macName) first, or you may get two threads."
    }

    /// Started, but the Mac answered no conversation to open (it couldn't
    /// save one): the thread shows in Chats once its agent starts.
    public static func startedNotice(macName: String) -> String {
        "Started in T3 Code on \(macName). It appears in Chats shortly."
    }
}

/// Why a message to a thread open in T3 Code on the computer waits instead of
/// going into it there, from the Mac's `heldBy.t3Code` and `t3CodeReason`,
/// with what to do about it. `nil` when messages go into T3 Code's thread.
public enum T3CodeNote {
    /// A thread T3 Code started, which T3 Code runs again once it is open:
    /// until then the computer runs its replies itself (`runsWithoutT3Code`).
    public static func runsWithout(computer: String) -> String {
        "T3 Code isn’t running on \(computer), so replies here run without it and its tools. Open T3 Code to continue there."
    }

    /// A waiting message to a thread T3 Code started, which T3 Code's send
    /// can't carry ("attachments", "skills" or "model", `t3CodeCantSend`): it
    /// waits rather than running without T3 Code until it is cancelled. T3
    /// Code switches a thread's model and effort itself, so "model" means a
    /// change to another provider.
    public static func cantSend(limit: String, computer: String) -> String {
        let (what, instead) = switch limit {
        case "attachments": ("Files this large", " with smaller files")
        case "skills": ("Skills", " without skills")
        case "model": ("A switch to another provider", " with this thread’s provider")
        default: ("This message", "")
        }
        return "\(what) can’t go through T3 Code on \(computer), so this message waits. Cancel it in the queue and send it again\(instead)."
    }

    public static func text(delivery: String?, reason: String?, computer: String, canTakeOver: Bool) -> String? {
        let sendNow = canTakeOver ? " Touch and hold Send to take over." : ""
        switch (delivery, reason) {
        case ("connect", "expired"):
            return "Messages wait: Cupola’s sign-in to T3 Code ended. Connect T3 Code again in Settings.\(sendNow)"
        case ("connect", "revoked"):
            return "Messages wait: T3 Code removed Cupola’s access. Connect again in Settings.\(sendNow)"
        case ("connect", "renewing"):
            return "Messages wait while Cupola renews its sign-in to T3 Code on \(computer).\(sendNow)"
        case ("connect", "unreadable"):
            return "Messages wait: Cupola couldn’t read its T3 Code sign-in on \(computer). Connect T3 Code again in Settings.\(sendNow)"
        case ("connect", _):
            // Send now cannot stop the codex app-server T3 Code runs: a message waits for it.
            let then = canTakeOver ? "send without stopping it, or hold Send to take over." : "send into it without waiting."
            return "Open in T3 Code on \(computer). Connect T3 Code in Settings to \(then)"
        case ("notFound", "finding"):
            return "Finding this thread in T3 Code on \(computer)…"
        case ("notFound", "unreachable"):
            return "Messages wait: T3 Code on \(computer) isn’t answering. Cupola tries again shortly.\(sendNow)"
        case ("notFound", "refused"):
            return "Messages wait: T3 Code didn’t take the last one. Cupola tries again shortly.\(sendNow)"
        case ("notFound", _):
            let when = canTakeOver ? "after you close it there, or touch and hold Send to take over." : "when the reply there finishes."
            return "Messages wait: Cupola can’t tell which T3 Code thread this is. They send \(when)"
        default:
            return nil
        }
    }
}

/// The permission modes of this chat in T3 Code, reported by the computer.
/// These describe T3's replies, independently of Cupola's own access settings.
public struct T3CodePermissions: Codable, Hashable, Sendable {
    public let runtimeMode: String
    public let interactionMode: String

    public var title: String {
        switch runtimeMode {
        case "approval-required": return "Supervised"
        case "auto-accept-edits": return "Auto-accept edits"
        case "auto": return "Auto"
        case "full-access": return "Full access"
        default: return "Mode unavailable"
        }
    }

    public var detail: String {
        let text = switch runtimeMode {
        case "approval-required": "Reads allowed files without asking. Asks before edits and commands unless already allowed in T3 Code."
        case "auto-accept-edits": "Edits files in its working folder without asking. Other actions may need approval."
        case "auto": "T3 Code checks actions automatically and asks when it needs your decision."
        case "full-access": "Runs commands and changes files without routine approval. Explicit rules may still ask."
        default: "Cupola couldn’t read this chat’s permission mode. Check it in T3 Code."
        }
        return interactionMode == "plan" ? "Plan mode is on. " + text : text
    }

    public static func approvalLocation(computer: String) -> String {
        "Approve permission requests in T3 Code on \(computer)."
    }

    public static func waiting(computer: String) -> String {
        "Approve in T3 Code on \(computer)"
    }
}
