//! The `wonder` command surface: its command tree and `--json` shapes.
//!
//! This module is platform-neutral (clap and serde only). The Linux, macOS
//! and Windows hosts build this same tree and print these same types; a
//! platform implements a command or answers it with [`Unavailable`].
//!
//! # Rules every platform follows
//!
//! - `--json` is global. With it, a command prints exactly one JSON object on
//!   stdout (`pair` and `chat` print one object per line, see below) and
//!   nothing else on stdout; prompts and progress go to stderr. Keys are
//!   camelCase. Fields may be added; existing fields are never renamed,
//!   retyped or removed.
//! - A failure under `--json` prints `{"error": {"code", "message"}}` on
//!   stdout. `code` is `unavailable` (exit 69: not on this platform yet, or
//!   needs something this machine lacks) or `failed` (exit 1). Usage errors
//!   are clap's text on stderr, exit 2.
//! - Without `--json`, output is for people: say what happened and the next
//!   step. Do not show protocol methods, IDs or transport detail unless the
//!   command exists to show them (`devices`, `status`).
//! - `doctor` exits 1 when any check failed, after printing its result.
//!
//! # Target shape
//!
//! | Command                     | `--json` stdout                                     |
//! |-----------------------------|-----------------------------------------------------|
//! | `init [options]`            | [`InitOutput`]                                      |
//! | `start [--foreground]`      | [`StartOutput`] (`--foreground` is text only)       |
//! | `stop`                      | [`StopOutput`]                                      |
//! | `status`                    | [`StatusOutput`]                                    |
//! | `logs [--lines N] [--follow]` | [`LogsOutput`] (`--follow` is text only)          |
//! | `pair [--timeout]`          | [`PairEvent`]: `offer`, `verification`, `connected` |
//! | `pair --cancel <id>`       | [`PairEvent::Cancelled`]                           |
//! | `pair --approve <deviceId> --code <text>` / `--deny <deviceId>` | [`PairEvent::Connected`] / [`PairEvent::Rejected`] |
//! | `devices`                   | [`DevicesOutput`]                                   |
//! | `devices revoke <id>`       | [`DeviceActionOutput`] (`state: revoked`)           |
//! | `devices forget <id>`       | [`DeviceActionOutput`] (`state: forgotten`)         |
//! | `devices rename <id> <name>` | [`DeviceActionOutput`] (`state: renamed`, `label`) |
//! | `providers`                 | [`ProvidersOutput`]                                 |
//! | `providers login <p>`  | [`ProviderLoginOutput`]                   |
//! | `config get <key>`          | [`ConfigOutput`]                                    |
//! | `config set <key> [value]`  | [`ConfigOutput`] (secrets come on stdin, never argv) |
//! | `doctor`                    | [`DoctorOutput`]                                    |
//! | `permissions [status]`      | [`PermissionsOutput`] (never prompts)               |
//! | `permissions grant`         | [`PermissionsOutput`] (after asking)                |
//! | `service install`           | [`ServiceOutput`]                                   |
//! | `service uninstall`         | [`ServiceOutput`]                                   |
//! | `service status`            | [`ServiceOutput`]                                   |
//! | `service restart`           | [`ServiceOutput`]                                   |
//! | `update [--check] [--channel C] [--version X] [--yes]` | [`UpdateOutput`]         |
//! | `triage [--lines N]`        | [`TriageOutput`]                                    |
//! | `version`                   | [`VersionOutput`]                                   |
//! | `chat -p M --project P [--thread ID\|--new] …` | [`ChatEvent`] per line, ending with `end` |
//! | `chat queue --thread ID`    | [`ChatQueueOutput`]                                 |
//! | `chat send-now ID --thread T` | [`ChatActionOutput`] (`state: sentNow`)          |
//! | `chat approve\|deny ID`     | [`ChatActionOutput`] (`approved`/`denied`)          |
//!
//! `<p>` is `codex`, `claude`, `opencode` or `cursor`. Config keys are `host-name` and
//! `voice-key` (agent settings and thread tools come later, as new keys).
//! `providers` reports installation and this computer's sign-in status.
//! `providers login <p>` runs the provider's own CLI in this terminal, then
//! reconnects Cupola to read the updated sign-in.
//!
//! Windows: `service` manages a logon task in the user's own session
//! (manager `windows`); `update` selects verified versioned binaries.
//! Linux: `service` is a systemd user unit (with lingering, so it also starts
//! at boot); `update` installs a SHA-256-verified release beside the old one
//! and switches `~/.local/bin`; `permissions` reports an empty list (Linux
//! asks for no system permissions); the daemon keeps a voice key in the macOS
//! Keychain or, on Linux, a file only this user can read. The contract tests
//! at the bottom pin every shape; change them only additively.

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::Value;

pub use wonderd::RELEASE_VERSION;

/// Exit status for [`Unavailable`] (sysexits `EX_UNAVAILABLE`).
pub const EXIT_UNAVAILABLE: i32 = 69;

macro_rules! environment_help {
    () => {
        "Environment: CUPOLA_DATA_DIR (default ~/.cupola), CUPOLA_CODEX_BIN, \
        CUPOLA_CLAUDE_BRIDGE, CUPOLA_OPENCODE_BRIDGE, CUPOLA_NODE_BIN, TAILSCALE_BIN, \
        CUPOLA_COMPUTER_PYTHON, CUPOLA_COMPUTER_BACKEND (python|native). The older \
        WONDER_* names still work. Data in ~/.wonder moves to ~/.cupola when the host \
        next starts; ~/.wonder stays as a link to it."
    };
}

/// Where the host lives and how it starts and upgrades (`cupola --help`).
#[cfg(target_os = "macos")]
macro_rules! install_help {
    () => {
        "\
Install:
  The host is the Cupola app in /Applications. The `cupola` command on your PATH
  (usually ~/.local/bin/cupola) is a link into that app, so keep the app where it is.
  `cupola service install` starts the host now and every time you sign in;
  `cupola start` runs it only until `cupola stop`, a restart or signing out.
  To upgrade, replace the app in /Applications with the new one, then run
  `cupola service restart` (or `cupola stop` and `cupola start`). Settings,
  conversations and paired phones stay; do not run `cupola init` again."
    };
}

#[cfg(not(target_os = "macos"))]
macro_rules! install_help {
    () => {
        "\
Install:
  `cupola service install` starts the host now and every time this computer starts
  or you sign in; `cupola start` runs it only until `cupola stop` or a restart.
  To upgrade, run `cupola update`: it installs the newer release beside this one
  and restarts the host. Settings, conversations and paired phones stay; do not
  run `cupola init` again."
    };
}

#[derive(Debug, Parser)]
#[command(
    name = "cupola",
    version = RELEASE_VERSION,
    about = "Run this computer as a Cupola host for your iPhone or iPad",
    long_about = "Run this computer as a Cupola host for your iPhone or iPad.\n\n\
        Set up once with `cupola init`, start the host with `cupola start`, then \
        connect your iPhone or iPad with `cupola pair`. Conversations, Projects \
        and provider sign-ins stay on this computer.",
    after_help = concat!(environment_help!(), "\n\nRun `cupola --help` for install and upgrade steps."),
    after_long_help = concat!(install_help!(), "\n\n", environment_help!())
)]
pub struct Cli {
    /// Print machine-readable JSON instead of text
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<HostCommand>,
}

#[derive(Debug, Subcommand)]
pub enum HostCommand {
    /// Set up this computer as a Cupola host
    #[command(after_long_help = "\
An iPhone or iPad on the same Wi-Fi connects without any tunnel (`cupola lan`);
a tunnel is only needed to reach this computer away from home.

Tunnels:
  tailscale  Tailscale Serve on this computer's MagicDNS name (default). Optional:
             it starts working once Tailscale is signed in with HTTPS certificates
             enabled, without setting up again. An occupied HTTPS port is never
             replaced; choose --https-port.
  none       No tunnel; --origin names an HTTPS address you route yourself. The
             host still listens only on loopback.

Computer view (--computer, experimental) lets your iPhone or iPad view and
control this computer. On Linux it shares the X11 desktop named by WONDER_COMPUTER_DISPLAY
(for example :0), or else runs a private virtual display that needs Xvfb and openbox; xdotool
and xclip add control and copy and paste. A Wayland desktop cannot be shared: sign in with an
X11 session. On Windows it shares the signed-in user's desktop. Archives
with native sharing (Linux and Windows x64) use it by default, without Python.
Archives without the native helper use Python with aiortc, mss and numpy
(WONDER_COMPUTER_PYTHON, default python3). WONDER_COMPUTER_BACKEND=python|native
selects explicitly.
Run `cupola doctor` to check the selected backend. Video goes
directly between phone and computer over the home network or a normal Tailscale
network interface; userspace Tailscale and NAT-only VM routes are not supported.")]
    Init(InitArgs),
    /// Start the Cupola host
    Start {
        /// Stay in this terminal instead of running in the background
        #[arg(long)]
        foreground: bool,
        /// Use this data directory, as CUPOLA_DATA_DIR does. The Windows
        /// logon task passes it because a task cannot carry environment.
        #[arg(long, value_name = "PATH", hide = true)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// Stop the Cupola host
    Stop,
    /// Remove Cupola's Tailscale route (`service uninstall` and `uninstall.sh` run this)
    #[command(hide = true)]
    Unpublish,
    /// Show whether the host is running and reachable
    Status,
    /// Show the host's recent log
    Logs {
        /// How many of the latest lines to show
        #[arg(long, value_name = "COUNT", default_value_t = 50)]
        lines: usize,
        /// Keep showing new lines until you press Ctrl-C
        #[arg(long)]
        follow: bool,
    },
    /// Connect an iPhone or iPad by showing a code to scan
    Pair {
        /// Approve the phone that claims this code without asking
        #[cfg(feature = "dev-pairing")]
        #[arg(long)]
        yes: bool,
        /// Give up waiting after this many seconds
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
        /// Cancel when the agent's input pipe closes (requires --yes)
        #[cfg(feature = "dev-pairing")]
        #[arg(long, requires = "yes")]
        watch_stdin: bool,
        /// Cancel only this offer, including its pending phone requests
        #[cfg_attr(feature = "dev-pairing", arg(long, value_name = "OFFER_ID", conflicts_with_all = ["yes", "timeout", "watch_stdin", "route", "qr", "approve", "deny"]))]
        #[cfg_attr(not(feature = "dev-pairing"), arg(long, value_name = "OFFER_ID", conflicts_with_all = ["timeout", "route", "qr", "approve", "deny"]))]
        cancel: Option<String>,
        /// Also write the QR code to this file (.svg, otherwise PNG); it is
        /// deleted when the offer ends and its path is in the JSON offer
        #[arg(long, value_name = "PATH", conflicts_with_all = ["approve", "deny"])]
        qr: Option<std::path::PathBuf>,
        /// Let the waiting phone connect using the text the person reads from
        /// their phone (use the deviceId from the verification)
        #[arg(requires = "code")]
        #[cfg_attr(feature = "dev-pairing", arg(long, value_name = "DEVICE_ID", conflicts_with_all = ["yes", "timeout", "watch_stdin", "route", "deny"]))]
        #[cfg_attr(not(feature = "dev-pairing"), arg(long, value_name = "DEVICE_ID", conflicts_with_all = ["timeout", "route", "deny"]))]
        approve: Option<String>,
        /// Verification text read by the person from their phone; must match
        /// exactly before --approve can connect it
        #[arg(long, value_name = "TEXT", requires = "approve", conflicts_with_all = ["deny", "cancel", "qr", "timeout", "route"])]
        #[cfg_attr(feature = "dev-pairing", arg(conflicts_with_all = ["yes", "watch_stdin"]))]
        code: Option<String>,
        /// Refuse the waiting phone
        #[cfg_attr(feature = "dev-pairing", arg(long, value_name = "DEVICE_ID", conflicts_with_all = ["yes", "timeout", "watch_stdin", "route"]))]
        #[cfg_attr(not(feature = "dev-pairing"), arg(long, value_name = "DEVICE_ID", conflicts_with_all = ["timeout", "route"]))]
        deny: Option<String>,
        /// Which way the iPhone or iPad reaches this computer while pairing
        /// (default: the host's main route)
        #[arg(long, value_enum, value_name = "ROUTE")]
        route: Option<PairRoute>,
    },
    /// Show every way an iPhone or iPad can reach this computer
    Routes,
    /// Let iPhones and iPads on the same network connect directly (on by default)
    #[command(after_long_help = "\
With the home network on (the default; `cupola lan off` turns it off), the host also listens on this computer's private
network addresses (home or office Wi-Fi and Ethernet), never on a public
address, a Tailscale address or a container bridge. Connections use TLS with
this computer's own key, which the iPhone or iPad checks; no certificate
authority is involved. A change applies when the host restarts.")]
    Lan {
        #[command(subcommand)]
        command: LanCommand,
    },
    /// Choose whether your iPhone or iPad may take control of this computer
    #[command(after_long_help = "\
Computer view (`cupola init --computer`) shows this computer on your iPhone or
iPad. With control on, Take control there moves the pointer and types here;
it also needs the Accessibility permission (`cupola permissions`). With control
off, the iPhone or iPad can only watch. A change applies the next time it opens
the computer view.")]
    Computer {
        #[command(subcommand)]
        command: ComputerCommand,
    },
    /// Reach this computer from anywhere through Cupola's relay, encrypted end to end
    Relay {
        #[command(subcommand)]
        command: RelayCommand,
    },
    /// List the phones connected to this computer, or disconnect one
    Devices {
        #[command(subcommand)]
        command: Option<DevicesCommand>,
    },
    /// Show which agent providers are installed here and sign in
    Providers {
        #[command(subcommand)]
        command: Option<ProvidersCommand>,
    },
    /// Read or change host settings
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Check this computer's setup and explain how to fix problems
    Doctor,
    /// Free space taken by previews no conversation uses any more
    Media {
        #[command(subcommand)]
        command: MediaCommand,
    },
    /// Teach voice mode your own words and names
    Voice {
        #[command(subcommand)]
        command: VoiceCommand,
    },
    /// Check or grant the system permissions the host needs (default: status)
    Permissions {
        #[command(subcommand)]
        command: Option<PermissionsCommand>,
    },
    /// Run the host automatically when you sign in
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Check for and install a newer Cupola host
    #[command(after_long_help = "\
The download is checked against its published SHA-256 before anything changes.
If agents are working, Cupola asks before restarting the host; their work pauses
and continues after the restart. Without a terminal to ask in (or with --json),
it defers restarting while agents work unless --yes. Computer sharing and an
in-flight message send always defer the restart. A failed readiness check on
macOS or Windows restores the previous release.

Releases come from WONDER_UPDATE_URL (a folder holding `stable.json`,
`nightly.json` and `versions/<X>.json`).")]
    Update {
        /// Only report whether an update is available
        #[arg(long)]
        check: bool,
        /// Which releases to follow (default: the channel this installation
        /// follows, else stable)
        #[arg(long, value_enum)]
        channel: Option<Channel>,
        /// Install this exact version instead of the latest
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
        /// Restart without asking, even while agents are working (their work
        /// pauses and continues after the restart)
        #[arg(long)]
        yes: bool,
    },
    /// Write redacted facts about this computer for your coding agent to diagnose
    #[command(after_long_help = "\
Collects what `cupola doctor`, `status`, `routes`, `permissions`, `providers` and `logs`
report, plus the host's recent restarts and, where the system allows it, a screen-sharing
self-test, into
one file only you can read. Pairing links, keys, tokens, network addresses, computer and
phone names, home folder names and conversation text are removed; `~` stands for your
home folder. Nothing is sent anywhere. The command prints what to tell your coding agent
(Claude Code, Codex, …) so it can follow the public playbook, propose fixes for your
approval and draft a GitHub issue.")]
    Triage {
        /// How many of the latest host log lines to include
        #[arg(long, value_name = "COUNT", default_value_t = 80)]
        lines: usize,
    },
    /// Show the installed version
    Version,
    /// Send a message to an agent in a Project and follow its reply
    #[command(
        args_conflicts_with_subcommands = true,
        subcommand_negates_reqs = true,
        after_long_help = CHAT_HELP
    )]
    Chat(ChatArgs),
    /// Start a task as a child of a thread, shown in that thread as an agent task
    #[command(after_long_help = DELEGATE_HELP)]
    Delegate(DelegateArgs),
    /// Speak Cupola RPC (JSON-RPC 2.0) on standard input and output
    #[command(after_long_help = "\
One JSON-RPC message per line, as this computer's owner. Start with `initialize`;
the methods, errors and event cursors are described in crates/wonderd/RPC.md.")]
    Rpc {
        /// Print the protocol's JSON Schema instead
        #[arg(long)]
        schema: bool,
    },
}

const CHAT_HELP: &str = "\
Sends through Cupola's own orchestration, as the iPhone or iPad does: the
host's provider runs the turn, and the thread appears in the Project on your
iPhone or iPad.
The message goes as this computer's owner. Without --thread a new thread
starts, with --provider (default: the Project's last provider, else Codex) and
--model (default: the provider's default model here).

The reply streams as it arrives: text on standard output, tool activity,
approvals and the final state on standard error. An approval names its ID;
answer it on a paired iPhone or iPad (`cupola chat approve` and `deny` say why
they cannot answer as the owner yet). The command ends when the turn ends.

A message sent while a reply runs waits in the queue; the reply ahead is not
shown. Once it is sent (on its own, or by `cupola chat send-now` into the
running reply) the turn it is part of streams here, and its end ends the
command. If the connection to the host drops, the command picks up where it
left off. --timeout also covers a host that stops answering.

Exit status: 0 the turn completed, 1 it failed (or the command did), 2 usage,
3 it was interrupted, 69 unavailable, 75 --timeout passed while the turn still
runs (it keeps running; read it in the app or with `cupola rpc`).

With --json, each line is one event object (`type`: sent, text, message,
tool, approval, approvalResolved, state, notice, end); the last is `end` with
`status` and `exitCode`.

Examples:
  cupola chat -p \"Summarize the README\" --project Cupola --new --json
  cupola chat -p \"Now the tests\" --thread 3f2a… --model gpt-5.5
  cupola chat queue --thread 3f2a…
  cupola chat send-now 9c1e… --thread 3f2a…";

const DELEGATE_HELP: &str = "\
Does what an agent's `wonder_delegate` does, as this computer's owner: a new
thread starts in the parent's Project with the task as its first message, runs
on its own, and appears in the parent thread as an agent task. The command
prints the child's thread ID and returns; follow it in the app or with
`cupola chat`.

The child works in the parent's folder unless --folder (one of the Project's
folders) or --worktree (a Git worktree of one) names another. Its access
defaults to the parent's and is never wider.

A thread may have 16 children by default; --max-children (1 to 64) or the
host's CUPOLA_DELEGATE_MAX_CHILDREN changes that, and past the limit the
command fails with a plain message.

Examples:
  cupola delegate --parent 3f2a… -p \"Fix the failing test\" --provider claude --model haiku
  cupola delegate --parent 3f2a… -p \"Review the diff\" --provider codex --worktree ~/src/app/.local/wt-review";

#[derive(Debug, Args)]
pub struct DelegateArgs {
    /// The parent thread's conversation ID
    #[arg(long, value_name = "ID")]
    pub parent: String,
    /// The task; the child sees only this
    #[arg(short = 'p', long = "prompt", value_name = "TASK")]
    pub prompt: String,
    /// Which agent runs the child
    #[arg(long, value_enum)]
    pub provider: Provider,
    /// Which model (default: the provider's default)
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,
    /// Reasoning effort the model offers
    #[arg(long, value_name = "EFFORT")]
    pub effort: Option<String>,
    /// Name the child thread (default: the task's first line)
    #[arg(long)]
    pub title: Option<String>,
    /// Work in this folder of the Project
    #[arg(long, value_name = "PATH", conflicts_with = "worktree")]
    pub folder: Option<String>,
    /// Work in this Git worktree of a Project folder
    #[arg(long, value_name = "PATH")]
    pub worktree: Option<String>,
    /// Most children the parent may have, 1 to 64
    #[arg(long, value_name = "N")]
    pub max_children: Option<u32>,
    /// Reuse on retries so the task starts once (default: a new one)
    #[arg(long, value_name = "UUID")]
    pub key: Option<String>,
}

#[derive(Debug, Args)]
pub struct ChatArgs {
    #[command(subcommand)]
    pub command: Option<ChatCommand>,
    /// The message to send
    #[arg(short = 'p', long = "prompt", value_name = "MESSAGE", required = true)]
    pub prompt: Option<String>,
    /// The Project, by name or ID (needed for a new thread)
    #[arg(long, value_name = "NAME", required_unless_present_any = ["thread", "scratch"], conflicts_with = "scratch")]
    pub project: Option<String>,
    /// Start with no Project, in a folder of its own
    #[arg(long, conflicts_with = "thread")]
    pub scratch: bool,
    /// Send in this existing thread (its conversation ID)
    #[arg(long, value_name = "ID", conflicts_with = "new")]
    pub thread: Option<String>,
    /// Start a new thread (the default without --thread)
    #[arg(long)]
    pub new: bool,
    /// Which agent runs the thread; on an existing thread, switches it
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Which model; on an existing thread, switches it
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,
    /// Stop following after this many seconds; the turn keeps running
    #[arg(long, value_name = "SECONDS", default_value_t = 1800)]
    pub timeout: u64,
}

#[derive(Debug, Subcommand)]
pub enum ChatCommand {
    /// Allow a waiting approval
    Approve {
        /// The approval's ID, as `cupola chat` printed it
        id: String,
    },
    /// Refuse a waiting approval
    Deny {
        /// The approval's ID, as `cupola chat` printed it
        id: String,
    },
    /// Move a thread onto a new git worktree and carry on there
    Worktree {
        /// The thread's conversation ID
        #[arg(long, value_name = "ID")]
        thread: String,
        /// The new branch's name (made from the thread's title when left out)
        #[arg(long, value_name = "NAME")]
        branch: Option<String>,
    },
    /// List the messages waiting in a thread's queue
    Queue {
        /// The thread's conversation ID
        #[arg(long, value_name = "ID")]
        thread: String,
    },
    /// Send a queued message into the running turn now
    SendNow {
        /// The queued message's ID, from `cupola chat queue`
        id: String,
        /// The thread's conversation ID
        #[arg(long, value_name = "ID")]
        thread: String,
    },
}

// Set up again, the host keeps every setting whose flag is left out.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Name your iPhone or iPad shows for this computer
    #[arg(long)]
    pub name: Option<String>,
    /// How your iPhone or iPad reaches this computer away from home [default: tailscale]
    #[arg(long, value_enum)]
    pub tunnel: Option<Tunnel>,
    /// HTTPS address you route to this computer yourself (with --tunnel none)
    #[arg(long, value_name = "URL")]
    pub origin: Option<String>,
    /// Tailscale HTTPS port; an occupied port is never replaced [default: 443]
    #[arg(long, value_name = "PORT")]
    pub https_port: Option<u16>,
    /// Local address the host listens on; must be loopback [default: 127.0.0.1:3777]
    #[arg(long, value_name = "ADDRESS")]
    pub listen: Option<String>,
    /// Share a virtual screen your iPhone or iPad can view and control (experimental)
    #[arg(long, overrides_with = "no_computer")]
    pub computer: bool,
    /// Stop sharing the virtual screen
    #[arg(long, overrides_with = "computer")]
    pub no_computer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Tunnel {
    /// Tailscale Serve on this computer's MagicDNS name, once Tailscale is signed in
    Tailscale,
    /// No tunnel; --origin names an HTTPS address you route yourself
    None,
}

impl Tunnel {
    pub fn as_str(self) -> &'static str {
        match self {
            Tunnel::Tailscale => "tailscale",
            Tunnel::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PairRoute {
    /// This computer's Tailscale name
    Tailscale,
    /// The home network (your iPhone or iPad must be on the same Wi-Fi)
    Home,
    /// Cupola's relay (pair on the home network or Tailscale first)
    Relay,
}

#[derive(Debug, Subcommand)]
pub enum LanCommand {
    /// Turn home network connections on
    On {
        /// Port to listen on at each home network address
        #[arg(long, value_name = "PORT", default_value_t = DEFAULT_LAN_PORT,
              value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
    },
    /// Turn home network connections off
    Off,
    /// Show whether home network connections are on, and where
    Status,
}

#[derive(Debug, Subcommand)]
pub enum ComputerCommand {
    /// Allow or refuse Take control from your iPhone or iPad
    Control {
        #[command(subcommand)]
        command: ControlCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ControlCommand {
    /// Let your iPhone or iPad take control of this computer
    On,
    /// Let your iPhone or iPad only watch this computer
    Off,
    /// Show whether your iPhone or iPad can view and control this computer
    Status,
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub enum RelayCommand {
    /// Allow the relay here; your iPhone or iPad then turns on Reach from anywhere
    On,
    /// Turn the relay off and release its address
    Off,
    /// Show whether the relay is on
    Status,
}

/// The home network port, by default and when `cupola lan on` names none.
pub const DEFAULT_LAN_PORT: u16 = 3778;

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Start the host automatically when you sign in
    Install,
    /// Stop starting the host automatically
    Uninstall,
    /// Show whether the host starts automatically
    Status,
    /// Restart the host through its service
    Restart,
}

#[derive(Debug, Subcommand)]
pub enum VoiceCommand {
    /// Words and names voice mode should transcribe correctly, such as T3 or Cupola
    Words {
        #[command(subcommand)]
        command: VoiceWordsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum VoiceWordsCommand {
    /// Show your voice words
    List,
    /// Add words; they apply when the next voice session starts
    Add {
        /// Words or names to add (use quotes for a phrase)
        #[arg(required = true, value_name = "WORD")]
        words: Vec<String>,
    },
    /// Remove words
    Remove {
        /// Words or names to remove
        #[arg(required = true, value_name = "WORD")]
        words: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MediaCommand {
    /// List preview files no conversation names any more; --apply deletes them
    Prune {
        /// Only files unchanged for at least this many days
        #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u32).range(1..))]
        older_than_days: u32,
        /// Delete the listed files (without it, nothing changes)
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DevicesCommand {
    /// Disconnect a phone; it can no longer reach this computer
    Revoke {
        /// The phone's ID from `cupola devices`
        id: String,
    },
    /// Remove a disconnected phone from the list
    Forget {
        /// The phone's ID from `cupola devices`
        id: String,
    },
    /// Give a phone a name you will recognize in the list
    Rename {
        /// The phone's ID from `cupola devices`
        id: String,
        /// The new name (up to 160 characters)
        #[arg(required = true, num_args = 1..)]
        name: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    /// Sign in to a provider on this computer with its own CLI
    Login {
        /// Which provider
        provider: Provider,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Provider {
    /// OpenAI Codex
    Codex,
    /// Claude Code
    Claude,
    /// OpenCode
    Opencode,
    /// Cursor (the Cursor CLI, `agent`)
    Cursor,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Claude => "claude",
            Provider::Opencode => "opencode",
            Provider::Cursor => "cursor",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Codex => "Codex",
            Provider::Claude => "Claude Code",
            Provider::Opencode => "OpenCode",
            Provider::Cursor => "Cursor",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show a setting
    Get {
        /// Which setting
        key: ConfigKey,
    },
    /// Change a setting
    #[command(after_long_help = "\
Keys:
  host-name  Name your iPhone or iPad shows for this computer. Omit the value to
             go back to the computer's own name.
  voice-key  OpenAI API key for voice mode. Type or pipe it on standard input so
             it never appears in your shell history or process list.")]
    Set {
        /// Which setting
        key: ConfigKey,
        /// New value (host-name only; voice-key is read from standard input)
        value: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConfigKey {
    /// Name your iPhone or iPad shows for this computer
    HostName,
    /// OpenAI API key for voice mode (never shown)
    VoiceKey,
}

#[derive(Debug, Subcommand)]
pub enum PermissionsCommand {
    /// Show which system permissions are granted, without asking for any
    Status,
    /// Ask the system for the permissions that are missing
    Grant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Channel {
    /// Tested releases
    Stable,
    /// Daily builds; newer and less tested
    Nightly,
}

impl Channel {
    /// The feed file name: `stable.json` or `nightly.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Nightly => "nightly",
        }
    }
}

// ---------------------------------------------------------------- errors

/// The command exists but cannot run here yet. Prints as `code: unavailable`.
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

/// `next` is the recovery step in user language. Linux implements every
/// command; macOS and Windows answer with this until theirs land.
#[allow(dead_code)]
pub fn not_on_this_platform(command: &str, next: &str) -> Unavailable {
    Unavailable(format!(
        "`cupola {command}` is not available on this platform yet. {next}"
    ))
}

#[derive(Debug, Serialize)]
pub struct ErrorOutput {
    pub error: ErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    Unavailable,
    Failed,
}

impl ErrorOutput {
    pub fn from_error(error: &(dyn std::error::Error + 'static)) -> (Self, i32) {
        let (code, exit) = if error.downcast_ref::<Unavailable>().is_some() {
            (ErrorCode::Unavailable, EXIT_UNAVAILABLE)
        } else {
            (ErrorCode::Failed, 1)
        };
        let output = Self {
            error: ErrorBody {
                code,
                message: error.to_string(),
            },
        };
        (output, exit)
    }
}

// ---------------------------------------------------------------- outputs

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitOutput {
    pub data_dir: String,
    pub host_name: Option<String>,
    pub tunnel: Tunnel,
    pub origin: Option<String>,
    pub listen_addr: String,
    pub https_port: u16,
    pub computer: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartOutput {
    /// Always `starting`: readiness is reported by `status`.
    pub state: &'static str,
    pub supervisor_pid: u32,
    pub log_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StopState {
    Stopped,
    NotRunning,
}

#[derive(Debug, Serialize)]
pub struct StopOutput {
    pub state: StopState,
}

/// `host`, `providers` and `devices` are the daemon's own loopback responses
/// (`/api/v1/host/status`, `/api/v1/host/providers`, `/api/v1/devices`), or
/// null when the daemon is not answering.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusOutput {
    pub supervisor_pid: Option<i32>,
    pub host: Option<Value>,
    pub providers: Option<Value>,
    pub devices: Option<Value>,
}

/// One JSON object per line. `offer` comes first; `verification` precedes each
/// approval prompt (including `--yes`); `rejected` may repeat; the
/// command ends after `connected` (exit 0) or an error object (timeout).
#[derive(Debug, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PairEvent {
    Offer {
        offer_id: String,
        url: String,
        origin: String,
        code: String,
        expires_at_ms: u64,
        /// Where `--qr` wrote the QR image; the file is deleted when the offer ends.
        #[serde(skip_serializing_if = "Option::is_none")]
        qr_path: Option<String>,
        /// The computer key the link pins: base64url SHA-256 of its SPKI, as
        /// `cupola lan status --json` reports it. Absent from older hosts.
        #[serde(skip_serializing_if = "Option::is_none")]
        host_key_pin: Option<String>,
    },
    Verification {
        device_id: String,
        label: String,
        host_installation_id: String,
    },
    Cancelled {
        offer_id: String,
    },
    Connected {
        device_id: String,
        label: String,
    },
    Rejected {
        device_id: String,
        label: String,
    },
}

/// `devices` is the daemon's `/api/v1/devices` list, revoked phones included.
#[derive(Debug, Serialize)]
pub struct DevicesOutput {
    pub devices: Vec<Value>,
}

/// `daemon` is the running daemon's provider report (null when stopped);
/// `providers` is its sign-in list (`providers.list`: every provider it
/// offers, Cursor included; null when stopped); the named runtimes are what
/// is installed on this computer.
#[derive(Debug, Serialize)]
pub struct ProvidersOutput {
    pub daemon: Option<Value>,
    pub providers: Option<Vec<Value>>,
    pub codex: LocalRuntime,
    pub claude: LocalRuntime,
    pub opencode: LocalRuntime,
    pub cursor: LocalRuntime,
}

#[derive(Debug, Serialize)]
pub struct LocalRuntime {
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    /// What to do next, in user language (sign in, or how to install).
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct LogsOutput {
    pub path: String,
    /// Oldest first.
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceState {
    Revoked,
    Forgotten,
    Renamed,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceActionOutput {
    pub device_id: String,
    pub state: DeviceState,
    /// The phone's name after the change, when the host returns it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// One line of `cupola chat --json`. A run prints `sent`, then events as the
/// turn runs, and ends with `end`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ChatEvent {
    /// The host accepted the message. `queued`: it waits behind other work.
    Sent {
        project_id: Option<String>,
        thread_id: String,
        message_id: String,
        created: bool,
        queued: bool,
    },
    /// A piece of the agent's reply.
    Text { text: String },
    /// One complete agent message (the pieces since the last one, joined).
    Message { text: String },
    /// Tool activity: `category` is `command`, `fileChange`, `tool`, `plan`
    /// or `reasoning`; `state` is the host's (`started`, `running`,
    /// `completed`, …).
    Tool {
        category: String,
        state: String,
        detail: Option<String>,
    },
    /// The agent waits for a decision. `kind` is the provider's request
    /// method; `decisions` the answers it accepts.
    Approval {
        approval_id: String,
        kind: String,
        summary: String,
        decisions: Vec<Value>,
    },
    /// An approval this run printed was answered.
    ApprovalResolved {
        approval_id: String,
        decision: Option<String>,
    },
    /// The message's delivery state changed (`dispatching_to_codex`,
    /// `streaming`, …).
    State { state: String },
    /// Something the person should know that is not part of the reply.
    Notice { message: String },
    /// The last line.
    End {
        status: ChatStatus,
        exit_code: i32,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatStatus {
    Completed,
    Failed,
    Interrupted,
    TimedOut,
}

impl ChatStatus {
    pub fn exit_code(self) -> i32 {
        match self {
            ChatStatus::Completed => 0,
            ChatStatus::Failed => 1,
            ChatStatus::Interrupted => 3,
            ChatStatus::TimedOut => 75,
        }
    }
}

/// `cupola chat queue`: the thread's queue, next first (`queuedMessage`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatQueueOutput {
    pub thread_id: String,
    pub queue: Vec<Value>,
}

/// `cupola chat send-now|approve|deny`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatActionOutput {
    pub id: String,
    /// `sentNow`, `approved` or `denied`.
    pub state: &'static str,
}

/// The provider's own CLI completed sign-in on this computer.
#[derive(Debug, Serialize)]
pub struct ProviderLoginOutput {
    pub provider: Provider,
    pub login: Value,
}

/// `value` is null for secrets (`voice-key`); `configured` says whether the
/// owner set it (for `host-name`, false means the computer's own name).
#[derive(Debug, Serialize)]
pub struct ConfigOutput {
    pub key: ConfigKey,
    pub value: Option<String>,
    pub configured: bool,
}

/// One way a phone can reach this host. `kind` names the transport; this is a
/// details view, so it may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RouteKind {
    Tailscale,
    Home,
    Relay,
    /// An HTTPS address the owner routes themselves (`--tunnel none`).
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RouteState {
    /// The running host serves this route.
    Ready,
    /// The running host is still bringing this route up.
    Starting,
    /// On, but it cannot work right now; `detail` says why and what to do.
    Unavailable,
    /// On; the host is not running.
    Stopped,
    /// Changed since the host started; a restart applies it.
    RestartNeeded,
    Off,
    /// The relay: allowed on this computer, waiting for the phone to turn it on.
    Allowed,
}

#[derive(Debug, Serialize)]
pub struct RouteStatus {
    pub kind: RouteKind,
    pub enabled: bool,
    pub state: RouteState,
    pub origins: Vec<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutesOutput {
    pub running: bool,
    pub routes: Vec<RouteStatus>,
    /// base64url SHA-256 of the host key's SPKI: what a phone pins for the
    /// home network. Null until the home network has been turned on.
    pub host_key_pin: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanOutput {
    pub enabled: bool,
    pub port: u16,
    pub state: RouteState,
    pub origins: Vec<String>,
    pub host_key_pin: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerControlOutput {
    /// Whether computer view is set up (`init --computer`)
    pub viewing: bool,
    pub control: ControlState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlState {
    On,
    Off,
    /// The saved choice is not one the computer helper trusts; control is refused.
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct RelayOutput {
    pub enabled: bool,
    pub state: RouteState,
    pub origin: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DoctorOutput {
    pub ok: bool,
    pub checks: Vec<Check>,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub status: CheckStatus,
    pub summary: String,
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckStatus {
    Ok,
    Warning,
    Failed,
    /// Not checked on this platform yet.
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct PermissionsOutput {
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Serialize)]
pub struct Permission {
    pub id: &'static str,
    pub state: PermissionState,
    pub summary: String,
    pub fix: Option<String>,
}

// Linux lists no permissions; macOS and Windows construct these.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionState {
    Granted,
    Denied,
    NotDetermined,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceOutput {
    /// `systemd`, `launchd` or `windows`.
    pub manager: &'static str,
    pub installed: bool,
    pub running: bool,
    /// Whether the host also starts when the computer boots, before anyone
    /// signs in (systemd lingering). Null where the platform has no such
    /// choice.
    pub starts_at_boot: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateOutput {
    pub channel: Channel,
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    pub installed: bool,
    /// Whether the running host now runs the installed version. False when
    /// it was not running, or the owner chose to restart later.
    pub restarted: bool,
    /// After a restart, whether any agent is ready yet. Agents that are down
    /// or slow to start do not undo an update that started the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents_ready: Option<bool>,
}

/// `facts` is the redacted document written to `path` (a private file in a
/// new folder); `problems` are the sentences the terminal summary shows.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TriageOutput {
    pub path: String,
    pub running: bool,
    pub problems: Vec<String>,
    /// The public playbook the person's coding agent should follow.
    pub playbook: &'static str,
    pub facts: Value,
}

#[derive(Debug, Serialize)]
pub struct VersionOutput {
    pub version: &'static str,
    pub platform: &'static str,
    pub arch: &'static str,
}

impl VersionOutput {
    pub fn current() -> Self {
        Self {
            version: RELEASE_VERSION,
            platform: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }
}

#[cfg(test)]
mod headless_contract {
    use super::*;
    use clap::CommandFactory;
    use serde_json::json;

    fn parse(line: &str) -> Cli {
        Cli::try_parse_from(std::iter::once("cupola").chain(line.split_whitespace()))
            .unwrap_or_else(|error| panic!("`cupola {line}` did not parse: {error}"))
    }

    // Contract: every command accepts `--json`, before or after it. Walks the
    // real command tree, so a new command cannot miss the flag.
    #[test]
    fn every_command_takes_json_and_bare_commands_keep_their_defaults() {
        let mut root = Cli::command();
        root.build();
        fn walk(command: &clap::Command, path: &str) {
            for sub in command.get_subcommands().filter(|s| s.get_name() != "help") {
                let name = format!("{path} {}", sub.get_name());
                assert!(
                    sub.get_arguments().any(|arg| arg.get_id() == "json"),
                    "`{name}` does not take --json"
                );
                walk(sub, &name);
            }
        }
        walk(&root, "cupola");
        for line in [
            "devices rename d1 Work iPhone",
            "lan on --port 4443",
            "status",
        ] {
            assert!(!parse(line).json, "{line}");
            assert!(parse(&format!("{line} --json")).json, "{line} --json");
            assert!(parse(&format!("--json {line}")).json, "--json {line}");
        }
        let Some(HostCommand::Init(init)) = parse("init").command else {
            panic!("init");
        };
        // Every setting left out keeps its earlier value (`init_at`).
        assert_eq!(
            (
                init.tunnel,
                init.https_port,
                init.listen.as_deref(),
                init.computer,
                init.no_computer
            ),
            (None, None, None, false, false)
        );
        let Some(HostCommand::Init(init)) = parse("init --computer --no-computer").command else {
            panic!("init");
        };
        assert_eq!((init.computer, init.no_computer), (false, true));
        assert!(parse("").command.is_none());
        assert!(matches!(
            parse("lan on").command,
            Some(HostCommand::Lan {
                command: LanCommand::On { port: 3778 }
            })
        ));
        assert!(matches!(
            parse("pair").command,
            Some(HostCommand::Pair { route: None, .. })
        ));
        let Some(HostCommand::Update {
            check,
            channel,
            version,
            yes,
        }) = parse("update").command
        else {
            panic!("update");
        };
        assert_eq!((check, channel, version, yes), (false, None, None, false));
        let Some(HostCommand::Logs { lines, follow }) = parse("logs").command else {
            panic!("logs");
        };
        assert_eq!((lines, follow), (50, false));
        assert!(matches!(
            parse("devices").command,
            Some(HostCommand::Devices { command: None })
        ));
        assert!(matches!(
            parse("providers").command,
            Some(HostCommand::Providers { command: None })
        ));
        // Bare `permissions` shows status (it never prompts).
        assert!(matches!(
            parse("permissions").command,
            Some(HostCommand::Permissions { command: None })
        ));
        let Some(HostCommand::Devices {
            command: Some(DevicesCommand::Rename { id, name }),
        }) = parse("devices rename d1 Work iPhone").command
        else {
            panic!("devices rename");
        };
        assert_eq!((id.as_str(), name.join(" ")), ("d1", "Work iPhone".into()));
    }

    /// `cupola --help` says how the host starts and upgrades without a new
    /// `cupola init`; `-h` points there.
    #[test]
    fn help_has_an_install_section() {
        let mut root = Cli::command();
        let long = root.render_long_help().to_string();
        let install =
            &long[long.find("Install:").expect(&long)..long.find("Environment:").expect(&long)];
        assert!(install.contains("`cupola service install`"), "{install}");
        assert!(
            install.contains("do not\n  run `cupola init` again")
                || install.contains("do not run `cupola init` again"),
            "{install}"
        );
        let short = root.render_help().to_string();
        assert!(!short.contains("Install:"), "{short}");
        assert!(short.ends_with("Run `cupola --help` for install and upgrade steps.\n"));
    }

    #[test]
    fn invalid_input_is_a_usage_error() {
        for line in [
            "pair --timeout soon",
            "pair --cancel",
            #[cfg(not(feature = "dev-pairing"))]
            "pair --yes",
            "pair --cancel o1 --yes",
            "pair --cancel o1 --timeout 10",
            "pair --watch-stdin",
            #[cfg(feature = "dev-pairing")]
            "pair --cancel o1 --watch-stdin",
            "pair --cancel o1 --qr x.png",
            "pair --approve d",
            "pair --approve d --code",
            "pair --code B36D1B62BB5F",
            "pair --deny d --code B36D1B62BB5F",
            "pair --cancel o1 --code B36D1B62BB5F",
            "pair --qr x.png --code B36D1B62BB5F",
            "pair --timeout 10 --code B36D1B62BB5F",
            "pair --route home --code B36D1B62BB5F",
            #[cfg(feature = "dev-pairing")]
            "pair --yes --code B36D1B62BB5F",
            #[cfg(feature = "dev-pairing")]
            "pair --yes --watch-stdin --code B36D1B62BB5F",
            "pair --approve d --code B36D1B62BB5F --deny d",
            "pair --approve d --code B36D1B62BB5F --yes",
            "pair --approve d --code B36D1B62BB5F --qr x.png",
            "pair --approve d --code B36D1B62BB5F --timeout 10",
            "init --tunnel funnel",
            "init --https-port 70000",
            "service",
            "service reload",
            "frobnicate",
            "permissions --request",
            "devices revoke",
            "devices rename d1",
            "providers login",
            "providers login gemini",
            "config",
            "config get",
            "config get theme",
            "update --channel beta",
            "logs --lines many",
            "pair --route lan",
            "pair --cancel o1 --route home",
            "routes on",
            "lan",
            "lan on --port 0",
            "lan on --port 70000",
            "lan off --port 3778",
            "relay",
            "relay maybe",
        ] {
            let error =
                Cli::try_parse_from(std::iter::once("cupola").chain(line.split_whitespace()))
                    .expect_err(line);
            assert_eq!(error.exit_code(), 2, "{line}");
        }
    }

    #[test]
    fn every_command_and_option_has_help_text() {
        let mut root = Cli::command();
        root.build();
        assert_eq!(root.get_name(), "cupola");
        fn walk(command: &clap::Command, path: &str) {
            for arg in command.get_arguments() {
                let id = arg.get_id().as_str();
                if matches!(id, "help" | "version") {
                    continue;
                }
                let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
                assert!(
                    !help.trim().is_empty(),
                    "`{path}` option `{id}` has no help"
                );
            }
            for sub in command.get_subcommands().filter(|s| s.get_name() != "help") {
                let name = format!("{path} {}", sub.get_name());
                let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
                assert!(!about.trim().is_empty(), "`{name}` has no help");
                walk(sub, &name);
            }
        }
        walk(&root, "cupola");
        let help = root.render_long_help().to_string();
        assert!(help.contains("cupola pair"), "{help}");
    }

    /// The host pairs with iPads too, so help never names the iPhone alone.
    #[test]
    fn help_names_the_ipad_wherever_it_names_the_iphone() {
        fn walk(command: &mut clap::Command, path: &str) {
            let help = command.render_long_help().to_string();
            let unwrapped = help.split_whitespace().collect::<Vec<_>>().join(" ");
            for (at, _) in unwrapped.match_indices("iPhone") {
                let rest = &unwrapped[at..];
                assert!(
                    rest.starts_with("iPhone or iPad") || rest.starts_with("iPhones and iPads"),
                    "`{path}` help names only the iPhone: {rest:.60}"
                );
            }
            for sub in command
                .get_subcommands_mut()
                .filter(|s| s.get_name() != "help")
            {
                let name = format!("{path} {}", sub.get_name());
                walk(sub, &name);
            }
        }
        let mut root = Cli::command();
        root.build();
        assert!(root
            .render_long_help()
            .to_string()
            .contains("iPhone or iPad"));
        walk(&mut root, "cupola");
    }

    fn keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    // Each row pins one `--json` shape: a renamed, removed or retyped field
    // breaks callers on every platform.
    #[test]
    fn json_shapes_match_the_documented_contract() {
        let runtime = || LocalRuntime {
            installed: true,
            path: Some("/usr/bin/codex".into()),
            version: Some("1.0".into()),
            detail: "Sign in".into(),
        };
        let rows: Vec<(&str, Value, Value)> = vec![
            (
                "init",
                serde_json::to_value(InitOutput {
                    data_dir: "/home/u/.wonder".into(),
                    host_name: Some("Linux".into()),
                    tunnel: Tunnel::None,
                    origin: Some("https://host.example".into()),
                    listen_addr: "127.0.0.1:3777".into(),
                    https_port: 443,
                    computer: false,
                })
                .unwrap(),
                json!({"dataDir": "/home/u/.wonder", "hostName": "Linux", "tunnel": "none",
                       "origin": "https://host.example", "listenAddr": "127.0.0.1:3777",
                       "httpsPort": 443, "computer": false}),
            ),
            (
                "start",
                serde_json::to_value(StartOutput {
                    state: "starting",
                    supervisor_pid: 42,
                    log_path: "/l".into(),
                })
                .unwrap(),
                json!({"state": "starting", "supervisorPid": 42, "logPath": "/l"}),
            ),
            (
                "stop",
                serde_json::to_value(StopOutput {
                    state: StopState::NotRunning,
                })
                .unwrap(),
                json!({"state": "notRunning"}),
            ),
            (
                "status",
                serde_json::to_value(StatusOutput {
                    supervisor_pid: None,
                    host: None,
                    providers: None,
                    devices: Some(json!([])),
                })
                .unwrap(),
                json!({"supervisorPid": null, "host": null, "providers": null, "devices": []}),
            ),
            (
                "pair offer",
                serde_json::to_value(PairEvent::Offer {
                    offer_id: "o".into(),
                    url: "https://h/pair#x".into(),
                    origin: "https://h".into(),
                    code: "ABCD".into(),
                    expires_at_ms: 9,
                    qr_path: None,
                    host_key_pin: None,
                })
                .unwrap(),
                json!({"event": "offer", "offerId": "o", "url": "https://h/pair#x",
                       "origin": "https://h", "code": "ABCD", "expiresAtMs": 9}),
            ),
            (
                "pair offer with qr",
                serde_json::to_value(PairEvent::Offer {
                    offer_id: "o".into(),
                    url: "https://h/pair#x".into(),
                    origin: "https://h".into(),
                    code: "ABCD".into(),
                    expires_at_ms: 9,
                    qr_path: Some("/tmp/pair.png".into()),
                    host_key_pin: Some("pin".into()),
                })
                .unwrap(),
                json!({"event": "offer", "offerId": "o", "url": "https://h/pair#x",
                       "origin": "https://h", "code": "ABCD", "expiresAtMs": 9,
                       "qrPath": "/tmp/pair.png", "hostKeyPin": "pin"}),
            ),
            (
                "pair verification",
                serde_json::to_value(PairEvent::Verification {
                    device_id: "d".into(),
                    label: "iPhone".into(),
                    host_installation_id: "h".into(),
                })
                .unwrap(),
                json!({"event":"verification", "deviceId":"d", "label":"iPhone",
                    "hostInstallationId":"h"}),
            ),
            (
                "pair cancelled",
                serde_json::to_value(PairEvent::Cancelled {
                    offer_id: "o".into(),
                })
                .unwrap(),
                json!({"event":"cancelled", "offerId":"o"}),
            ),
            (
                "pair connected",
                serde_json::to_value(PairEvent::Connected {
                    device_id: "d".into(),
                    label: "iPhone".into(),
                })
                .unwrap(),
                json!({"event": "connected", "deviceId": "d", "label": "iPhone"}),
            ),
            (
                "pair rejected",
                serde_json::to_value(PairEvent::Rejected {
                    device_id: "d".into(),
                    label: "iPhone".into(),
                })
                .unwrap(),
                json!({"event": "rejected", "deviceId": "d", "label": "iPhone"}),
            ),
            (
                "devices",
                serde_json::to_value(DevicesOutput {
                    devices: vec![json!({"id": "d"})],
                })
                .unwrap(),
                json!({"devices": [{"id": "d"}]}),
            ),
            (
                "providers",
                serde_json::to_value(ProvidersOutput {
                    daemon: None,
                    providers: Some(vec![json!({"id": "cursor", "state": "signedIn"})]),
                    codex: runtime(),
                    claude: runtime(),
                    opencode: runtime(),
                    cursor: runtime(),
                })
                .unwrap(),
                {
                    let local = json!({"installed": true, "path": "/usr/bin/codex",
                                       "version": "1.0", "detail": "Sign in"});
                    json!({"daemon": null,
                           "providers": [{"id": "cursor", "state": "signedIn"}],
                           "codex": local, "claude": local, "opencode": local,
                           "cursor": local})
                },
            ),
            (
                "doctor",
                serde_json::to_value(DoctorOutput {
                    ok: false,
                    checks: vec![Check {
                        id: "initialized",
                        status: CheckStatus::Failed,
                        summary: "s".into(),
                        fix: Some("f".into()),
                    }],
                })
                .unwrap(),
                json!({"ok": false, "checks": [{"id": "initialized", "status": "failed",
                                                "summary": "s", "fix": "f"}]}),
            ),
            (
                "permissions",
                serde_json::to_value(PermissionsOutput {
                    permissions: vec![Permission {
                        id: "screenRecording",
                        state: PermissionState::NotDetermined,
                        summary: "s".into(),
                        fix: None,
                    }],
                })
                .unwrap(),
                json!({"permissions": [{"id": "screenRecording", "state": "notDetermined",
                                        "summary": "s", "fix": null}]}),
            ),
            (
                "service",
                serde_json::to_value(ServiceOutput {
                    manager: "systemd",
                    installed: true,
                    running: false,
                    starts_at_boot: Some(true),
                })
                .unwrap(),
                json!({"manager": "systemd", "installed": true, "running": false,
                       "startsAtBoot": true}),
            ),
            (
                "update",
                serde_json::to_value(UpdateOutput {
                    channel: Channel::Nightly,
                    current: "1".into(),
                    latest: None,
                    update_available: false,
                    installed: false,
                    restarted: false,
                    agents_ready: None,
                })
                .unwrap(),
                json!({"channel": "nightly", "current": "1", "latest": null,
                       "updateAvailable": false, "installed": false, "restarted": false}),
            ),
            (
                "logs",
                serde_json::to_value(LogsOutput {
                    path: "/home/u/.wonder/logs/wonder.log".into(),
                    lines: vec!["a".into(), "b".into()],
                })
                .unwrap(),
                json!({"path": "/home/u/.wonder/logs/wonder.log", "lines": ["a", "b"]}),
            ),
            (
                "devices revoke",
                serde_json::to_value(DeviceActionOutput {
                    device_id: "d".into(),
                    state: DeviceState::Revoked,
                    label: None,
                })
                .unwrap(),
                json!({"deviceId": "d", "state": "revoked"}),
            ),
            (
                "devices forget",
                serde_json::to_value(DeviceActionOutput {
                    device_id: "d".into(),
                    state: DeviceState::Forgotten,
                    label: None,
                })
                .unwrap(),
                json!({"deviceId": "d", "state": "forgotten"}),
            ),
            (
                "devices rename",
                serde_json::to_value(DeviceActionOutput {
                    device_id: "d".into(),
                    state: DeviceState::Renamed,
                    label: Some("Work iPhone".into()),
                })
                .unwrap(),
                json!({"deviceId": "d", "state": "renamed", "label": "Work iPhone"}),
            ),
            (
                "providers login",
                serde_json::to_value(ProviderLoginOutput {
                    provider: Provider::Codex,
                    login: json!({"method": "terminal", "state": "succeeded"}),
                })
                .unwrap(),
                json!({"provider": "codex",
                       "login": {"method": "terminal", "state": "succeeded"}}),
            ),
            (
                "config host-name",
                serde_json::to_value(ConfigOutput {
                    key: ConfigKey::HostName,
                    value: Some("Studio".into()),
                    configured: true,
                })
                .unwrap(),
                json!({"key": "hostName", "value": "Studio", "configured": true}),
            ),
            (
                "config voice-key",
                serde_json::to_value(ConfigOutput {
                    key: ConfigKey::VoiceKey,
                    value: None,
                    configured: false,
                })
                .unwrap(),
                json!({"key": "voiceKey", "value": null, "configured": false}),
            ),
            (
                "routes",
                serde_json::to_value(RoutesOutput {
                    running: true,
                    routes: vec![
                        RouteStatus {
                            kind: RouteKind::Home,
                            enabled: true,
                            state: RouteState::RestartNeeded,
                            origins: vec!["https://192.168.1.20:3778".into()],
                            detail: Some("d".into()),
                        },
                        RouteStatus {
                            kind: RouteKind::Tailscale,
                            enabled: true,
                            state: RouteState::Ready,
                            origins: vec!["https://h.ts.net".into()],
                            detail: None,
                        },
                        RouteStatus {
                            kind: RouteKind::Custom,
                            enabled: true,
                            state: RouteState::Stopped,
                            origins: vec![],
                            detail: None,
                        },
                        RouteStatus {
                            kind: RouteKind::Relay,
                            enabled: false,
                            state: RouteState::Off,
                            origins: vec![],
                            detail: None,
                        },
                    ],
                    host_key_pin: Some("pin".into()),
                })
                .unwrap(),
                json!({"running": true, "hostKeyPin": "pin", "routes": [
                    {"kind": "home", "enabled": true, "state": "restartNeeded",
                     "origins": ["https://192.168.1.20:3778"], "detail": "d"},
                    {"kind": "tailscale", "enabled": true, "state": "ready",
                     "origins": ["https://h.ts.net"], "detail": null},
                    {"kind": "custom", "enabled": true, "state": "stopped",
                     "origins": [], "detail": null},
                    {"kind": "relay", "enabled": false, "state": "off",
                     "origins": [], "detail": null},
                ]}),
            ),
            (
                "lan",
                serde_json::to_value(LanOutput {
                    enabled: true,
                    port: 3778,
                    state: RouteState::Unavailable,
                    origins: vec![],
                    host_key_pin: None,
                    detail: Some("d".into()),
                })
                .unwrap(),
                json!({"enabled": true, "port": 3778, "state": "unavailable", "origins": [],
                       "hostKeyPin": null, "detail": "d"}),
            ),
            (
                "lan starting",
                serde_json::to_value(RouteState::Starting).unwrap(),
                json!("starting"),
            ),
            (
                "relay",
                serde_json::to_value(RelayOutput {
                    enabled: true,
                    state: RouteState::Ready,
                    origin: Some("https://r-a.example".into()),
                    detail: None,
                })
                .unwrap(),
                json!({"enabled": true, "state": "ready", "origin": "https://r-a.example",
                       "detail": null}),
            ),
            (
                "error",
                serde_json::to_value(
                    ErrorOutput::from_error(&not_on_this_platform("service install", "Next.")).0,
                )
                .unwrap(),
                json!({"error": {"code": "unavailable",
                    "message": "`cupola service install` is not available on this platform yet. Next."}}),
            ),
        ];
        for (name, actual, expected) in rows {
            assert_eq!(actual, expected, "`{name}` --json shape changed");
        }
        let version = serde_json::to_value(VersionOutput::current()).unwrap();
        assert_eq!(keys(&version), ["arch", "platform", "version"]);
    }

    #[test]
    fn errors_map_to_their_exit_status() {
        let (unavailable, exit) = ErrorOutput::from_error(&not_on_this_platform("update", "Next."));
        assert_eq!((unavailable.error.code, exit), (ErrorCode::Unavailable, 69));
        let other: Box<dyn std::error::Error> = "host did not stop".into();
        let (failed, exit) = ErrorOutput::from_error(other.as_ref());
        assert_eq!((failed.error.code, exit), (ErrorCode::Failed, 1));
    }
}
