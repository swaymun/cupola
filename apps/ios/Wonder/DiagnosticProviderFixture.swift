#if WONDER_DIAGNOSTICS
import Foundation
import WonderPairing

extension DiagnosticSubagentFixture {
    /// Recovery matrix uses only the owned synthetic computer and local files.
    static var recoveryCase: String? {
        let args = ProcessInfo.processInfo.arguments
        guard let index = args.firstIndex(of: "-diagnostics-recovery-case"), args.indices.contains(index + 1) else { return nil }
        return args[index + 1]
    }
    static var recoveryUnresolved: Bool { recoveryCase != nil && !signalled("recovery-resolved") }

    @MainActor static func seedRecoveryDraft() {
        guard recoveryCase != nil else { return }
        var draft = NewChatDraft(destination: .project(id: "read-project"), text: "Keep my recovery draft",
                                 family: .claude, model: "claude:sonnet")
        draft.startsInT3Code = ["expired", "unreachable"].contains(recoveryCase ?? "")
        let file = try! StagedFile(name: "recovery.txt", mimeType: "text/plain", data: Data("Recovery attachment must survive.".utf8))
        draft.attachments = [try! NewChatDraftStore.stage(file)]
        NewChatDraftStore.save(draft, host: hostID)
        NewChatDraftStore.lastHost = hostID
    }

    /// OpenCode and Cursor enabled in T3 Code; offline catalog fixture only.
    static var t3CodeProvidersFixture: Bool { ProcessInfo.processInfo.arguments.contains("-diagnostics-t3-code-providers") }

    /// Same wire shape as wonderd's projection of orchestrator_capabilities.
    static func t3PickerCatalog(search: String? = nil) -> [String: Any] {
        func model(_ native: String, _ name: String, _ family: String, _ instance: String, _ efforts: [String], fast: Bool) -> [String: Any] {
            ["id": "t3:\(instance):\(native)", "displayName": name, "agentFamily": family, "hidden": false,
             "reasoningEfforts": efforts.map { ["id": $0, "label": $0.capitalized] },
             "defaultReasoningEffort": efforts.first as Any? ?? NSNull(),
             "serviceTiers": fast ? [["id": "default", "label": "Standard"], ["id": "fast", "label": "Fast"]] : [],
             "defaultServiceTier": fast ? "default" : NSNull()]
        }
        var providers: [[String: Any]] = [
            ["instanceId": "claudeAgent", "family": "claude", "name": "Claude in T3 Code",
             "runtimeModes": ["approval-required", "auto-accept-edits", "full-access"],
             "models": [model("claude-sonnet-5-5", "Claude Sonnet fixture", "claude", "claudeAgent", ["medium"], fast: false),
                        model("claude-opus-5-5", "Opus in T3 Code", "claude", "claudeAgent", ["high", "medium", "low"], fast: true)]],
            ["instanceId": "codex", "family": "codex", "name": "Codex in T3 Code",
             "omittedModels": 1,
             "runtimeModes": ["approval-required", "full-access"],
             "models": [model("gpt-6.1", "GPT in T3 Code", "codex", "codex", ["low", "medium", "high"], fast: true)]]
        ]
        if t3CodeProvidersFixture {
            providers += [
                ["instanceId": "opencode", "family": "opencode", "name": "OpenCode in T3 Code",
                 "runtimeModes": ["full-access"],
                 "models": [model("opencode/beam-free", "Beam Free in T3 Code", "opencode", "opencode", [], fast: false)]],
                ["instanceId": "cursor", "family": "cursor", "name": "Cursor in T3 Code",
                 "runtimeModes": ["full-access"],
                 "models": [model("grok-4.7", "Grok 4.7 in T3 Code", "cursor", "cursor", [], fast: false)]]
            ]
        }
        if let search, !search.isEmpty {
            providers[1]["models"] = (providers[1]["models"] as! [[String: Any]])
                + [model("gpt-6.1-pro", "Pro in T3 Code", "codex", "codex", ["high"], fast: false)]
            for index in providers.indices {
                providers[index]["models"] = (providers[index]["models"] as! [[String: Any]]).filter {
                    ($0["id"] as? String)?.localizedCaseInsensitiveContains(search) == true
                        || ($0["displayName"] as? String)?.localizedCaseInsensitiveContains(search) == true
                }
            }
        }
        return ["providers": providers]
    }
    /// Use the several-computers fixture for host isolation.
    /// Distinct origins let the transport route the actual selected model's requests.
    static var providerComputersFixture: Bool { severalComputersFixture }
    static func providerComputerID(_ url: URL?) -> String {
        guard providerComputersFixture, let host = url?.host,
              host.hasSuffix(".synthetic.invalid") else { return hostID }
        return String(host.dropLast(".synthetic.invalid".count))
    }
    static func providerComputerName(_ host: String) -> String {
        extraComputers.first { $0.0 == host }?.1 ?? "Synthetic fixture"
    }
    static func providerComputerOrigin(_ host: String) -> String {
        providerComputersFixture && host != hostID ? "https://\(host).synthetic.invalid" : "https://synthetic.invalid"
    }

    static var opencodeFixture: Bool {
        ProcessInfo.processInfo.arguments.contains("-diagnostics-opencode-fixture")
    }
    static var cursorFixture: Bool {
        ProcessInfo.processInfo.arguments.contains("-diagnostics-cursor-fixture")
    }
    static var acpFixture: Bool { opencodeFixture || cursorFixture }

    /// One family owns the fixture's sidebar row, detail and model controls.
    static var projectFamily: AgentFamily {
        if composerCompletionFixture && composerCodexFixture { return .codex }
        if cursorFixture { return .cursor }
        if opencodeFixture { return .opencode }
        if currentStepFixture || promptSuggestionFixture || composerCompletionFixture || turnCollapseFixture || taskWakesFixture { return .claude }
        if !projectReadFixture { return .codex }
        if sendClaudeFixture { return .claude }
        if projectActionsFixture || projectArchiveFixture || projectSpeedFixture
            || projectSubagentFixture || projectFilesSendFixture
            || projectProviderSwitchFixture || sendStatesFixture { return .codex }
        return .claude
    }

    static func projectModelID(saved: String?) -> String {
        switch projectFamily {
        case .codex: return saved ?? "gpt-fixture"
        case .claude: return saved ?? "claude:sonnet"
        case .opencode: return saved ?? "opencode:local/fixture"
        case .cursor: return saved ?? "cursor:default[]"
        }
    }

    static var acpModels: [[String: Any]] {
        let models = cursorFixture
            // Enough Cursor models for the picker's search field (model-search.wctl).
            ? [("default[]", "Auto fixture", true), ("luna[reasoning=medium]", "Luna fixture", false)]
                + ["Claude Opus 5.5", "Claude Sonnet 5.5", "GPT-5.6 Sol", "GPT-5.4 Mini", "Gemini 3.1 Pro",
                   "Grok 4.7", "Composer 2.5", "GLM 5.3 Flash"].map { name in
                    (name.lowercased().replacingOccurrences(of: " ", with: "-") + "[fast=false]", name, false)
                }
            : [("local/fixture", "Local fixture", true), ("opencode/free-fixture", "Free fixture", false)]
        return models.map { id, name, isDefault in
            ["id": projectFamily.rawValue + ":" + id, "displayName": name, "hidden": false,
             "agentFamily": projectFamily.rawValue, "isDefault": isDefault,
             "defaultReasoningEffort": NSNull(), "reasoningEfforts": []]
        }
    }

    static var claudeModels: [[String: Any]] {
        [("sonnet", "Sonnet fixture", ["medium", "high", "ultracode"]), ("haiku", "Haiku fixture", [])].map { id, name, efforts in
            ["id": "claude:" + id, "displayName": name, "hidden": false,
             "agentFamily": "claude", "isDefault": id == "sonnet",
             "defaultReasoningEffort": efforts.first as Any? ?? NSNull(),
             "reasoningEfforts": efforts.map { ["id": $0, "label": $0 == "ultracode" ? "Ultracode" : $0.capitalized] },
             // Claude Code's fast mode, as the Claude bridge lists it for a model that supports it.
             "serviceTiers": id == "sonnet" ? [["id": "default", "label": "Standard", "description": "Standard speed, standard usage"],
                                               ["id": "fast", "label": "Fast", "description": "Faster output, higher usage"]] : [],
             "defaultServiceTier": id == "sonnet" ? "default" : NSNull()]
        }
    }
}
#endif
