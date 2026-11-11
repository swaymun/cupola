import Foundation

/// What one provider offers through Wonder, as the Mac publishes it for each
/// family in `GET /api/v1/projects` (`families[i].capabilities`, owned by
/// `ProviderCapabilities` in crates/wonder-store/src/provider.rs). Views ask
/// this record instead of comparing provider names. Each field's doc says
/// what the app does when it is false.
public struct ProviderCapabilities: Codable, Hashable, Sendable {
    /// How reliably the runtime reports that a turn ended.
    public enum TerminalStatus: String, Codable, Hashable, Sendable {
        case strong
        /// An outcome can stay unconfirmed; show Stop and replies as uncertain.
        case weak
        public init(from decoder: Decoder) throws { self = Self(rawValue: try String(from: decoder)) ?? .weak }
    }
    /// Where archiving a thread is recorded.
    public enum ArchiveStyle: String, Codable, Hashable, Sendable {
        /// In the provider's own history ("Archived in Codex").
        case provider
        /// In Wonder only ("Archived in Wonder").
        case wonder
        public init(from decoder: Decoder) throws { self = Self(rawValue: try String(from: decoder)) ?? .wonder }
    }
    /// How a thread enters plan mode.
    public enum PlanStyle: String, Codable, Hashable, Sendable {
        /// No plan control.
        case unavailable
        /// A Plan mode toggle beside the access choice.
        case toggle
        /// One of the access menu's permission modes; no separate toggle.
        case permissionMode
        public init(from decoder: Decoder) throws { self = Self(rawValue: try String(from: decoder)) ?? .unavailable }
    }
    /// Where the provider's agent tasks come from.
    public enum AgentTasks: String, Codable, Hashable, Sendable {
        /// None: no agent task list.
        case none
        /// Native subagent threads with archive and paging.
        case nativeThreads
        /// Transcript tasks, stoppable one by one, without archive or paging.
        case transcript
        public init(from decoder: Decoder) throws { self = Self(rawValue: try String(from: decoder)) ?? .none }
    }

    /// Stop ends a running reply. False: no Stop.
    public var interrupt: Bool
    /// Send now (Guide) into a running reply. False: the message is queued.
    public var steer: Bool
    /// A Project agent may steer another thread. Informational for the app.
    public var agentSteer: Bool
    /// The Mac holds a message sent during a reply. False: Send is disabled while a reply runs.
    public var queue: Bool
    /// The model can change between turns. False: the model picker is fixed after the first turn.
    public var modelSwitch: Bool
    public var terminalStatus: TerminalStatus
    /// Fork from a reply. False: no Fork action.
    public var nativeFork: Bool
    /// Edit from here on the owner's sent messages (the provider's history is
    /// cut back to before one). False: no Edit from here.
    public var rewind: Bool
    /// A rewind can also restore files the agent edited, once the owner turns
    /// on the provider's file backups in Settings. Optional: hosts before it
    /// send none, which means no file restore.
    public var rewindFiles: Bool?
    /// Sessions started on the Mac can be listed, attached and opened from
    /// shortcuts. False: only Wonder-created threads.
    public var nativeSessions: Bool
    public var archive: ArchiveStyle
    /// Files and images can be attached. False: no attach action.
    public var attachments: Bool
    /// A native slash-command catalog is served. False: no command picker.
    public var slashCommands: Bool
    /// `@file` mentions. False: no mention picker.
    public var fileMentions: Bool
    /// The typed skills picker. False: no picker; selected skills block sending.
    public var skills: Bool
    /// A suggested next prompt can follow a reply. False: none is shown.
    public var promptSuggestions: Bool
    /// Wonder's access choices apply. False: the menu shows the provider's own settings.
    public var accessChoices: Bool
    /// Structured questions the phone answers. False: questions are plain text.
    public var structuredQuestions: Bool
    /// Decline can carry an instruction. False: Decline sends no message.
    public var denyWithInstruction: Bool
    public var plan: PlanStyle
    public var agentTasks: AgentTasks
    /// Subagents have human-readable names. False: show the task and a status glyph.
    public var namedSubagents: Bool
    /// Project agents get Wonder's thread tools and can be delegated to.
    public var threadTools: Bool
    /// Thread goals. False: no goal controls.
    public var goals: Bool
    /// Image generation. False: none offered.
    public var imageGeneration: Bool
    /// Account usage limits. False: no usage display or usage gate.
    public var usageLimits: Bool
    /// Connected apps (connectors) can be listed. False: not in the Connections picker.
    public var connectedApps: Bool
    /// Opening a session started on the Mac re-attaches it so the desktop app keeps
    /// it in the Project. False: opening reads saved history only.
    public var desktopProjectSync: Bool
    /// Wonder's skill pack (what changes when the owner is on the phone) can be
    /// turned on for this provider. False: Settings does not list it among the
    /// agents the skills apply to.
    public var wonderSkills: Bool
    /// Legacy wire compatibility only. Current hosts send false; the phone
    /// never offers added-account management or routing, even for older hosts.
    public var accounts: Bool
    /// The harness's own browser control. `.nativeChrome`: Claude in Chrome's
    /// tool events offer the Chrome window holding Claude's tab in the computer
    /// preview. Optional: hosts before it send none, which means none.
    public var browserUse: BrowserUse?
    public enum BrowserUse: String, Codable, Hashable, Sendable {
        case none, nativeChrome
        public init(from decoder: Decoder) throws { self = Self(rawValue: try String(from: decoder)) ?? .none }
    }
    /// Approval modes a thread may store; more than one means a choice is offered.
    public var approvalModes: [String]

    public init(interrupt: Bool, steer: Bool, agentSteer: Bool, queue: Bool, modelSwitch: Bool,
                terminalStatus: TerminalStatus, nativeFork: Bool, rewind: Bool, nativeSessions: Bool,
                archive: ArchiveStyle, attachments: Bool, slashCommands: Bool, fileMentions: Bool,
                skills: Bool, promptSuggestions: Bool, accessChoices: Bool, structuredQuestions: Bool,
                denyWithInstruction: Bool, plan: PlanStyle, agentTasks: AgentTasks, namedSubagents: Bool,
                threadTools: Bool, goals: Bool, imageGeneration: Bool, usageLimits: Bool, connectedApps: Bool,
                desktopProjectSync: Bool, wonderSkills: Bool = false, accounts: Bool = false, rewindFiles: Bool? = nil,
                browserUse: BrowserUse? = nil, approvalModes: [String]) {
        self.interrupt = interrupt; self.steer = steer; self.agentSteer = agentSteer; self.queue = queue
        self.modelSwitch = modelSwitch; self.terminalStatus = terminalStatus; self.nativeFork = nativeFork
        self.rewind = rewind; self.nativeSessions = nativeSessions; self.archive = archive
        self.attachments = attachments; self.slashCommands = slashCommands; self.fileMentions = fileMentions
        self.skills = skills; self.promptSuggestions = promptSuggestions; self.accessChoices = accessChoices
        self.structuredQuestions = structuredQuestions; self.denyWithInstruction = denyWithInstruction
        self.plan = plan; self.agentTasks = agentTasks; self.namedSubagents = namedSubagents
        self.threadTools = threadTools; self.goals = goals; self.imageGeneration = imageGeneration
        self.usageLimits = usageLimits; self.connectedApps = connectedApps
        self.desktopProjectSync = desktopProjectSync; self.wonderSkills = wonderSkills; self.accounts = accounts
        self.rewindFiles = rewindFiles
        self.browserUse = browserUse
        self.approvalModes = approvalModes
    }

    /// The record for a family the host did not describe, or described in a
    /// shape this app cannot read: nothing beyond sending, so no control
    /// promises a feature the host has not offered.
    public static let unoffered = Self(
        interrupt: false, steer: false, agentSteer: false, queue: false, modelSwitch: false,
        terminalStatus: .weak, nativeFork: false, rewind: false, nativeSessions: false,
        archive: .wonder, attachments: false, slashCommands: false, fileMentions: false,
        skills: false, promptSuggestions: false, accessChoices: false, structuredQuestions: false,
        denyWithInstruction: false, plan: .unavailable, agentTasks: .none, namedSubagents: false,
        threadTools: false, goals: false, imageGeneration: false, usageLimits: false, connectedApps: false,
        desktopProjectSync: false, approvalModes: ["ask"])

    /// Whether a rewind can restore files (see `rewindFiles`).
    public var restoresFilesOnRewind: Bool { rewind && rewindFiles == true }
    /// Whether threads choose among approval modes.
    public var allowsApprovalChoice: Bool { approvalModes.count > 1 }
}

extension Array where Element == ProjectFamilyAvailability {
    /// The record the host sent for `family`, or `unoffered` when the host
    /// did not list the family.
    public func capabilities(for family: AgentFamily) -> ProviderCapabilities {
        first { $0.family == family }?.capabilities ?? .unoffered
    }
}
