//! The provider registry: one descriptor per agent harness holding every fact
//! that differs between providers. Callers ask the descriptor instead of
//! branching on the family, and every `match` here is exhaustive, so adding a
//! provider fails to compile exactly where a decision is needed.
//!
//! A conversation's family comes from the provider that listed its model or
//! from its stored binding. Nothing infers it from a model id.

use crate::AgentFamily;

/// Which identifier a project binding shares with the provider's own session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeSessionKey {
    /// The runtime thread id is the provider's session.
    ThreadId,
    /// The runtime thread is Wonder's bridge; its session id is the provider's.
    BridgeSessionId,
}

/// Which optional conversation features a provider offers, as Wonder's
/// integration actually delivers them today. This is the one record the host
/// gates on and publishes to the phone (`GET /api/v1/projects`, per family), so
/// neither decides a feature by provider name. Each field names what Wonder does
/// when it is false. Bump [`CAPABILITIES_VERSION`] when a field is added and
/// keep it optional on the phone, which falls back to its legacy record for
/// hosts that send none (`ProviderCapabilities.legacy` in WonderPairing).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    /// Stop ends a running reply. False: no Stop; the reply runs to its end.
    pub interrupt: bool,
    /// Send now (Guide): the phone adds a message to a running reply. False:
    /// the message waits in the queue and is sent when the reply ends.
    pub steer: bool,
    /// A Project agent's `wonder_thread_send` may use mode `steer`. False: the
    /// tool refuses with `steer_unsupported` and the agent queues instead.
    pub agent_steer: bool,
    /// Wonder holds a message sent during a reply and sends it when the reply
    /// ends. False: sending is disabled while a reply runs.
    pub queue: bool,
    /// A thread may change model between turns of one session. False: the
    /// model is fixed once the thread starts.
    pub model_switch: bool,
    /// How reliably the runtime reports that a turn ended.
    pub terminal_status: TerminalStatus,
    /// The provider can copy a session's history, up to a chosen turn, into a
    /// new session that continues independently. False: no Fork action.
    pub native_fork: bool,
    /// Edit from here: the provider's own history can be cut back to before
    /// one of the owner's messages (`POST .../rewind`), which then returns
    /// to the composer. False: no Edit from here action.
    pub rewind: bool,
    /// A rewind can also restore the files the agent edited after that
    /// message, from backups the provider keeps once the owner turns them on
    /// (`ProviderSetting::FileCheckpointing`, Settings > Providers). False:
    /// a rewind changes only the conversation and says files are unchanged.
    pub rewind_files: bool,
    /// Sessions started on the Mac outside Wonder can be listed, attached and
    /// opened from shortcuts. False: only Wonder-created threads are listed.
    pub native_sessions: bool,
    /// Where archiving a thread is recorded.
    pub archive: ArchiveStyle,
    /// Files and images can be attached to a message. False: no attach action.
    /// A provider may still refuse a type its own setup cannot read.
    pub attachments: bool,
    /// Wonder serves the provider's native slash-command catalog to the
    /// phone (`GET /api/v1/conversations/{id}/commands`). False: no command
    /// list; typed `/commands` are still sent as text.
    pub slash_commands: bool,
    /// `@` in the composer searches the Project's files
    /// (`.../workspace/search`) and inserts the provider's mention for the
    /// chosen path ([`ProviderDescriptor::file_mention`]). False: no file list.
    pub file_mentions: bool,
    /// The typed skills picker (Project skills sent as native skill input).
    /// False: no picker, and a message carrying skills is refused.
    pub skills: bool,
    /// Replies can end with a suggested next prompt. False: none is shown.
    pub prompt_suggestions: bool,
    /// The harness accepts per-thread access controls from Wonder. False: the
    /// owner's own provider settings apply and the menu says so.
    pub access_choices: bool,
    /// The agent can ask structured questions the phone answers. False:
    /// questions arrive as plain reply text.
    pub structured_questions: bool,
    /// Declining a command can carry an instruction for the agent. False:
    /// Decline sends no message.
    pub deny_with_instruction: bool,
    /// How plan mode is chosen.
    pub plan: PlanStyle,
    /// Where the provider's agent tasks (subagents) come from.
    pub agent_tasks: AgentTasks,
    /// Subagents have human-readable names (Codex nicknames). Without them an
    /// avatar of initials is noise, so Wonder shows the task and a status glyph.
    pub named_subagents: bool,
    /// The provider's Project agents get Wonder's thread tools (list, read,
    /// send, wait, delegate) and can be chosen as a delegate. False: no tools,
    /// and `wonder_delegate` does not offer the provider.
    pub thread_tools: bool,
    /// The thread can hold a long-running goal. False: no goal controls.
    pub goals: bool,
    /// The agent can generate images. False: no image generation is offered.
    pub image_generation: bool,
    /// The provider reports account usage limits. False: no usage display,
    /// and sending is never blocked on a usage window.
    pub usage_limits: bool,
    /// The provider lists connected apps (connectors) for a thread. False: it
    /// is not offered in the Connections picker.
    pub connected_apps: bool,
    /// Opening a session started on the Mac re-attaches it so the provider's
    /// desktop app keeps it in the Project. False: opening reads saved history
    /// only.
    pub desktop_project_sync: bool,
    /// Wonder's own skill pack (what changes when the owner is on the phone)
    /// can be turned on for Wonder's sessions, with a phone signal on turns
    /// written there. False: Settings says the skills do not apply to it.
    pub wonder_skills: bool,
    /// Retained for older clients. Added accounts are no longer supported.
    pub accounts: bool,
    /// Retained for older clients. Provider sign-in happens on the computer.
    pub account_sign_in_on_phone: bool,
    /// How message search reaches this provider's sessions started on the
    /// Mac outside Wonder. Wonder's own messages are always searched.
    pub history_search: HistorySearch,
    /// The harness's own browser control, which the phone offers in the
    /// computer preview when its tools run.
    pub browser_use: BrowserUse,
    /// What a reply can leave running that a thread row shows as background
    /// work (`backgroundWork` in the Project lists). `None`: never shown.
    pub background_work: BackgroundWork,
}

/// Where Wonder learns that work outlived a thread's reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundWork {
    /// Nothing is reported once a reply ends.
    None,
    /// Background terminals the runtime keeps per loaded thread.
    Terminals,
    /// Background agents, commands and monitors the session's process keeps:
    /// Wonder's runtime roster and task list, and the live session records
    /// of the provider's apps on the Mac.
    Tasks,
}

/// Version of the published [`ProviderCapabilities`] record.
pub const CAPABILITIES_VERSION: u8 = 1;

/// How reliably a runtime reports the end of a turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalStatus {
    /// The native runtime reports every turn's outcome.
    Strong,
    /// An outcome can stay unconfirmed (ACP cancel is a notification), so a
    /// Stop or reply can end as uncertain and is shown that way.
    Weak,
}

/// Where archiving a thread is recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ArchiveStyle {
    /// In the provider's own history, so its desktop app agrees.
    Provider,
    /// In Wonder only; the provider's history is unchanged.
    Wonder,
}

/// How a thread enters plan mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanStyle {
    /// No plan mode: no plan control is shown.
    Unavailable,
    /// A separate toggle kept between turns alongside the access choice.
    Toggle,
    /// One of the provider's permission modes, chosen in the access menu;
    /// choosing another mode leaves it.
    PermissionMode,
}

/// Where a provider's agent tasks come from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentTasks {
    /// None are reported: no agent task list.
    None,
    /// Native subagent threads the runtime lists, archives and pages.
    NativeThreads,
    /// Tasks and background commands read from the session transcript; they
    /// can be stopped one by one but have no archive or paging.
    Transcript,
}

/// A harness's own browser control.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserUse {
    /// None of its own. Computer use, where offered, may still drive a browser.
    None,
    /// Claude in Chrome, when the owner set it up for Claude Code: its tools
    /// ask for approval like other MCP tools, and the phone shows the Chrome
    /// window holding Claude's tab (resolved through Chrome's AppleScript tab
    /// IDs, which equal the extension's).
    NativeChrome,
}

/// How message search reaches a provider's native history.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HistorySearch {
    /// Not searched: only what Wonder stored itself.
    None,
    /// The runtime's own cross-thread search (Codex `thread/search`), with
    /// in-thread occurrences when the runtime offers them.
    Runtime,
    /// A local full-text index of the provider's transcript files on this
    /// Mac, rebuilt from them at any time.
    TranscriptIndex,
}

impl ProviderCapabilities {
    /// Whether the provider has a plan mode a thread can stay in between turns.
    pub const fn plan_mode(&self) -> bool {
        !matches!(self.plan, PlanStyle::Unavailable)
    }
}

/// Who enforces a thread's approval choice. A stored approval is a promise
/// only where Wonder sets it on the session; elsewhere it is a placeholder,
/// and nothing may read it as Ask.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalEnforcement {
    /// Wonder sets the stored approval on every session (Codex's approval
    /// policy, the Claude bridge's policy), so Ask shows a card before each
    /// action the mode covers.
    Session,
    /// The agent's own settings on the Mac decide. ACP offers no per-session
    /// permission setter and Wonder invents none, so a card appears only for
    /// an action the agent itself asks about. The stored value is the neutral
    /// placeholder older hosts wrote and guarantees nothing.
    Native,
}

/// A thread's approval as Wonder can vouch for it, decoded from the stored
/// `claude_approval` column by [`AgentFamily::permission_profile`]. Ids
/// match the ACP runtime's profiles (`services/acp-runtime/catalog.mjs`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionProfile {
    /// A card before every action the provider's Ask covers.
    Ask,
    /// File edits in the workspace run without a card; other actions ask.
    AcceptEdits,
    /// Actions inside the sandbox run without a card.
    Auto,
    /// Whatever the agent's own settings allow; nothing Wonder enforces.
    Native,
}

impl PermissionProfile {
    /// How much the agent may do without a card, narrowest first. `Native`
    /// is the widest: nothing bounds the agent's own settings, so an
    /// authority comparison never treats it as Ask.
    pub const fn breadth(self) -> u8 {
        match self {
            Self::Ask => 0,
            Self::AcceptEdits => 1,
            Self::Auto => 2,
            Self::Native => 3,
        }
    }
}

/// How a chosen Project file is written into a message. Only forms the
/// provider understands natively, or plain text the owner could have typed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileMention {
    /// No file list is offered.
    None,
    /// `@path` (or `@"path with spaces"`), which Claude Code resolves itself
    /// and attaches the file or folder listing to the prompt.
    AtPath,
    /// The path as ordinary message text; the agent opens it with its own
    /// tools. Used where the provider's input has no file mention item.
    PlainText,
}

/// How Wonder's skill pack and the phone signal reach one provider's
/// sessions. Nothing is sent while every skill is off.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WonderSkillDelivery {
    /// Codex App Server: Wonder's skill folder as the runtime's extra skill
    /// root (`skills/extraRoots/set`, held in the runtime's memory only), and
    /// the phone signal as `turn/start.additionalContext`.
    AppServerRoot,
    /// Wonder's Claude bridge: the skill folder as a local plugin loaded per
    /// session with only Wonder's skills enabled, and the phone signal as one
    /// line of the turn's input (Claude Code has no per-turn context field).
    BridgePlugin,
}

/// How the owner's custom instruction reaches one provider's sessions.
/// Nothing is sent while it is off.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionDelivery {
    /// Codex App Server: an application entry in each Wonder turn's
    /// `turn/start.additionalContext` (with the phone signal's), so the
    /// owner's own `developer_instructions` stay as they are.
    TurnContext,
    /// Wonder's Claude bridge: appended to Claude Code's system prompt for
    /// the process that runs the turn (`systemPrompt.append`).
    BridgeAppend,
}

/// How a thread's speed is chosen when nothing is stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefaultSpeed {
    /// The standard tier, named `default`.
    Standard,
    /// The tier the model's own catalog entry names.
    CatalogDefault,
}

#[derive(Clone, Copy, Debug)]
pub struct ProviderDescriptor {
    pub family: AgentFamily,
    pub display_name: &'static str,
    /// Prefix of every runtime thread id this provider issues, when it has one.
    pub thread_id_prefix: Option<&'static str>,
    /// Namespace the provider puts on the model ids it lists, when it has one.
    /// It keeps providers from shadowing each other in the shared catalog; it
    /// is never used to decide which provider owns a model.
    pub model_id_namespace: Option<&'static str>,
    pub capabilities: ProviderCapabilities,
    /// How a file chosen after `@` is inserted. `capabilities.file_mentions`
    /// is true exactly when this is not [`FileMention::None`].
    pub file_mention: FileMention,
    /// Approval mode stored on a new thread (`claude_approval` column).
    pub default_approval: &'static str,
    /// Provider-specific approval modes a thread may store.
    pub approval_modes: &'static [&'static str],
    /// Whether Wonder enforces the stored approval or the agent's own
    /// settings do; see [`AgentFamily::permission_profile`].
    pub approval_enforcement: ApprovalEnforcement,
    pub default_speed: DefaultSpeed,
    pub native_session: NativeSessionKey,
    /// How Wonder's skill pack is delivered; None when it cannot be.
    pub skill_delivery: Option<WonderSkillDelivery>,
    /// How the owner's custom instruction is delivered; None when the
    /// harness has no way to take one (OpenCode's ACP has no system prompt).
    pub instruction_delivery: Option<InstructionDelivery>,
    /// A cold chat shows the last command list a live session reported in
    /// its account and folder (`cached`, possibly out of date); false: a cold
    /// chat has no list.
    pub saved_commands: bool,
}

const CODEX: ProviderDescriptor = ProviderDescriptor {
    family: AgentFamily::Codex,
    display_name: "Codex",
    thread_id_prefix: None,
    model_id_namespace: None,
    capabilities: ProviderCapabilities {
        interrupt: true,
        steer: true,
        agent_steer: true,
        queue: true,
        model_switch: true,
        terminal_status: TerminalStatus::Strong,
        native_fork: true,
        // `thread/revert` replaces the thread's durable history with the
        // turns before one; it does not revert local file changes.
        rewind: true,
        rewind_files: false,
        native_sessions: true,
        archive: ArchiveStyle::Provider,
        attachments: true,
        // Cupola dispatches the native compact/review actions; skills keep their own picker.
        slash_commands: true,
        // Its input has no file mention item (`mention` is for plugins), so
        // the path is written as text, as the owner would type it.
        file_mentions: true,
        skills: true,
        prompt_suggestions: false,
        access_choices: true,
        structured_questions: true,
        deny_with_instruction: false,
        plan: PlanStyle::Toggle,
        agent_tasks: AgentTasks::NativeThreads,
        named_subagents: true,
        thread_tools: true,
        goals: true,
        image_generation: true,
        usage_limits: true,
        connected_apps: true,
        desktop_project_sync: true,
        wonder_skills: true,
        accounts: false,
        account_sign_in_on_phone: false,
        history_search: HistorySearch::Runtime,
        browser_use: BrowserUse::None,
        background_work: BackgroundWork::Terminals,
    },
    file_mention: FileMention::PlainText,
    default_approval: "ask",
    approval_modes: &["ask"],
    approval_enforcement: ApprovalEnforcement::Session,
    default_speed: DefaultSpeed::Standard,
    native_session: NativeSessionKey::ThreadId,
    skill_delivery: Some(WonderSkillDelivery::AppServerRoot),
    instruction_delivery: Some(InstructionDelivery::TurnContext),
    saved_commands: false,
};

const CLAUDE: ProviderDescriptor = ProviderDescriptor {
    family: AgentFamily::Claude,
    display_name: "Claude",
    thread_id_prefix: Some("claude-"),
    model_id_namespace: Some("claude:"),
    capabilities: ProviderCapabilities {
        interrupt: true,
        // The bridge keeps a Project reply's process open and takes Guide
        // into it (`turn/steer`, see `claude_send_now_tests`).
        steer: true,
        // `wonder_thread_send` steer predates Claude Guide and still refuses.
        agent_steer: false,
        queue: true,
        model_switch: true,
        terminal_status: TerminalStatus::Strong,
        native_fork: true,
        // The bridge resumes the session at the entry before the message
        // (`resumeSessionAt`) and, with checkpointing on, restores files
        // through the SDK's `rewindFiles`.
        rewind: true,
        rewind_files: true,
        native_sessions: true,
        archive: ArchiveStyle::Wonder,
        attachments: true,
        // The bridge retains the live session's native catalog
        // (`thread/commands/list`); a cold session's is `saved_commands`.
        slash_commands: true,
        // Claude Code resolves `@path` in a prompt natively.
        file_mentions: true,
        skills: false,
        prompt_suggestions: true,
        access_choices: true,
        structured_questions: true,
        // The bridge offers it on Bash approvals (`supportsDenyMessage`).
        deny_with_instruction: true,
        plan: PlanStyle::PermissionMode,
        agent_tasks: AgentTasks::Transcript,
        named_subagents: false,
        thread_tools: true,
        goals: false,
        image_generation: false,
        usage_limits: true,
        connected_apps: true,
        desktop_project_sync: false,
        wonder_skills: true,
        accounts: false,
        account_sign_in_on_phone: false,
        // Claude Code has no search API; its JSONL transcripts are indexed.
        history_search: HistorySearch::TranscriptIndex,
        browser_use: BrowserUse::NativeChrome,
        background_work: BackgroundWork::Tasks,
    },
    file_mention: FileMention::AtPath,
    default_approval: "auto",
    approval_modes: &["ask", "accept_edits", "auto"],
    approval_enforcement: ApprovalEnforcement::Session,
    default_speed: DefaultSpeed::CatalogDefault,
    native_session: NativeSessionKey::BridgeSessionId,
    skill_delivery: Some(WonderSkillDelivery::BridgePlugin),
    instruction_delivery: Some(InstructionDelivery::BridgeAppend),
    saved_commands: true,
};

// OpenCode runs through Wonder's ACP bridge (services/acp-runtime), which
// speaks the Claude bridge's dialect. It is experimental and uses the owner's
// installed OpenCode and configured providers.
const OPENCODE: ProviderDescriptor = ProviderDescriptor {
    family: AgentFamily::OpenCode,
    display_name: "OpenCode",
    thread_id_prefix: Some("opencode-"),
    model_id_namespace: Some("opencode:"),
    capabilities: ProviderCapabilities {
        // ACP cancel is confirmed by the bridge before Stop succeeds.
        interrupt: true,
        // ACP has no way to add input to a running prompt.
        steer: false,
        agent_steer: false,
        queue: true,
        model_switch: true,
        terminal_status: TerminalStatus::Weak,
        // OpenCode's ACP fork is unstable; Wonder does not offer it yet.
        native_fork: false,
        // ACP has no way to remove turns from a session.
        rewind: false,
        rewind_files: false,
        // Its desktop sessions cannot be attached by the host yet.
        native_sessions: false,
        archive: ArchiveStyle::Wonder,
        // Forwarded as ACP content; a setup that cannot read a type refuses it.
        attachments: true,
        // The bridge retains the live session's ACP command catalog
        // (`available_commands_update`); a cold session reports it unavailable.
        slash_commands: true,
        // ACP's native mention is a `resource_link` content block, which
        // Wonder's message input cannot carry yet. No text stand-in.
        file_mentions: false,
        skills: false,
        prompt_suggestions: false,
        // ACP leaves permissions in the owner's native OpenCode configuration.
        access_choices: false,
        structured_questions: false,
        deny_with_instruction: false,
        // OpenCode's Plan agent, kept as a toggle.
        plan: PlanStyle::Toggle,
        agent_tasks: AgentTasks::None,
        named_subagents: false,
        // Offered through a per-session MCP server in ACP `mcpServers`.
        thread_tools: true,
        goals: false,
        image_generation: false,
        usage_limits: false,
        connected_apps: false,
        desktop_project_sync: false,
        // ACP has no per-session skill folder; OpenCode reads skills only
        // from the owner's own configuration, which Wonder does not change.
        wonder_skills: false,
        accounts: false,
        account_sign_in_on_phone: false,
        history_search: HistorySearch::None,
        browser_use: BrowserUse::None,
        background_work: BackgroundWork::None,
    },
    file_mention: FileMention::None,
    default_approval: "ask",
    approval_modes: &["ask"],
    approval_enforcement: ApprovalEnforcement::Native,
    default_speed: DefaultSpeed::Standard,
    native_session: NativeSessionKey::BridgeSessionId,
    skill_delivery: None,
    instruction_delivery: None,
    saved_commands: false,
};

// Cursor uses the same ACP lifecycle. List its capabilities explicitly so a
// new OpenCode capability cannot silently enable it for Cursor.
const CURSOR: ProviderDescriptor = ProviderDescriptor {
    family: AgentFamily::Cursor,
    display_name: "Cursor",
    thread_id_prefix: Some("cursor-"),
    model_id_namespace: Some("cursor:"),
    capabilities: ProviderCapabilities {
        // ACP cancel is confirmed by the bridge before Stop succeeds.
        interrupt: true,
        // ACP has no way to add input to a running prompt.
        steer: false,
        agent_steer: false,
        queue: true,
        model_switch: true,
        terminal_status: TerminalStatus::Weak,
        // The shared bridge does not expose Cursor session forking.
        native_fork: false,
        // ACP has no way to remove turns from a session.
        rewind: false,
        rewind_files: false,
        // Its desktop sessions cannot be attached by the host yet.
        native_sessions: false,
        archive: ArchiveStyle::Wonder,
        // Forwarded as ACP content; a setup that cannot read a type refuses it.
        attachments: true,
        // The bridge retains the live session's ACP command catalog
        // (`available_commands_update`); a cold session reports it unavailable.
        slash_commands: true,
        // ACP's native mention is a `resource_link` content block, which
        // Wonder's message input cannot carry yet. No text stand-in.
        file_mentions: false,
        skills: false,
        prompt_suggestions: false,
        // ACP leaves permissions in the owner's native Cursor configuration.
        access_choices: false,
        structured_questions: false,
        deny_with_instruction: false,
        // Cursor's native Plan mode, kept as a toggle.
        plan: PlanStyle::Toggle,
        agent_tasks: AgentTasks::None,
        named_subagents: false,
        // Offered through a per-session MCP server in ACP `mcpServers`, as
        // for OpenCode; Cursor keeps one MCP client per session.
        thread_tools: true,
        goals: false,
        image_generation: false,
        usage_limits: false,
        connected_apps: false,
        desktop_project_sync: false,
        // Wonder does not add a skill pack through Cursor's ACP.
        wonder_skills: false,
        accounts: false,
        account_sign_in_on_phone: false,
        history_search: HistorySearch::None,
        browser_use: BrowserUse::None,
        background_work: BackgroundWork::None,
    },
    file_mention: FileMention::None,
    default_approval: "ask",
    approval_modes: &["ask"],
    approval_enforcement: ApprovalEnforcement::Native,
    default_speed: DefaultSpeed::Standard,
    native_session: NativeSessionKey::BridgeSessionId,
    skill_delivery: None,
    instruction_delivery: None,
    saved_commands: false,
};

impl AgentFamily {
    pub const ALL: [AgentFamily; 4] = [
        AgentFamily::Codex,
        AgentFamily::Claude,
        AgentFamily::OpenCode,
        AgentFamily::Cursor,
    ];

    pub const fn provider(self) -> &'static ProviderDescriptor {
        match self {
            Self::Codex => &CODEX,
            Self::Claude => &CLAUDE,
            Self::OpenCode => &OPENCODE,
            Self::Cursor => &CURSOR,
        }
    }

    /// Whether this provider could have issued `thread`. A provider without a
    /// prefix owns every id that no other provider's prefix claims.
    pub fn accepts_thread_id(self, thread: &str) -> bool {
        match self.provider().thread_id_prefix {
            Some(prefix) => thread.starts_with(prefix),
            None => !Self::ALL.iter().any(|other| {
                other
                    .provider()
                    .thread_id_prefix
                    .is_some_and(|prefix| thread.starts_with(prefix))
            }),
        }
    }

    /// Whether Cupola sets this provider's permissions per thread. False
    /// (OpenCode, Cursor): the owner's own settings in the provider decide,
    /// and Cupola cannot hold a thread to Ask or Read only.
    pub fn permissions_enforced(self) -> bool {
        self.provider().capabilities.access_choices
    }

    /// Whether threads of this provider choose among approval modes; others
    /// keep the neutral stored value.
    pub fn allows_approval_choice(self) -> bool {
        let provider = self.provider();
        provider.approval_enforcement == ApprovalEnforcement::Session
            && provider.approval_modes.len() > 1
    }

    /// What a thread's stored approval stands for on this provider. Where
    /// the agent's own settings decide, the neutral value older hosts stored
    /// (`ask`) is [`PermissionProfile::Native`], so existing threads keep
    /// their native behaviour and are never read as an enforced Ask. An
    /// absent value (a row or request from an older host) is the narrowest
    /// profile, never the provider's default, and a value this provider
    /// does not store is refused rather than mapped onto another profile.
    pub fn permission_profile(self, stored: Option<&str>) -> Option<PermissionProfile> {
        let provider = self.provider();
        match provider.approval_enforcement {
            ApprovalEnforcement::Native => stored
                .is_none_or(|value| value == provider.default_approval)
                .then_some(PermissionProfile::Native),
            ApprovalEnforcement::Session => {
                let value = stored.unwrap_or("ask");
                if !provider.approval_modes.contains(&value) {
                    return None;
                }
                match value {
                    "ask" => Some(PermissionProfile::Ask),
                    "accept_edits" => Some(PermissionProfile::AcceptEdits),
                    "auto" => Some(PermissionProfile::Auto),
                    _ => None,
                }
            }
        }
    }

    /// Whether any provider stores `mode` as a thread approval.
    pub fn is_known_approval_mode(mode: &str) -> bool {
        Self::ALL
            .iter()
            .any(|family| family.provider().approval_modes.contains(&mode))
    }

    /// The provider that issued `thread`, for ids seen without a binding.
    pub fn issuing_thread(thread: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|family| family.accepts_thread_id(thread))
            .unwrap_or_default()
    }

    /// Whether `model` sits in a namespace another provider reserves.
    pub fn model_in_foreign_namespace(self, model: &str) -> bool {
        Self::ALL.iter().any(|other| {
            *other != self
                && other
                    .provider()
                    .model_id_namespace
                    .is_some_and(|namespace| model.starts_with(namespace))
        })
    }

    /// Whether `model` carries this provider's own namespace, if it has one.
    pub fn model_in_own_namespace(self, model: &str) -> bool {
        self.provider()
            .model_id_namespace
            .is_none_or(|namespace| model.starts_with(namespace))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_describe_their_own_family() {
        for family in AgentFamily::ALL {
            assert_eq!(family.provider().family, family);
            // The phone is told about Wonder's skills exactly where they can be delivered.
            assert_eq!(
                family.provider().capabilities.wonder_skills,
                family.provider().skill_delivery.is_some()
            );
        }
    }

    // Contract: file restore is only published where rewind itself is.
    #[test]
    fn file_restore_needs_rewind() {
        for family in AgentFamily::ALL {
            let capabilities = family.provider().capabilities;
            assert!(
                !capabilities.rewind_files || capabilities.rewind,
                "{family:?}"
            );
        }
    }

    // Contract: the published file-mention flag and the host's insertion form
    // cannot disagree, so the phone never offers a list the host refuses.
    #[test]
    fn file_mention_flag_follows_the_insertion_form() {
        for family in AgentFamily::ALL {
            let provider = family.provider();
            assert_eq!(
                provider.capabilities.file_mentions,
                provider.file_mention != FileMention::None,
                "{family:?}"
            );
        }
    }

    #[test]
    fn thread_identity_follows_the_issuing_provider() {
        assert!(AgentFamily::Claude.accepts_thread_id("claude-abc"));
        assert!(!AgentFamily::Claude.accepts_thread_id("abc"));
        assert!(AgentFamily::Codex.accepts_thread_id("abc"));
        assert!(!AgentFamily::Codex.accepts_thread_id("claude-abc"));
        assert_eq!(
            AgentFamily::issuing_thread("claude-abc"),
            AgentFamily::Claude
        );
        assert_eq!(AgentFamily::issuing_thread("abc"), AgentFamily::Codex);
        assert_eq!(
            AgentFamily::issuing_thread("opencode-abc"),
            AgentFamily::OpenCode
        );
        assert_eq!(
            AgentFamily::issuing_thread("cursor-abc"),
            AgentFamily::Cursor
        );
        assert!(!AgentFamily::Codex.accepts_thread_id("cursor-abc"));
        assert!(!AgentFamily::OpenCode.accepts_thread_id("cursor-abc"));
        assert!(AgentFamily::Cursor.model_in_own_namespace("cursor:gpt-6-luna[reasoning=low]"));
        assert!(AgentFamily::OpenCode.model_in_foreign_namespace("cursor:default[]"));
        assert!(!AgentFamily::Codex.accepts_thread_id("opencode-abc"));
        assert!(AgentFamily::Codex.model_in_foreign_namespace("opencode:opencode/big-pickle"));
    }

    // Contract: a stored approval or model is accepted only where some
    // provider issues it, so a typo or another provider's model never routes.
    #[test]
    fn approval_modes_and_model_namespaces_are_owned() {
        assert!(AgentFamily::is_known_approval_mode("ask"));
        assert!(AgentFamily::is_known_approval_mode("accept_edits"));
        assert!(!AgentFamily::is_known_approval_mode("bypass"));
        assert!(AgentFamily::Claude.model_in_own_namespace("claude:opus"));
        assert!(!AgentFamily::Claude.model_in_own_namespace("gpt-5.4"));
        assert!(!AgentFamily::OpenCode.model_in_own_namespace("claude:opus"));
        assert!(AgentFamily::Codex.model_in_own_namespace("gpt-5.4"));
    }

    // Contract: a stored approval decodes to what Wonder can vouch for. Where
    // the agent's own settings decide (OpenCode, Cursor), the `ask` older
    // hosts stored is Native, never an enforced Ask; an absent value is the
    // narrowest profile, not the provider's default (Claude's Auto); and a
    // value the provider does not store is refused, never approximated.
    #[test]
    fn stored_approval_decodes_to_an_enforceable_profile() {
        use PermissionProfile::*;
        let cases: &[(AgentFamily, Option<&str>, Option<PermissionProfile>)] = &[
            (AgentFamily::OpenCode, Some("ask"), Some(Native)),
            (AgentFamily::OpenCode, None, Some(Native)),
            (AgentFamily::OpenCode, Some("auto"), None),
            (AgentFamily::OpenCode, Some("accept_edits"), None),
            (AgentFamily::OpenCode, Some("native"), None),
            (AgentFamily::Cursor, Some("ask"), Some(Native)),
            (AgentFamily::Cursor, None, Some(Native)),
            (AgentFamily::Cursor, Some("auto"), None),
            (AgentFamily::Cursor, Some(""), None),
            (AgentFamily::Claude, Some("ask"), Some(Ask)),
            (AgentFamily::Claude, Some("accept_edits"), Some(AcceptEdits)),
            (AgentFamily::Claude, Some("auto"), Some(Auto)),
            (AgentFamily::Claude, None, Some(Ask)),
            (AgentFamily::Claude, Some("bypass"), None),
            (AgentFamily::Claude, Some("full_access"), None),
            (AgentFamily::Claude, Some("Ask"), None),
            (AgentFamily::Claude, Some("native"), None),
            (AgentFamily::Codex, Some("ask"), Some(Ask)),
            (AgentFamily::Codex, None, Some(Ask)),
            (AgentFamily::Codex, Some("auto"), None),
        ];
        for &(family, stored, expected) in cases {
            assert_eq!(
                family.permission_profile(stored),
                expected,
                "{family:?} {stored:?}"
            );
        }
    }

    // Contract: no provider's stored values or defaults widen silently. Every
    // value a provider stores, its default included, decodes; an absent value
    // is never wider than any of them; and a natively decided thread ranks
    // wider than every enforced profile, so it is never compared as Ask.
    #[test]
    fn permission_profiles_never_widen_silently() {
        for family in AgentFamily::ALL {
            let provider = family.provider();
            let absent = family.permission_profile(None).expect("absent decodes");
            assert!(
                family
                    .permission_profile(Some(provider.default_approval))
                    .is_some(),
                "{family:?} default"
            );
            for mode in provider.approval_modes {
                let profile = family
                    .permission_profile(Some(mode))
                    .expect("stored mode decodes");
                assert!(absent.breadth() <= profile.breadth(), "{family:?} {mode}");
                assert_eq!(
                    profile == PermissionProfile::Native,
                    provider.approval_enforcement == ApprovalEnforcement::Native,
                    "{family:?} {mode}"
                );
            }
        }
        for enforced in [
            PermissionProfile::Ask,
            PermissionProfile::AcceptEdits,
            PermissionProfile::Auto,
        ] {
            assert!(PermissionProfile::Native.breadth() > enforced.breadth());
        }
    }

    // Contract: Wonder enforces approval exactly where it sets per-thread
    // access, and a natively decided provider stores only its neutral
    // placeholder and offers no approval choice.
    #[test]
    fn approval_enforcement_follows_access_choices() {
        for family in AgentFamily::ALL {
            let provider = family.provider();
            let native = provider.approval_enforcement == ApprovalEnforcement::Native;
            assert_eq!(native, !provider.capabilities.access_choices, "{family:?}");
            if native {
                assert_eq!(provider.approval_modes, [provider.default_approval]);
                assert!(!family.allows_approval_choice(), "{family:?}");
            }
        }
    }

    // Contract: a provider without a plan style never shows a plan control.
    #[test]
    fn plan_mode_follows_the_plan_style() {
        let capabilities = AgentFamily::Codex.provider().capabilities;
        for (plan, expected) in [
            (PlanStyle::Unavailable, false),
            (PlanStyle::Toggle, true),
            (PlanStyle::PermissionMode, true),
        ] {
            assert_eq!(
                ProviderCapabilities {
                    plan,
                    ..capabilities
                }
                .plan_mode(),
                expected
            );
        }
    }
}
