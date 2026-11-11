import Foundation

public enum ActivityKind: String, Sendable {
    case userMessage, agentMessage, approval, commandExecution, fileChange, webSearch
    case mcpToolCall, dynamicToolCall, collabAgentToolCall, subAgentActivity, functionCallOutput
    case imageView, imageGeneration, sleep, reasoning, plan, hookPrompt, enteredReviewMode, exitedReviewMode
    case contextCompaction, providerSwitch, taskNotification, error, unknown
}

public enum ActivityState: String, Sendable {
    case started, streaming, waiting, completed, failed, interrupted, unknown
    public var isRunning: Bool { self == .started || self == .streaming || self == .waiting }

    static func prepare(_ item: ReadItem) -> Self {
        let state = Self(rawValue: item.state) ?? .unknown
        guard state != .interrupted else { return state }
        if item.type == "commandExecution", state.isRunning { return state }
        let payload = item.payload ?? [:]
        let resultError: Bool
        if case .object(let result) = payload["result"] { resultError = result["isError"]?.bool == true }
        else { resultError = false }
        let hasError = payload["error"].map { $0 != .null && $0.string != "" } == true
        let imageError = item.type == "imageGeneration" && payload["failure"].map { $0 != .null && $0.string != "" } == true
        let commandError = item.type == "commandExecution" && payload["exitCode"]?.number.map { $0 != 0 } == true
        let kind = ActivityKind(rawValue: item.type) ?? .unknown
        let messageNotSent = ActivityAction.ownedTool(kind, payload) == .messageChat
            && ActivityAction.messageOutcome(item, payload).failed
        return item.type == "error" || messageNotSent || resultError || hasError || imageError || commandError || payload["success"]?.bool == false ? .failed : state
    }
}

/// Provider-declared work, prepared once with the row. Shell text and tool names
/// remain in details; neither is evidence that an action read a file or ran tests.
public struct ActivityAction: Equatable, Sendable {
    public enum Kind: String, Sendable {
        case read, listFiles, search, command, edit, webSearch, fetch, findInPage, computer
        case ask, updateTasks, createTask, readTasks, stopTask, findTools, changeMode
        case spawnAgent, messageAgent, resumeAgent, waitAgent, stopAgent, listAgents
        case imageView, imageGeneration, wait, startReview, finishReview, instructions, result, think, tool
        case publishPage, previewPage, showVisual
        case listChats, readChat, messageChat, queueMessage, confirmMessage, waitChat, delegateTask

        var words: (present: String, past: String, verb: String, object: String, symbol: String) {
            switch self {
            case .read: ("Reading", "Read", "read", "files", "doc.text")
            case .listFiles: ("Listing", "Listed", "list", "files", "folder")
            case .search: ("Searching", "Searched", "search", "files", "magnifyingglass")
            case .command: ("Running", "Ran", "run", "command", "terminal")
            case .edit: ("Editing", "Edited", "edit", "files", "pencil")
            case .webSearch: ("Searching", "Searched", "search", "the web", "globe")
            case .fetch: ("Opening", "Opened", "open", "web page", "link")
            case .findInPage: ("Searching", "Searched", "search", "page", "doc.text.magnifyingglass")
            case .computer: ("Using", "Used", "use", "computer", "desktopcomputer")
            case .ask: ("Asking", "Asked", "ask", "a question", "questionmark.bubble")
            case .updateTasks: ("Updating", "Updated", "update", "todos", "checklist")
            case .createTask: ("Creating", "Created", "create", "task", "checklist")
            case .readTasks: ("Checking", "Checked", "check", "tasks", "checklist")
            case .stopTask: ("Stopping", "Stopped", "stop", "task", "stop.circle")
            case .findTools: ("Finding", "Found", "find", "tools", "wrench.and.screwdriver")
            case .changeMode: ("Changing", "Changed", "change", "mode", "switch.2")
            case .spawnAgent: ("Starting", "Started", "start", "agent", "sparkles.rectangle.stack")
            case .messageAgent: ("Messaging", "Messaged", "message", "agent", "text.bubble")
            case .resumeAgent: ("Resuming", "Resumed", "resume", "agent", "play.circle")
            case .waitAgent: ("Waiting for", "Waited for", "wait for", "agent", "hourglass")
            case .stopAgent: ("Stopping", "Stopped", "stop", "agent", "stop.circle")
            case .listAgents: ("Checking", "Checked", "check", "agents", "list.bullet")
            case .imageView: ("Viewing", "Viewed", "view", "image", "photo")
            case .imageGeneration: ("Generating", "Generated", "generate", "image", "wand.and.stars")
            case .wait: ("Waiting", "Waited", "wait", "", "clock")
            case .startReview: ("Starting", "Started", "start", "review", "checkmark.shield")
            case .finishReview: ("Finishing", "Finished", "finish", "review", "checkmark.shield.fill")
            case .instructions: ("Reading", "Read", "read", "instructions", "text.quote")
            case .result: ("Receiving", "Received", "receive", "tool result", "doc.plaintext")
            case .think: ("Thinking", "Thought", "think", "", "brain")
            case .tool: ("Using", "Used", "use", "tool", "puzzlepiece.extension")
            case .publishPage: ("Publishing", "Published", "publish", "page", "doc.richtext")
            case .previewPage: ("Checking", "Checked", "check", "page", "doc.text.magnifyingglass")
            case .showVisual: ("Showing", "Showed", "show", "visual", "chart.bar.doc.horizontal")
            case .listChats: ("Checking", "Checked", "check", "chats", "list.bullet")
            case .readChat: ("Reading", "Read", "read", "chat", "text.bubble")
            case .messageChat: ("Sending", "Sent", "send", "message", "paperplane")
            // The chat was busy: the message waits its turn there.
            case .queueMessage: ("Sending", "Queued", "send", "message", "paperplane")
            // The computer could not tell whether the chat received it.
            case .confirmMessage: ("Sending", "Couldn’t confirm", "confirm", "message", "paperplane")
            case .waitChat: ("Waiting for", "Waited for", "wait for", "reply", "hourglass")
            // Handing off queues the task in its chat; it is not yet running.
            case .delegateTask: ("Handing off", "Handed off", "hand off", "task", "arrow.triangle.branch")
            }
        }
    }
    public let kind: Kind
    public let paths: [String]
    public let target: String?
    public let description: String?
    public var symbol: String { kind.words.symbol }

    public func label(state: ActivityState) -> String {
        let words = kind.words
        let object = target ?? words.object
        let verb: String
        switch state {
        case .started, .streaming, .waiting: verb = words.present
        case .completed: verb = words.past
        case .failed: verb = "Couldn’t " + words.verb
        case .interrupted: return "Stopped · " + joined(words.present, object)
        case .unknown: return joined(words.object.capitalized, target ?? "")
        }
        let label = joined(verb, object)
        if kind == .command, state.isRunning, let description { return label + " · " + description }
        return label
    }

    private func joined(_ verb: String, _ object: String) -> String { object.isEmpty ? verb : verb + " " + object }

    static func prepare(_ item: ReadItem, kind: ActivityKind) -> [Self] {
        let payload = item.payload ?? [:]
        func object(_ value: ThreadValue?) -> [String: ThreadValue] {
            if case .object(let fields) = value { return fields }; return [:]
        }
        func clean(_ text: String?) -> String? {
            guard let text else { return nil }
            let value = text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
            return value.isEmpty || value.hasPrefix("[") ? nil : String(value.prefix(256))
        }
        func action(_ kind: Kind, _ fields: [String: ThreadValue] = [:], target: String? = nil) -> Self {
            let sourcePaths = fields["paths"]?.array?.compactMap(\.string) ?? fields["path"]?.string.map { [$0] } ?? fields["diffs"]?.array?.compactMap { object($0)["path"]?.string } ?? []
            func hasKnownPath(_ path: String) -> Bool { !path.isEmpty && !path.contains("[") && !path.contains("\0") }
            let incomplete = fields["pathsTruncated"]?.bool == true || sourcePaths.count > 32 || sourcePaths.contains { !hasKnownPath($0) }
            var seen: Set<String> = []
            let paths = sourcePaths.prefix(32).filter { hasKnownPath($0) && seen.insert($0).inserted }
            let label: String?
            if let target { label = clean(target) }
            else if kind == .search, let query = clean(fields["query"]?.string) { label = "for " + query }
            else if kind == .webSearch, let query = clean(fields["query"]?.string) { label = "the web for " + query }
            else if kind == .findInPage, let query = clean(fields["query"]?.string ?? fields["pattern"]?.string) { label = "page for " + query }
            else if kind == .fetch { label = fields["url"]?.string.flatMap { URLComponents(string: $0)?.host }.flatMap(clean) }
            else if [.read, .edit, .listFiles].contains(kind), !paths.isEmpty {
                label = paths.count == 1 ? clean(FileChangeSummary.filename(paths[0])) : "\(paths.count) files"
            } else { label = nil }
            return Self(kind: kind, paths: incomplete ? [] : paths, target: incomplete ? nil : label,
                        description: clean(fields["description"]?.string))
        }
        let declared = object(payload["toolAction"])
        if let kind = declared["kind"]?.string.flatMap(Kind.init(rawValue:)) { return [action(kind, declared)] }
        switch kind {
        case .commandExecution:
            let declared = (payload["commandActions"]?.array ?? []).prefix(32).map { value -> Self in
                let fields = object(value)
                let kind: Kind
                switch fields["type"]?.string {
                case "read": kind = .read
                case "listFiles": kind = .listFiles
                case "search": kind = .search
                default: kind = .command
                }
                return action(kind, fields)
            }
            return declared.isEmpty || payload["commandActionsTruncated"]?.bool == true ? [action(.command)] : declared
        case .fileChange: return [action(.edit, payload)]
        case .webSearch:
            var fields = object(payload["action"])
            fields["query"] = payload["query"] ?? fields["query"] ?? fields["queries"]?.array?.first
            let operation: Kind
            switch fields["type"]?.string {
            case "openPage": operation = .fetch
            case "findInPage": operation = .findInPage
            default: operation = .webSearch
            }
            return [action(operation, fields)]
        case .mcpToolCall, .dynamicToolCall:
            if payload["server"]?.string == "cua_repl" || payload["tool"]?.string == "wonder_computer_use" { return [action(.computer)] }
            // The Mac recognized a page published for the reader, or a page
            // the agent rendered to check its work.
            if payload["tool"]?.string == "wonder_show_visual" { return [action(.showVisual)] }
            // Cupola's own thread tools, as the host registered them: Codex and
            // Claude report them as dynamic tools without a server, OpenCode and
            // Cursor under the Cupola server. The verb turns past tense only on a
            // confirmed result; arguments name no resolved chat, so none is shown.
            if let operation = ownedTool(kind, payload) {
                return [action(operation == .messageChat ? messageOutcome(item, payload).kind : operation)]
            }
            if payload["htmlRender"] != nil { return [action(.publishPage)] }
            if payload["mediaInDetails"]?.bool == true { return [action(.previewPage)] }
            // App Server appContext is the verified connector identity. An arbitrary
            // MCP server/tool name is not a product name or evidence of its effects.
            let app = object(payload["appContext"])
            let name = app["connectorId"]?.string?.isEmpty == false ? clean(app["appName"]?.string) : nil
            return [action(.tool, target: name)]
        case .collabAgentToolCall:
            let operation: Kind
            switch payload["tool"]?.string {
            case "spawnAgent": operation = .spawnAgent
            case "sendInput", "sendMessage", "followupTask": operation = .messageAgent
            case "resumeAgent": operation = .resumeAgent
            case "wait": operation = .waitAgent
            case "closeAgent", "interruptAgent": operation = .stopAgent
            case "listAgents": operation = .listAgents
            default: operation = .tool
            }
            let name = clean(payload["agentNickname"]?.string ?? payload["agentRole"]?.string)
            return [action(operation, target: name.map { "@" + $0 })]
        case .subAgentActivity: return [] // A task notification is not another tool action.
        case .imageView: return [action(.imageView)]
        case .imageGeneration: return [action(.imageGeneration)]
        case .sleep: return [action(.wait)]
        case .reasoning: return [action(.think)]
        case .enteredReviewMode: return [action(.startReview)]
        case .exitedReviewMode: return [action(.finishReview)]
        case .hookPrompt: return [action(.instructions)]
        case .functionCallOutput: return [action(.result)]
        case .userMessage, .agentMessage, .approval, .plan, .contextCompaction, .providerSwitch, .taskNotification, .error, .unknown: return []
        }
    }

    /// Cupola's own thread tools, as the host registered them: Codex and
    /// Claude report them as dynamic tools without a server, OpenCode and
    /// Cursor under the Cupola server.
    static func ownedTool(_ kind: ActivityKind, _ payload: [String: ThreadValue]) -> Kind? {
        let owned = kind == .dynamicToolCall ? (payload["server"]?.string ?? "").isEmpty
            : kind == .mcpToolCall && payload["server"]?.string == "Cupola"
        return owned ? ownedTools[payload["tool"]?.string ?? ""] : nil
    }

    /// What a finished send reports about its message (`report_delivery` in
    /// thread_tools.rs): only "delivered" is a sent message. "pending" waits
    /// its turn; "failed" and "uncertain" (or no outcome at all) are not
    /// confirmed, so the row fails rather than claiming the message arrived.
    static func messageOutcome(_ item: ReadItem, _ payload: [String: ThreadValue]) -> (kind: Kind, failed: Bool) {
        guard item.state == ActivityState.completed.rawValue, payload["success"]?.bool != false else { return (.messageChat, false) }
        let text = (payload["result"] ?? payload["contentItems"])?.toolOutputText ?? ""
        let result = text.data(using: .utf8).flatMap { try? JSONDecoder().decode(ThreadValue.self, from: $0) }
        guard case .object(let fields) = result else { return (.confirmMessage, true) }
        switch fields["outcome"]?.string {
        case "delivered": return (.messageChat, false)
        case "pending": return (.queueMessage, false)
        case "failed": return (.messageChat, true)
        default: return (.confirmMessage, true)
        }
    }

    /// Protocol IDs stay as the host registers them (`thread_tools.rs`).
    private static let ownedTools: [String: Kind] = [
        "wonder_thread_list": .listChats, "wonder_thread_read": .readChat, "wonder_thread_send": .messageChat,
        "wonder_thread_wait": .waitChat, "wonder_delegate": .delegateTask,
    ]

    static func summary(_ kind: Kind, count: Int, paths: Set<String>, hasUnknownPaths: Bool) -> String {
        switch kind {
        case .read, .edit:
            let verb = kind == .read ? "Read" : "Edited"
            guard !hasUnknownPaths, !paths.isEmpty else { return verb + " files" }
            return "\(verb) \(paths.count) \(paths.count == 1 ? "file" : "files")"
        case .command: return "Ran \(count) \(count == 1 ? "command" : "commands")"
        case .webSearch: return count == 1 ? "Searched the web" : "Searched the web \(count) times"
        case .tool: return "Used \(count) \(count == 1 ? "tool" : "tools")"
        default:
            let label = Self(kind: kind, paths: [], target: nil, description: nil).label(state: .completed)
            return count == 1 ? label : label + " \(count) times"
        }
    }
}
