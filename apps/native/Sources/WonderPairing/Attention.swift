import Foundation
import CryptoKit

/// A decoded JSON value for structured approval requests and decisions.
public enum JSONValue: Codable, Equatable, Sendable {
    case object([String: JSONValue])
    case array([JSONValue])
    case string(String)
    case number(Double)
    case bool(Bool)
    case null

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() { self = .null }
        else if let value = try? container.decode([String: JSONValue].self) { self = .object(value) }
        else if let value = try? container.decode([JSONValue].self) { self = .array(value) }
        else if let value = try? container.decode(String.self) { self = .string(value) }
        else if let value = try? container.decode(Bool.self) { self = .bool(value) }
        else { self = .number(try container.decode(Double.self)) }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .object(let value): try container.encode(value)
        case .array(let value): try container.encode(value)
        case .string(let value): try container.encode(value)
        case .number(let value): try container.encode(value)
        case .bool(let value): try container.encode(value)
        case .null: try container.encodeNil()
        }
    }
}

/// Only a successful signed response confirms delivery. A validation rejection
/// is known not to have been forwarded; transport and no-op outcomes stay uncertain.
public enum ApprovalResolutionOutcome: Sendable {
    case confirmed, rejected, unconfirmed
}

public struct AttentionRequest: Codable, Identifiable, Sendable {
    public let approvalId: String
    public let method: String
    public let params: AttentionParams
    public let actionNonce: String
    public let conversationId: String?
    public let resolutionIdempotencyKey: String?
    public var id: String { approvalId }
    /// The host verifies Project ownership, including provider child tasks.
    /// Tool-supplied IDs and unassigned requests cannot select a conversation.
    public func belongs(to conversationID: String) -> Bool {
        guard let conversationId, !conversationId.isEmpty else { return false }
        return conversationId == conversationID
    }
    public var isQuestion: Bool { method == "item/tool/requestUserInput" }
    /// An agent asking for a token: answered on its own route, never as a normal reply.
    public var isSecretRequest: Bool { method == "wonder/requestSecret" && params.secretRequest != nil }
    public var computerAction: ComputerAction? {
        guard method == "item/tool/call", params.tool == "wonder_computer_use",
              let action = params.arguments, action.detail != nil else { return nil }
        return action
    }
    public var supportsDecision: Bool {
        !phoneChoices.isEmpty
    }
}
public struct AttentionParams: Codable, Sendable {
    public let kind: String?
    public let supportsDenyMessage: Bool?
    public let notificationQuestion: NotificationQuestion?
    public let secretRequest: SecretRequestSummary?
    public let tool: String?
    public let arguments: ComputerAction?
    public let threadId: String?
    public let turnId: String?
    public let reason: String?
    public let command: String?
    public let cwd: String?
    public let filePath: String?
    public let isBlocking: Bool?
    public let expiresAtMs: UInt64?
    public let questions: [ChatQuestion]?
    public let changes: [RequestedFileChange]?
    // Unknown structured decisions are deliberately not converted to strings.
    public let availableDecisions: [DecisionValue]?
    public let permissions: JSONValue?
    public let additionalPermissions: JSONValue?
    public let networkApprovalContext: JSONValue?
    public let grantRoot: String?
    public let message: String?
    public let mode: String?
    public let url: String?
    public let requestedSchema: JSONValue?
    public let elicitationContext: JSONValue?
}

/// What the phone shows for a secret request. No field here can carry the value.
public struct SecretRequestSummary: Codable, Sendable {
    public let label: String
    public let purpose: String
    public let destination: String
    public let expiresAtMs: UInt64
}

/// The signed Save or Cancel for a secret request. The value exists only in
/// this struct and the one request body: it is never encoded to disk or logged,
/// so the type is deliberately not Codable.
public struct SecretDecision: Sendable {
    public let saves: Bool
    let value: String?
    let actionNonce: String
    let idempotencyKey: String
    public init(request: AttentionRequest, value: String?, idempotencyKey: String = UUID().uuidString) {
        saves = value != nil; self.value = value; actionNonce = request.actionNonce; self.idempotencyKey = idempotencyKey
    }
    public static func target(approvalID: String) -> (requestPath: String, signedTarget: String) {
        let escaped = approvalID.addingPercentEncoding(withAllowedCharacters: .alphanumerics)!
        return ("/api/v1/secret-requests/" + escaped, "/api/v1/secret-requests/" + approvalID)
    }
    public func transcript(path: String, connection: SavedConnection, issuedAtMs: UInt64) throws -> Data {
        let canonical = Data(try approvalJSON(.array([.string(saves ? "save" : "cancel"), .string(actionNonce), .string("pending"), .string(idempotencyKey), .string(value ?? "")])).utf8)
        let hash = SHA256.hash(data: canonical).map { String(format: "%02x", $0) }.joined()
        return Data(["wonder-action-v1", "secret.save", path, hash, actionNonce,
            connection.credential.csrfToken, connection.credential.deviceId, connection.credential.hostInstallationId,
            String(issuedAtMs), "pending"].joined(separator: "\n").utf8)
    }
    public func payload(issuedAtMs: UInt64, signature: String) throws -> Data {
        var body: [String: Any] = ["decision": saves ? "save" : "cancel", "actionNonce": actionNonce, "expectedState": "pending",
            "idempotencyKey": idempotencyKey, "issuedAtMs": issuedAtMs, "signature": signature]
        if let value { body["value"] = value }
        return try JSONSerialization.data(withJSONObject: body, options: [.sortedKeys, .withoutEscapingSlashes])
    }
}

/// The host includes this only when the complete native question fits a notification.
public struct NotificationQuestion: Codable, Sendable {
    public let id: String
    public let body: String
}

public struct PhoneApprovalChoice: Identifiable, Equatable, Sendable {
    public let id: String
    public let title: String
    public let detail: String?
    public let decision: String
    public let structuredDecision: JSONValue?
    /// Stops the agent's current work rather than only answering the request.
    public let isDestructive: Bool
    fileprivate init(_ id: String, _ title: String, _ detail: String? = nil,
                     decision: String? = nil, structured: JSONValue? = nil, destructive: Bool = false) {
        self.id = id; self.title = title; self.detail = detail
        self.decision = decision ?? id; structuredDecision = structured; isDestructive = destructive
    }
}

public struct PhoneApprovalField: Identifiable, Sendable {
    public enum Kind: String, Sendable { case text, number, integer, boolean, choice, multiChoice }
    public let id: String
    public let title: String
    public let kind: Kind
    public let options: [String]
    public let optionTitles: [String: String]
    public let detail: String?
    public let defaultValue: String?
    public let required: Bool
    public let secret: Bool
    fileprivate let schema: [String: JSONValue]
}

public enum PhoneApprovalError: Error, LocalizedError {
    case invalidChoice, invalidAnswers
    public var errorDescription: String? {
        switch self {
        case .invalidChoice: "This choice is no longer available. Refresh the request."
        case .invalidAnswers: "Complete the requested fields with valid values before replying."
        }
    }
}

extension AttentionRequest {
    /// Claude's native tool approval is carried as `name: <input>` inside a
    /// command-shaped envelope. Only the explicit tool kind selects this path;
    /// an ordinary shell command must never be reinterpreted as a tool call.
    private var toolApproval: (name: String, input: String)? {
        guard params.kind == "tool", let command = params.command,
              let separator = command.range(of: ": ") else { return nil }
        return (String(command[..<separator.lowerBound]), String(command[separator.upperBound...]))
    }

    public var approvalTitle: String {
        if isSecretRequest { return "Secret requested" }
        if let toolApproval {
            if toolApproval.name.hasPrefix("mcp__") {
                return ToolDisplayName.title(tool: toolApproval.name) ?? "Use tool"
            }
            return ToolDisplayName.title(tool: toolApproval.name)?.replacingOccurrences(of: "Tool: ", with: "", options: .anchored).capitalized ?? "Use tool"
        }
        switch method {
        case "item/commandExecution/requestApproval": return params.kind == "tool" ? "Use tool" : "Run command"
        case "item/fileChange/requestApproval": return "Change files"
        case "item/permissions/requestApproval": return "Grant access"
        default: return "Permission requested"
        }
    }

    public var approvalInput: String? { toolApproval?.input ?? params.command }

    /// What a tool approval will do, in words. Unlike the raw command, the
    /// title names the action ("Write a file"); provider-specific tool names
    /// that are not known keep `approvalTitle`.
    public var toolActionTitle: String? {
        guard let name = toolApproval?.name else { return nil }
        let key = name.lowercased().filter { $0.isLetter || $0.isNumber }
        let known = ["write": "Write a file", "edit": "Edit a file", "multiedit": "Edit a file", "strreplace": "Edit a file",
                     "read": "Read a file", "readfile": "Read a file", "notebookedit": "Edit a notebook",
                     "webfetch": "Open a web page", "fetch": "Open a web page", "websearch": "Search the web",
                     "glob": "Find files", "grep": "Search in files", "ls": "List a folder", "list": "List a folder",
                     "delete": "Delete a file", "deletefile": "Delete a file", "task": "Start a helper agent"]
        return known[key] ?? approvalTitle
    }

    /// One plain line per argument of a tool approval, so the card says what
    /// the tool will touch. Every argument is listed: one left out could
    /// change where the tool writes or sends data. Values are shortened for
    /// reading; the exact input stays in `approvalInput` for full review.
    public var toolApprovalDetails: [String] {
        guard let input = toolApproval?.input else { return [] }
        guard let value = try? JSONDecoder().decode(JSONValue.self, from: Data(input.utf8)),
              let object = value.approvalDictionary else {
            return input.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || input == "{}"
                ? [] : ["Open Full details to review exactly what it will do."]
        }
        let lead = ["file_path", "filePath", "notebook_path", "notebookPath", "path", "url", "command", "pattern", "query",
                    "old_string", "oldString", "new_string", "newString"]
        let keys = lead.filter { object[$0] != nil } + object.keys.filter { !lead.contains($0) }.sorted()
        let lines = keys.compactMap { key in object[key].flatMap { Self.plainValue($0) }.map { Self.plainLabel(key) + ": " + $0 } }
        let limit = 12
        guard lines.count > limit else { return lines }
        return Array(lines.prefix(limit)) + ["\(lines.count - limit) more details. Open Full details to review them."]
    }

    /// A bounded description of the request for voice, which cannot show the
    /// card: the action, its reason and what it touches. Notifications and
    /// replies keep comparing the exact `params.command`, never this text.
    public var plainSummary: String {
        var lines = [toolActionTitle ?? approvalTitle]
        if let reason = params.reason?.trimmingCharacters(in: .whitespacesAndNewlines), !reason.isEmpty { lines.append(reason) }
        if toolApproval != nil {
            lines += toolApprovalDetails
        } else if let detail = computerAction?.detail {
            lines.append(detail)
        } else if let command = params.command, !command.isEmpty {
            lines.append("Command: " + command)
        }
        if toolApproval == nil, let cwd = params.cwd, !cwd.isEmpty { lines.append("Folder: " + cwd) }
        if let path = params.filePath { lines.append("File: " + path) }
        let changed = (params.changes ?? []).map(\.path)
        if !changed.isEmpty { lines.append((changed.count == 1 ? "File: " : "Files: ") + changed.joined(separator: ", ")) }
        if let message = params.message, !message.isEmpty { lines.append(message) }
        if let url = elicitationURL { lines.append("Opens " + (url.host ?? url.absoluteString)) }
        lines += permissionDetails
        let text = lines.joined(separator: "\n")
        return text.count > 1000 ? String(text.prefix(999)) + "…" : text
    }

    /// Whether `plainSummary` shows everything the owner would approve. Voice
    /// may only offer an Allow-class choice on a complete summary; a cut-off
    /// summary, a request with no subject and a file change (named by path
    /// only, its diff unread) must be reviewed on screen. Decline is always fine.
    public var summaryIsComplete: Bool {
        if !(params.changes ?? []).isEmpty { return false }
        var subject = false
        if let input = toolApproval?.input {
            // Every argument must be listed whole: a value cut at 160 characters,
            // a multi-line value, a nested value or an unreadable input hides part of it.
            guard let value = try? JSONDecoder().decode(JSONValue.self, from: Data(input.utf8)),
                  let object = value.approvalDictionary, object.count <= 12 else { return false }
            for entry in object.values {
                switch entry {
                case .string(let text): if text.count > 160 || text.contains("\n") { return false }
                case .number, .bool, .null: break
                default: return false
                }
            }
            subject = true
        }
        if computerAction?.detail != nil { subject = true }
        if let command = params.command, !command.isEmpty { subject = true }
        if params.filePath != nil || elicitationURL != nil || !permissionDetails.isEmpty { subject = true }
        guard subject else { return false }
        var length = (toolActionTitle ?? approvalTitle).count
        for part in [params.reason, params.command, params.cwd, params.filePath, params.message] { length += (part?.count ?? 0) + 12 }
        length += permissionDetails.joined().count
        return length < 900 && !plainSummary.hasSuffix("…")
    }

    private static func plainLabel(_ key: String) -> String {
        let known = ["file_path": "File", "filePath": "File", "notebook_path": "Notebook", "notebookPath": "Notebook", "path": "Path",
                     "url": "Address", "command": "Command", "pattern": "Pattern", "query": "Search for", "content": "Content",
                     "old_string": "Replace", "oldString": "Replace", "new_string": "With", "newString": "With",
                     "new_source": "New content", "replace_all": "Replace every match", "replaceAll": "Replace every match",
                     "prompt": "Instructions", "cwd": "Folder", "workdir": "Folder", "allowed_domains": "Only these sites",
                     "blocked_domains": "Never these sites"]
        if let label = known[key] { return label }
        var words = ""
        for (index, character) in key.enumerated() {
            if character == "_" || character == "-" { words.append(" "); continue }
            if character.isUppercase, index > 0 { words.append(" ") }
            words.append(contentsOf: character.lowercased())
        }
        let trimmed = words.split(separator: " ").joined(separator: " ")
        return trimmed.prefix(1).uppercased() + trimmed.dropFirst()
    }

    private static func plainValue(_ value: JSONValue) -> String? {
        func shorten(_ text: String, _ limit: Int = 160) -> String {
            text.count > limit ? String(text.prefix(limit - 1)) + "…" : text
        }
        switch value {
        case .null: return nil
        case .bool(let flag): return flag ? "Yes" : "No"
        case .number(let number):
            return number.rounded() == number && abs(number) < 1e15 ? String(Int64(number)) : String(number)
        case .string(let text):
            let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
            // One long line keeps both ends, so an address still shows where it goes.
            guard lines.count > 1 else {
                return text.isEmpty ? "Empty" : text.count > 160 ? String(text.prefix(100)) + "…" + String(text.suffix(59)) : text
            }
            let first = lines.first { !$0.trimmingCharacters(in: .whitespaces).isEmpty }.map { String($0) } ?? ""
            return shorten(first, 120) + " (\(lines.count) lines)"
        case .array(let items):
            if let strings = value.approvalStrings, !strings.isEmpty { return shorten(strings.joined(separator: ", ")) }
            if items.isEmpty { return "None" }
            return items.count == 1 ? "1 item" : "\(items.count) items"
        case .object(let object):
            let parts = object.keys.sorted().compactMap { key in object[key].flatMap { plainValue($0) }.map { plainLabel(key) + " " + $0 } }
            return parts.isEmpty ? "None" : shorten(parts.joined(separator: "; "))
        }
    }

    /// The choice a saved, not yet confirmed reply made, matched by the exact
    /// decision bytes it will resend.
    public func savedChoice(_ intent: DecisionIntent) -> PhoneApprovalChoice? {
        guard intent.actionNonce == actionNonce else { return nil }
        return phoneChoices.first { choice in
            let candidate = DecisionIntent(request: self, decision: choice.decision, responseJson: intent.responseJson,
                                           structuredDecision: choice.structuredDecision)
            return candidate.decision == intent.decision && candidate.structuredDecision == intent.structuredDecision
                && candidate.responseJson == intent.responseJson
        }
    }

    /// Shown while a reply is saved but unconfirmed. Only that same reply can
    /// be sent again, so an uncertain Allow is never replaced with a Decline.
    public func savedReplyNotice(_ intent: DecisionIntent) -> String {
        let wait = "until your computer confirms it or the request expires."
        guard let choice = savedChoice(intent) else { return "Your reply is saved but not confirmed yet. It can’t be changed " + wait }
        return "You chose “\(choice.title)”. It isn’t confirmed yet, and it can’t be changed " + wait
    }

    /// A different reply was tried while an earlier one is still unconfirmed.
    public func savedReplyConflict(_ intent: DecisionIntent) -> String {
        let wait = "Only that reply can be sent until your computer confirms it or the request expires."
        guard let choice = savedChoice(intent) else { return "Your earlier reply is saved but not confirmed yet. " + wait }
        return "You already chose “\(choice.title)”. " + wait
    }

    public var phoneChoices: [PhoneApprovalChoice] {
        switch method {
        case "item/commandExecution/requestApproval", "item/fileChange/requestApproval":
            let advertised = params.availableDecisions ?? [.text("accept"), .text("acceptForSession"), .text("decline"), .text("cancel")]
            var seen = Set<String>()
            return advertised.compactMap(commandChoice).filter { seen.insert($0.id).inserted }
        case "item/permissions/requestApproval":
            var choices: [PhoneApprovalChoice] = []
            if parsedPermissions != nil {
                choices += [PhoneApprovalChoice("allowTurn", "Allow for this turn", decision: "accept"),
                            PhoneApprovalChoice("allowSession", "Allow for this session", "Keeps these permissions for the current session.", decision: "acceptForSession")]
            }
            choices.append(PhoneApprovalChoice("decline", "Decline"))
            return choices
        case "mcpServer/elicitation/request":
            var choices: [PhoneApprovalChoice] = []
            if elicitationURL != nil {
                choices.append(PhoneApprovalChoice("accept", "I’ve completed this"))
            } else if parsedForm != nil {
                choices.append(PhoneApprovalChoice("accept", isNativeComputerApproval ? "Allow once" : "Submit"))
            }
            return choices + [PhoneApprovalChoice("decline", "Decline"), PhoneApprovalChoice("cancel", "Cancel request")]
        case "item/tool/call":
            if computerAction != nil {
                return [PhoneApprovalChoice("accept", "Allow once"), PhoneApprovalChoice("decline", "Decline")]
            }
            return [PhoneApprovalChoice("decline", "Decline", decision: "respond")]
        default: return []
        }
    }

    private func commandChoice(_ value: DecisionValue) -> PhoneApprovalChoice? {
        switch value {
        case .text(let decision):
            switch decision {
            case "accept": return PhoneApprovalChoice(decision, "Allow once")
            case "acceptForSession": return PhoneApprovalChoice(decision, "Allow for this session", "Future matching requests in this session can run without asking again.")
            case "decline": return PhoneApprovalChoice(decision, "Decline")
            case "cancel": return PhoneApprovalChoice(decision, "Decline and stop reply", "Decline the request and stop the agent’s reply.", destructive: true)
            default: return nil
            }
        case .structured(let value):
            guard let object = value.approvalDictionary, object.count == 1 else { return nil }
            let permissionKey = object["acpPermission"] == nil ? "opencodePermission" : "acpPermission"
            let provider = permissionKey == "acpPermission" ? "Cursor" : "OpenCode"
            if let option = object[permissionKey]?.approvalDictionary,
               Set(option.keys) == ["optionId", "kind"],
               let optionID = option["optionId"]?.approvalString,
               !optionID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
               let kind = option["kind"]?.approvalString,
               ["allow_always", "reject_always"].contains(kind) {
                let allow = kind == "allow_always"
                return PhoneApprovalChoice(provider.lowercased() + ":" + kind + ":" + optionID,
                    allow ? "Always allow" : "Always decline",
                    allow ? "\(provider) remembers this rule and allows future matching requests without asking."
                        : "\(provider) remembers this rule and declines future matching requests.",
                    decision: permissionKey, structured: value)
            }
            guard method == "item/commandExecution/requestApproval" else { return nil }
            if let amendment = object["acceptWithExecpolicyAmendment"]?.approvalDictionary,
               Set(amendment.keys) == ["execpolicy_amendment"],
               let prefix = amendment["execpolicy_amendment"]?.approvalStrings, !prefix.isEmpty {
                let words = prefix.map { (try? approvalJSON(.string($0))) ?? $0 }.joined(separator: " ")
                return PhoneApprovalChoice("execpolicy:" + words, "Allow and remember command", "Future commands starting with these exact arguments can run without asking:\n" + words,
                    decision: "acceptWithExecpolicyAmendment", structured: value)
            }
            if let amendment = object["applyNetworkPolicyAmendment"]?.approvalDictionary,
               Set(amendment.keys) == ["network_policy_amendment"],
               let rule = amendment["network_policy_amendment"]?.approvalDictionary,
               Set(rule.keys) == ["action", "host"],
               let action = rule["action"]?.approvalString, ["allow", "deny"].contains(action),
               let host = rule["host"]?.approvalString, !host.isEmpty {
                return PhoneApprovalChoice("network:" + action + ":" + host,
                    action == "allow" ? "Always allow this host" : "Always block this host",
                    "Save a network rule for \(host).", decision: "applyNetworkPolicyAmendment", structured: value)
            }
            return nil
        case .unsupported: return nil
        }
    }

    public var permissionDetails: [String] {
        var details: [String] = []
        if isNativeComputerApproval, let context = params.elicitationContext?.approvalDictionary {
            if let subtitle = context["subtitle"]?.approvalString { details.append(subtitle) }
            details += context["details"]?.approvalStrings ?? []
            if let risk = context["riskLevel"]?.approvalString {
                details.append("Risk level: \(risk.capitalized)")
            }
        }
        if let root = params.grantRoot { details.append("Requested write access for this session: \(root)") }
        for profile in [params.permissions, params.additionalPermissions].compactMap({ $0 }) {
            if let parsed = permissionDescriptions(profile) { details += parsed }
            else { details.append("Some requested permissions cannot be displayed safely.") }
        }
        if let network = params.networkApprovalContext?.approvalDictionary,
           let host = network["host"]?.approvalString {
            details.append("Connect to \(host)" + (network["protocol"]?.approvalString.map { " using \($0)." } ?? "."))
        }
        return details
    }

    private var isNativeComputerApproval: Bool {
        method == "mcpServer/elicitation/request"
            && params.elicitationContext?.approvalDictionary?["isComputerUse"] == .bool(true)
    }

    private var parsedPermissions: JSONValue? {
        guard let permissions = params.permissions, permissionDescriptions(permissions) != nil else { return nil }
        return permissions
    }

    public var elicitationFields: [PhoneApprovalField] { parsedForm ?? [] }

    public var elicitationURL: URL? {
        guard method == "mcpServer/elicitation/request", params.mode == "url",
              let value = params.url, let url = URL(string: value),
              url.scheme?.lowercased() == "https", let host = url.host, !host.isEmpty,
              url.user == nil, url.password == nil else { return nil }
        return url
    }

    public var unsupportedReason: String? {
        switch method {
        case "item/tool/call" where computerAction == nil:
            return "This action cannot be run safely. You can decline it here so the agent can choose another approach."
        case "item/permissions/requestApproval" where parsedPermissions == nil:
            return "This permission request has details Cupola cannot safely interpret."
        case "mcpServer/elicitation/request" where parsedForm == nil && elicitationURL == nil:
            return params.mode == "openai/userVerification"
                ? "This service requires device verification that Cupola does not support."
                : "This service requested an unsupported form or link."
        case "item/commandExecution/requestApproval", "item/fileChange/requestApproval":
            return phoneChoices.isEmpty ? "No supported choices were provided. Refresh the request or stop the current turn." : nil
        default: return isQuestion || !phoneChoices.isEmpty ? nil : "This request type is unavailable. Refresh the request or stop the current turn."
        }
    }

    public func canSubmit(choice: PhoneApprovalChoice, answers: [String: String]) -> Bool {
        guard phoneChoices.contains(choice) else { return false }
        if method == "mcpServer/elicitation/request", choice.decision == "accept", elicitationURL == nil {
            return (try? formContent(answers)) != nil
        }
        return true
    }

    public func responseJSON(for choice: PhoneApprovalChoice, answers: [String: String]) throws -> String? {
        guard phoneChoices.contains(choice) else { throw PhoneApprovalError.invalidChoice }
        switch method {
        case "item/permissions/requestApproval":
            let permissions: JSONValue
            if choice.id == "decline" { permissions = .object([:]) }
            else if let requested = parsedPermissions { permissions = requested }
            else { throw PhoneApprovalError.invalidChoice }
            return try approvalJSON(.object(["permissions": permissions, "scope": .string(choice.id == "allowSession" ? "session" : "turn")]))
        case "mcpServer/elicitation/request":
            var value: [String: JSONValue] = ["action": .string(choice.decision)]
            if choice.decision == "accept", elicitationURL == nil { value["content"] = try formContent(answers) }
            return try approvalJSON(.object(value))
        case "item/tool/call":
            return try approvalJSON(.object(["success": .bool(computerAction != nil && choice.id == "accept"), "contentItems": .array([])]))
        default: return nil
        }
    }

    private var parsedForm: [PhoneApprovalField]? {
        guard method == "mcpServer/elicitation/request", ["form", "openai/form", "openaiForm"].contains(params.mode ?? "form"),
              let schema = params.requestedSchema?.approvalDictionary,
              Set(schema.keys).isSubset(of: ["$schema", "type", "properties", "required", "title", "description", "additionalProperties"]),
              schema["type"]?.approvalString == "object", let properties = schema["properties"]?.approvalDictionary,
              properties.count <= 32,
              schema["additionalProperties"] == nil || schema["additionalProperties"] == .bool(false) else { return nil }
        let required: [String]
        if let value = schema["required"], value != .null {
            guard let names = value.approvalStrings, Set(names).isSubset(of: Set(properties.keys)) else { return nil }
            required = names
        } else { required = [] }
        var result: [PhoneApprovalField] = []
        for id in properties.keys.sorted() {
            guard let field = PhoneApprovalField(id: id, value: properties[id]!, required: required.contains(id)) else { return nil }
            result.append(field)
        }
        return result
    }

    private func formContent(_ answers: [String: String]) throws -> JSONValue {
        guard let fields = parsedForm, Set(answers.keys).isSubset(of: Set(fields.map(\.id))) else { throw PhoneApprovalError.invalidAnswers }
        var content: [String: JSONValue] = [:]
        for field in fields {
            if let value = try field.answer(answers[field.id]) { content[field.id] = value }
        }
        return .object(content)
    }
}

/// Unknown or incomplete actions remain unsupported; an approval must describe
/// exactly the arguments the Mac will execute.
public struct ComputerAction: Codable, Sendable {
    public let action: String?
    public let x: Double?
    public let y: Double?
    public let text: String?
    public let keyCode: UInt16?
    public let modifiers: UInt64?
    public let bundleId: String?
    private let validFields: Bool
    private struct Key: CodingKey {
        let stringValue: String
        var intValue: Int? { nil }
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { return nil }
    }
    public init(from decoder: Decoder) throws {
        guard let c = try? decoder.container(keyedBy: Key.self) else {
            action = nil; x = nil; y = nil; text = nil; keyCode = nil; modifiers = nil; bundleId = nil; validFields = false
            return
        }
        func value<T: Decodable>(_ name: String, _ type: T.Type) -> T? { try? c.decode(type, forKey: Key(stringValue: name)!) }
        action = value("action", String.self); x = value("x", Double.self); y = value("y", Double.self)
        text = value("text", String.self); keyCode = value("keyCode", UInt16.self)
        modifiers = value("modifiers", UInt64.self); bundleId = value("bundleId", String.self)
        let fields: Set<String>
        switch action {
        case "status", "screenshot": fields = ["action"]
        case "click": fields = ["action", "x", "y"]
        case "type": fields = ["action", "text"]
        case "key": fields = ["action", "keyCode", "modifiers"]
        case "focusApp": fields = ["action", "bundleId"]
        default: fields = []
        }
        validFields = Set(c.allKeys.map(\.stringValue)).isSubset(of: fields)
            && (!c.contains(Key(stringValue: "modifiers")!) || modifiers != nil)
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Key.self)
        try c.encodeIfPresent(action, forKey: Key(stringValue: "action")!)
        try c.encodeIfPresent(x, forKey: Key(stringValue: "x")!); try c.encodeIfPresent(y, forKey: Key(stringValue: "y")!)
        try c.encodeIfPresent(text, forKey: Key(stringValue: "text")!); try c.encodeIfPresent(keyCode, forKey: Key(stringValue: "keyCode")!)
        try c.encodeIfPresent(modifiers, forKey: Key(stringValue: "modifiers")!); try c.encodeIfPresent(bundleId, forKey: Key(stringValue: "bundleId")!)
        // Preserve unsupported status across caching and restoration.
        if !validFields { try c.encode(true, forKey: Key(stringValue: "unsupported")!) }
    }
    public var detail: String? {
        guard validFields else { return nil }
        switch action {
        case "screenshot": return "Capture your computer’s screen."
        case "status": return "Read the active app and computer access status."
        case "click":
            guard let x, let y, x.isFinite, y.isFinite, abs(x) < 1e9, abs(y) < 1e9 else { return nil }
            return "Click at (\(Int(x.rounded())), \(Int(y.rounded()))) on your computer."
        case "type": guard let text, text.utf8.count <= 32768 else { return nil }; return "Type into the focused field on your computer:\n\(text)"
        case "key":
            guard let keyCode, (modifiers ?? 0) & ~(UInt64(31) << 16) == 0 else { return nil }
            let names = [(16, "Caps Lock"), (17, "Shift"), (18, "Control"), (19, "Option"), (20, "Command")]
                .filter { (modifiers ?? 0) & (UInt64(1) << $0.0) != 0 }.map(\.1)
            let key = Self.keyNames[keyCode].map { "the \($0) key" } ?? "key code \(keyCode)"
            return "Press \(key)" + (names.isEmpty ? " on your computer." : " with " + names.joined(separator: ", ") + ".")
        case "focusApp": guard let bundleId, !bundleId.isEmpty, bundleId.utf8.count <= 256 else { return nil }; return "Focus the app \(bundleId) on your computer."
        default: return nil
        }
    }
    /// macOS virtual key codes (US layout) the computer-use tool sends.
    /// Others keep their code rather than guessing a name.
    private static let keyNames: [UInt16: String] = [
        36: "Return", 48: "Tab", 49: "Space", 51: "Delete", 53: "Escape", 117: "Forward Delete",
        115: "Home", 119: "End", 116: "Page Up", 121: "Page Down",
        123: "Left Arrow", 124: "Right Arrow", 125: "Down Arrow", 126: "Up Arrow",
        0: "A", 11: "B", 8: "C", 2: "D", 14: "E", 3: "F", 5: "G", 4: "H", 34: "I", 38: "J", 40: "K", 37: "L", 46: "M",
        45: "N", 31: "O", 35: "P", 12: "Q", 15: "R", 1: "S", 17: "T", 32: "U", 9: "V", 13: "W", 7: "X", 16: "Y", 6: "Z",
        29: "0", 18: "1", 19: "2", 20: "3", 21: "4", 23: "5", 22: "6", 26: "7", 28: "8", 25: "9"]
}
private extension PhoneApprovalField {
    init?(id: String, value: JSONValue, required: Bool) {
        guard let schema = value.approvalDictionary, let type = schema["type"]?.approvalString else { return nil }
        let common: Set<String> = ["type", "title", "description", "default", "writeOnly", "isSecret"]
        let allowed: Set<String>
        switch type {
        case "string": allowed = common.union(["format", "minLength", "maxLength", "enum", "enumNames", "oneOf"])
        case "number", "integer": allowed = common.union(["minimum", "maximum"])
        case "boolean": allowed = common
        case "array": allowed = common.union(["items", "minItems", "maxItems"])
        default: return nil
        }
        guard Set(schema.keys).isSubset(of: allowed) else { return nil }
        for key in ["title", "description", "format"] {
            if let value = schema[key], value != .null, value.approvalString == nil { return nil }
        }
        for key in ["isSecret", "writeOnly"] {
            if let value = schema[key], value != .null, value != .bool(true), value != .bool(false) { return nil }
        }
        for key in ["minimum", "maximum", "minLength", "maxLength", "minItems", "maxItems"] {
            if let value = schema[key], value != .null {
                guard let number = value.approvalNumber, number.isFinite else { return nil }
                if !["minimum", "maximum"].contains(key), number < 0 || number.rounded() != number { return nil }
            }
        }
        if let min = schema["minimum"]?.approvalNumber, let max = schema["maximum"]?.approvalNumber, min > max { return nil }
        if let min = schema["minLength"]?.approvalNumber, let max = schema["maxLength"]?.approvalNumber, min > max { return nil }
        if let min = schema["minItems"]?.approvalNumber, let max = schema["maxItems"]?.approvalNumber, min > max { return nil }
        let kind: Kind
        var options: [String] = []
        var optionTitles: [String: String] = [:]
        if type == "array" || schema["enum"] != nil || schema["oneOf"] != nil {
            let source: [String: JSONValue]
            if type == "array" {
                guard let items = schema["items"]?.approvalDictionary,
                      Set(items.keys).isSubset(of: ["type", "enum", "anyOf"]),
                      items["type"] == nil || items["type"] == .string("string") else { return nil }
                source = items; kind = .multiChoice
            } else { source = schema; kind = .choice }
            if let values = source["enum"] {
                guard source["oneOf"] == nil, source["anyOf"] == nil, let strings = values.approvalStrings else { return nil }
                options = strings
                if let names = schema["enumNames"], names != .null {
                    guard let titles = names.approvalStrings, titles.count == strings.count else { return nil }
                    for (value, title) in zip(strings, titles) { optionTitles[value] = title }
                }
            } else {
                guard let variants = source[type == "array" ? "anyOf" : "oneOf"]?.approvalArray else { return nil }
                for variant in variants {
                    guard let entry = variant.approvalDictionary, Set(entry.keys) == ["const", "title"],
                          let value = entry["const"]?.approvalString, let title = entry["title"]?.approvalString else { return nil }
                    options.append(value); optionTitles[value] = title
                }
            }
            guard !options.isEmpty, Set(options).count == options.count else { return nil }
        } else {
            switch type {
            case "string":
                if let format = schema["format"]?.approvalString,
                   !["email", "uri", "date", "date-time", "password"].contains(format) { return nil }
                kind = .text
            case "number": kind = .number
            case "integer": kind = .integer
            case "boolean": kind = .boolean
            default: return nil
            }
        }
        self.id = id; self.title = schema["title"]?.approvalString ?? id; self.kind = kind
        self.options = options; self.optionTitles = optionTitles; detail = schema["description"]?.approvalString
        self.required = required; self.schema = schema
        secret = schema["writeOnly"] == .bool(true) || schema["isSecret"] == .bool(true) || schema["format"] == .string("password")
        if let value = schema["default"], value != .null {
            switch value {
            case .string(let text): defaultValue = text
            case .number, .bool, .array: defaultValue = try? approvalJSON(value)
            default: defaultValue = nil
            }
        } else { defaultValue = nil }
        if schema["default"] != nil, schema["default"] != .null {
            guard let defaultValue, (try? answer(defaultValue)) != nil else { return nil }
        }
    }

    func answer(_ input: String?) throws -> JSONValue? {
        guard let input, !input.isEmpty else {
            if required { throw PhoneApprovalError.invalidAnswers }
            return nil
        }
        guard input.utf8.count <= 32768 else { throw PhoneApprovalError.invalidAnswers }
        func within(_ value: Double, _ lower: String, _ upper: String) -> Bool {
            (schema[lower]?.approvalNumber.map { value >= $0 } ?? true)
                && (schema[upper]?.approvalNumber.map { value <= $0 } ?? true)
        }
        switch kind {
        case .text:
            guard within(Double(input.unicodeScalars.count), "minLength", "maxLength"), validFormat(input) else { throw PhoneApprovalError.invalidAnswers }
            return .string(input)
        case .number, .integer:
            guard let number = Double(input), number.isFinite,
                  kind != .integer || (number.rounded() == number && abs(number) <= 9_007_199_254_740_991),
                  within(number, "minimum", "maximum") else { throw PhoneApprovalError.invalidAnswers }
            return .number(number)
        case .boolean:
            guard ["true", "false"].contains(input) else { throw PhoneApprovalError.invalidAnswers }
            return .bool(input == "true")
        case .choice:
            guard options.contains(input),
                  within(Double(input.unicodeScalars.count), "minLength", "maxLength"),
                  validFormat(input) else { throw PhoneApprovalError.invalidAnswers }
            return .string(input)
        case .multiChoice:
            guard let choices = try? JSONDecoder().decode([String].self, from: Data(input.utf8)),
                  Set(choices).count == choices.count, choices.allSatisfy(options.contains),
                  within(Double(choices.count), "minItems", "maxItems") else { throw PhoneApprovalError.invalidAnswers }
            return .array(choices.map(JSONValue.string))
        }
    }

    func validFormat(_ input: String) -> Bool {
        switch schema["format"]?.approvalString {
        case "email":
            let parts = input.split(separator: "@", omittingEmptySubsequences: false)
            return parts.count == 2 && parts.allSatisfy { !$0.isEmpty } && !input.contains(where: \.isWhitespace)
        case "uri": return URL(string: input)?.scheme?.isEmpty == false
        case "date":
            guard input.range(of: #"^\d{4}-\d{2}-\d{2}$"#, options: .regularExpression) != nil else { return false }
            let formatter = DateFormatter(); formatter.locale = Locale(identifier: "en_US_POSIX")
            formatter.dateFormat = "yyyy-MM-dd"; formatter.isLenient = false
            return formatter.date(from: input).map { formatter.string(from: $0) == input } ?? false
        case "date-time":
            let formatter = ISO8601DateFormatter()
            if formatter.date(from: input) != nil { return true }
            formatter.formatOptions.insert(.withFractionalSeconds)
            return formatter.date(from: input) != nil
        default: return true
        }
    }
}

private func permissionDescriptions(_ value: JSONValue) -> [String]? {
    guard let profile = value.approvalDictionary, Set(profile.keys).isSubset(of: ["fileSystem", "network"]) else { return nil }
    var result: [String] = []
    if let value = profile["network"], value != .null {
        guard let network = value.approvalDictionary, Set(network.keys).isSubset(of: ["enabled"]) else { return nil }
        if let enabled = network["enabled"], enabled != .null {
            guard enabled == .bool(true) || enabled == .bool(false) else { return nil }
            result.append(enabled == .bool(true) ? "Allow network access." : "Keep network access disabled.")
        }
    }
    if let value = profile["fileSystem"], value != .null {
        guard let files = value.approvalDictionary, Set(files.keys).isSubset(of: ["entries", "read", "write", "globScanMaxDepth"]) else { return nil }
        for (key, label) in [("read", "Read"), ("write", "Read and write")] {
            if let paths = files[key], paths != .null {
                guard let strings = paths.approvalStrings else { return nil }
                result += strings.map { "\(label): \($0)" }
            }
        }
        if let entries = files["entries"], entries != .null {
            guard let entries = entries.approvalArray else { return nil }
            for entry in entries {
                guard let entry = entry.approvalDictionary, Set(entry.keys) == ["access", "path"],
                      let access = entry["access"]?.approvalString, let label = ["read":"Read", "write":"Read and write", "deny":"Deny access"][access],
                      let path = entry["path"], let description = permissionPath(path) else { return nil }
                result.append("\(label): \(description)")
            }
        }
        if let depth = files["globScanMaxDepth"], depth != .null {
            guard let number = depth.approvalNumber, number >= 1, number <= 9_007_199_254_740_991, number.rounded() == number else { return nil }
            result.append("Search matching paths up to \(Int(number)) folders deep.")
        }
    }
    return result.isEmpty ? ["No additional file or network access."] : result
}

private func permissionPath(_ value: JSONValue) -> String? {
    guard let path = value.approvalDictionary else { return nil }
    switch path["type"]?.approvalString {
    case "path": guard Set(path.keys) == ["type", "path"] else { return nil }; return path["path"]?.approvalString
    case "glob_pattern": guard Set(path.keys) == ["type", "pattern"] else { return nil }; return path["pattern"]?.approvalString.map { "paths matching \($0)" }
    case "special":
        guard Set(path.keys) == ["type", "value"], let special = path["value"]?.approvalDictionary,
              let kind = special["kind"]?.approvalString else { return nil }
        let label: String
        switch kind {
        case "root", "minimal", "tmpdir", "slash_tmp":
            guard Set(special.keys) == ["kind"] else { return nil }
            label = ["root":"all filesystem roots", "minimal":"minimal system paths", "tmpdir":"the temporary folder", "slash_tmp":"/tmp"][kind]!
        case "project_roots":
            guard Set(special.keys).isSubset(of: ["kind", "subpath"]) else { return nil }; label = "project folders"
        case "unknown":
            guard Set(special.keys).isSubset(of: ["kind", "path", "subpath"]), let name = special["path"]?.approvalString else { return nil }; label = name
        default: return nil
        }
        if let subpath = special["subpath"], subpath != .null {
            guard let name = subpath.approvalString else { return nil }; return label + "/" + name
        }
        return label
    default: return nil
    }
}

private extension JSONValue {
    var approvalDictionary: [String: JSONValue]? { if case .object(let value) = self { return value }; return nil }
    var approvalArray: [JSONValue]? { if case .array(let value) = self { return value }; return nil }
    var approvalString: String? { if case .string(let value) = self { return value }; return nil }
    var approvalNumber: Double? { if case .number(let value) = self { return value }; return nil }
    var approvalStrings: [String]? {
        guard let array = approvalArray else { return nil }
        let strings = array.compactMap(\.approvalString); return strings.count == array.count ? strings : nil
    }
    var approvalObject: Any {
        switch self {
        case .object(let value): value.mapValues(\.approvalObject)
        case .array(let value): value.map(\.approvalObject)
        case .string(let value): value
        case .number(let value): value
        case .bool(let value): value
        case .null: NSNull()
        }
    }
}

/// Rust serde_json stores object keys in UTF-8 lexical order. Foundation's
/// sortedKeys can use numeric or locale collation, so order objects explicitly.
private func approvalJSON(_ value: JSONValue) throws -> String {
    switch value {
    case .object(let object):
        let keys = object.keys.sorted { $0.utf8.lexicographicallyPrecedes($1.utf8) }
        return "{" + (try keys.map { try approvalJSON(.string($0)) + ":" + approvalJSON(object[$0]!) }).joined(separator: ",") + "}"
    case .array(let array): return "[" + (try array.map(approvalJSON)).joined(separator: ",") + "]"
    default:
        return String(decoding: try JSONSerialization.data(withJSONObject: value.approvalObject, options: [.fragmentsAllowed, .withoutEscapingSlashes]), as: UTF8.self)
    }
}

public enum DecisionValue: Codable, Sendable {
    case text(String), structured(JSONValue), unsupported
    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if let text = try? c.decode(String.self) { self = .text(text) }
        else if let value = try? c.decode(JSONValue.self), case .object = value { self = .structured(value) }
        else { self = .unsupported }
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .text(let s): try c.encode(s)
        case .structured(let value): try c.encode(value)
        case .unsupported: try c.encodeNil()
        }
    }
    public var text: String? { if case .text(let s) = self { return s }; return nil }
}
public struct RequestedFileChange: Codable, Sendable {
    public let path: String
    public let kind: String?
    /// Unified diff the host proposes; absent when the host had none or it was too large.
    public let diff: String?
    public let truncated: Bool?
}
public struct ChatQuestion: Codable, Identifiable, Sendable {
    public var multiSelect: Bool? = nil
    public let id: String
    public let header: String?
    public let isSecret: Bool?
    public let isOther: Bool?
    public let question: String
    public let options: [QuestionOption]?
    /// Multi-choice drafts store an array, never comma-delimited labels.
    /// Legacy single-choice/free-text drafts retain their exact text.
    public func answers(from draft: String) -> [String] {
        guard multiSelect == true else { return draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? [] : [draft] }
        return ((try? JSONDecoder().decode([String].self, from: Data(draft.utf8))) ?? [])
            .filter { !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
    }
}
public struct QuestionOption: Codable, Sendable { public let label: String; public let description: String? }

/// The HTTP route escapes one path segment. Rust's Axum Path<String> decodes
/// that segment once, then constructs the action target from the original ID.
public struct ApprovalResolutionTarget: Sendable {
    public let requestPath: String
    public let signedTarget: String
    public init(approvalID: String) {
        let escaped = approvalID.addingPercentEncoding(withAllowedCharacters: .alphanumerics)!
        requestPath = "/api/v1/approvals/" + escaped + "/resolve"
        signedTarget = "/api/v1/approvals/" + approvalID + "/resolve"
    }
}

/// The logical decision is persisted before signing. Timestamps/signatures may
/// refresh after session renewal; decision bytes, nonce and identity never change.
public struct DecisionIntent: Codable, Sendable {
    public let decision: String
    public let actionNonce: String
    public let expectedState = "pending"
    public let idempotencyKey: String
    public let responseJson: String?
    public let structuredDecision: JSONValue?
    public init(request: AttentionRequest, decision: String, responseJson: String?, structuredDecision: JSONValue? = nil) {
        let computer = request.computerAction != nil && ["accept", "decline"].contains(decision)
        self.decision = computer ? "respond" : decision; actionNonce = request.actionNonce
        idempotencyKey = request.resolutionIdempotencyKey ?? UUID().uuidString
        self.responseJson = computer ? "{\"success\":\(decision == "accept" ? "true" : "false"),\"contentItems\":[]}" : responseJson
        self.structuredDecision = structuredDecision
    }
    public func transcript(path: String, connection: SavedConnection, issuedAtMs: UInt64) throws -> Data {
        let canonical = Data(try approvalJSON(.array([structuredDecision ?? .string(decision), .string(actionNonce), .string(expectedState), .string(idempotencyKey), .string(responseJson ?? "")])).utf8)
        let hash = SHA256.hash(data: canonical).map { String(format: "%02x", $0) }.joined()
        return Data(["wonder-action-v1", "approval.resolve", path, hash, actionNonce,
            connection.credential.csrfToken, connection.credential.deviceId, connection.credential.hostInstallationId,
            String(issuedAtMs), expectedState].joined(separator: "\n").utf8)
    }
    public func payload(issuedAtMs: UInt64, signature: String) throws -> Data {
        var body: [String: Any] = ["decision": (structuredDecision ?? .string(decision)).approvalObject, "actionNonce": actionNonce, "expectedState": expectedState,
            "idempotencyKey": idempotencyKey, "issuedAtMs": issuedAtMs, "signature": signature]
        if let responseJson { body["responseJson"] = responseJson }
        return try JSONSerialization.data(withJSONObject: body, options: [.sortedKeys, .withoutEscapingSlashes])
    }
}

public struct AsyncQuestion: Codable, Identifiable, Sendable {
    public let id: String
    public let conversationId: String
    public let turnId: String
    public let itemId: String
    public let questions: [AsyncQuestionPrompt]
    public let state: String
    public let expiresAtMs: UInt64
    public let response: AsyncAnswerIntent?
    public var rowId: String { turnId + "/" + itemId }
    public func canAnswer(now: UInt64) -> Bool { state == "pending" && now < expiresAtMs }
    /// When a pending question stops being answerable; settled questions
    /// need no clock. One millisecond late, so the floating-point date never
    /// reads as still answerable when the view updates at it.
    public var answerDeadline: Date? {
        state == "pending" ? Date(timeIntervalSince1970: Double(expiresAtMs) / 1000 + 0.001) : nil
    }
}
public struct AsyncQuestionPrompt: Codable, Sendable {
    public let title: String
    public let options: [String]?
}

/// Provider history carries the form and its reply as separate items. These
/// saved forms are read-only; pending actions still belong to the host API.
public struct NativeQuestionPresentation: Sendable {
    public let questions: [AsyncQuestionPrompt]
    public internal(set) var answers: [Int: String] = [:]
    public var isAnswered: Bool { questions.indices.allSatisfy { answers[$0] != nil } }

    static func prepare(_ item: ReadItem) -> Self? {
        guard item.type == "agentMessage", item.payload?["delivery"]?.string == "async",
              let values = item.payload?["questions"]?.array, !values.isEmpty, values.count <= 20,
              let data = try? JSONEncoder().encode(values),
              let questions = try? JSONDecoder().decode([AsyncQuestionPrompt].self, from: data),
              questions.allSatisfy({ !$0.title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }) else { return nil }
        return Self(questions: questions)
    }
}

struct NativeQuestionReply: Decodable, Sendable {
    let questionItemId: String
    let question: String
    let answer: String
    var target: (itemID: String, index: Int)? {
        guard let data = questionItemId.data(using: .utf8),
              let values = try? JSONDecoder().decode([ThreadValue].self, from: data), values.count == 3,
              values[0].string == "request_user_input_async", let itemID = values[1].string, !itemID.isEmpty,
              let index = values[2].number, index >= 0, index < 20, index.rounded() == index else { return nil }
        return (itemID, Int(index))
    }
    static func parse(_ text: String) -> [Self]? {
        let start = "<send_user_message_question_reply>"
        let end = "</send_user_message_question_reply>"
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix(start), trimmed.hasSuffix(end), trimmed.utf8.count <= 262_144,
              let data = String(trimmed.dropFirst(start.count).dropLast(end.count)).data(using: .utf8),
              let replies = try? JSONDecoder().decode([Self].self, from: data),
              !replies.isEmpty, replies.count <= 20, replies.allSatisfy({ $0.target != nil && !$0.question.isEmpty }) else { return nil }
        return replies
    }
}
public struct AsyncAnswerIntent: Codable, Sendable {
    public let answers: [String]
    public let skip: Bool
    public init(answers: [String], skip: Bool) { self.answers = answers; self.skip = skip }
    public func validationError(for question: AsyncQuestion) -> AsyncAnswerValidationError? {
        if skip { return answers.isEmpty ? nil : .answerRequired }
        guard answers.count == question.questions.count,
              answers.allSatisfy({ !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }) else { return .answerRequired }
        guard answers.allSatisfy({ $0.utf8.count <= 8192 }) else { return .answerTooLong }
        // Match the host's title/newline/answer envelope, including separators.
        let bytes = zip(question.questions, answers).reduce(0) { $0 + $1.0.title.utf8.count + 1 + $1.1.utf8.count }
            + max(0, answers.count - 1) * 2
        return bytes > 65536 ? .responseTooLong : nil
    }
}

public enum AsyncAnswerValidationError: LocalizedError, Equatable, Sendable {
    case answerRequired, answerTooLong, responseTooLong
    public var errorDescription: String? {
        switch self {
        case .answerRequired: "Answer every question before replying, or choose Skip."
        case .answerTooLong: "An answer is too long. Shorten it and try again."
        case .responseTooLong: "This reply is too long. Shorten your answers and try again."
        }
    }
}
