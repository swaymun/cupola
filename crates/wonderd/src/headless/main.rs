//! EXPERIMENTAL: `wonder`, a command-line supervisor for running the
//! Wonder host headless on Linux and Windows and in a windowless signed
//! macOS bundle. The command tree and `--json` shapes live in [`cli`];
//! operating-system specifics live in `os_unix.rs`/`os_windows.rs` and
//! `macos.rs`.
//!
//! It owns only process supervision and local setup. Everything the phone
//! talks to is the unchanged `wonderd` daemon: pairing, sessions, chats and
//! Projects keep their normal authentication. The phone reaches the daemon
//! through Tailscale Serve, which forwards to loopback, and, once `wonder lan
//! on` is set, directly over TLS on the computer's home network addresses.
//!
//! Built only with `--features experimental-headless`.

use std::{
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use clap::{CommandFactory, Parser};
use serde_json::{json, Value};
use wonder_api::rescue::RescueState;

mod cli;
#[cfg(target_os = "macos")]
mod control_preference;
#[cfg(target_os = "macos")]
mod macos;
#[cfg_attr(unix, path = "os_unix.rs")]
#[cfg_attr(windows, path = "os_windows.rs")]
mod os;
use os::{supervisor_lock, write_private, ControlListener};
mod process;
#[cfg_attr(not(unix), allow(dead_code))]
mod rescue;
mod service_environment;
mod triage;
mod windows_task;
use cli::MediaCommand;
#[cfg(not(target_os = "macos"))]
use cli::ServiceCommand;
use cli::{Channel, UpdateOutput};
use cli::{ChatCommand, ChatEvent, ChatStatus};
use cli::{
    Check, CheckStatus, Cli, ComputerCommand, ConfigCommand, ConfigKey, ConfigOutput,
    DeviceActionOutput, DeviceState, DevicesCommand, DevicesOutput, DoctorOutput, ErrorOutput,
    HostCommand, InitArgs, InitOutput, LanCommand, LanOutput, LocalRuntime, LogsOutput, PairEvent,
    PairRoute, PermissionsCommand, PermissionsOutput, Provider, ProviderLoginOutput,
    ProvidersCommand, ProvidersOutput, RelayCommand, RelayOutput, RouteKind, RouteState,
    RouteStatus, RoutesOutput, ServiceOutput, StartOutput, StatusOutput, StopOutput, StopState,
    Tunnel, Unavailable, VersionOutput,
};
use cli::{VoiceCommand, VoiceWordsCommand};
use process::OwnedChild;
static STOPPING: AtomicBool = AtomicBool::new(false);
/// Another host holds the supervisor lock. `init` and a second supervisor
/// both refuse with it, so it names the way out.
const HOST_RUNNING: &str =
    "Cupola host is running. Stop it first with `cupola stop`, then try again.";
/// `pair --qr`: the file the offer's QR image goes to (set once, by `main`).
static PAIR_QR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    wonderd::cupola_env::adopt();
    #[cfg(target_os = "macos")]
    if let Some(result) = macos::dispatch_internal() {
        if let Err(error) = result {
            eprintln!("cupola: {error}");
            std::process::exit(1);
        }
        return;
    }
    let cli = Cli::parse();
    let json = cli.json;
    // The supervisor's stderr is the host log when a service runs it.
    let supervising = matches!(
        cli.command,
        Some(HostCommand::Start {
            foreground: true,
            ..
        })
    );
    let result = match cli.command {
        Some(HostCommand::Init(args)) => init(&args, json),
        Some(HostCommand::Start {
            foreground,
            data_dir,
        }) => {
            // Set before anything resolves paths or spawns, so the background
            // supervisor and its children inherit it.
            if let Some(data_dir) = data_dir {
                std::env::set_var("WONDER_DATA_DIR", data_dir);
            }
            start(foreground, json)
        }
        Some(HostCommand::Stop) => stop(json),
        Some(HostCommand::Unpublish) => unpublish(json),
        Some(HostCommand::Status) => block_on(status(json)),
        Some(HostCommand::Logs { lines, follow }) => logs(lines, follow, json),
        Some(HostCommand::Pair {
            #[cfg(feature = "dev-pairing")]
            yes,
            timeout,
            cancel,
            #[cfg(feature = "dev-pairing")]
            watch_stdin,
            route,
            qr,
            approve,
            code,
            deny,
        }) => match (cancel, approve, deny) {
            (Some(id), _, _) => block_on(cancel_pair(&id, json)),
            (_, Some(id), _) => block_on(decide_pair(&id, true, code.as_deref(), json)),
            (_, _, Some(id)) => block_on(decide_pair(&id, false, None, json)),
            _ => check_pair_route(route).and_then(|via| {
                // Preserve main's release guard: blind approval is dev-only.
                #[cfg(not(feature = "dev-pairing"))]
                let (yes, watch_stdin) = (false, false);
                if let Some(path) = qr {
                    let _ = PAIR_QR.set(path);
                }
                block_on(pair(yes, timeout, json, watch_stdin, via))
            }),
        },
        Some(HostCommand::Routes) => routes(json),
        Some(HostCommand::Lan { command }) => lan(command, json),
        Some(HostCommand::Computer { command }) => computer(command, json),
        Some(HostCommand::Relay { command }) => relay_command(command, json),
        Some(HostCommand::Devices { command: None }) => block_on(devices(json)),
        Some(HostCommand::Devices {
            command: Some(command),
        }) => block_on(device_action(command, json)),
        Some(HostCommand::Providers { command: None }) => block_on(providers(json)),
        Some(HostCommand::Providers {
            command: Some(command),
        }) => block_on(provider_login(command, json)),
        Some(HostCommand::Config { command }) => block_on(config(command, json)),
        Some(HostCommand::Doctor) => block_on(doctor(json)),
        Some(HostCommand::Media {
            command:
                MediaCommand::Prune {
                    older_than_days,
                    apply,
                },
        }) => block_on(media_prune(older_than_days, apply, json)),
        Some(HostCommand::Voice {
            command: VoiceCommand::Words { command },
        }) => block_on(voice_words(command, json)),
        Some(HostCommand::Permissions { command }) => permissions(command, json),
        #[cfg(target_os = "macos")]
        Some(HostCommand::Service { command }) => macos::service(command, json),
        #[cfg(not(target_os = "macos"))]
        Some(HostCommand::Service { command }) => service(command, json),
        #[cfg(target_os = "macos")]
        Some(HostCommand::Update {
            check,
            channel,
            version,
            yes,
        }) => block_on(macos::update(
            check,
            followed_channel(channel),
            version,
            yes,
            json,
        )),
        #[cfg(all(unix, not(target_os = "macos")))]
        Some(HostCommand::Update {
            check,
            channel,
            version,
            yes,
        }) => block_on(update(check, followed_channel(channel), version, yes, json)),
        #[cfg(windows)]
        Some(HostCommand::Update {
            check,
            channel,
            version,
            yes,
        }) => block_on(update_windows(
            check,
            followed_channel(channel),
            version,
            yes,
            json,
        )),
        Some(HostCommand::Triage { lines }) => triage::triage(lines, json),
        Some(HostCommand::Version) => version(json),
        Some(HostCommand::Chat(args)) => block_on(chat(args, json)),
        Some(HostCommand::Delegate(args)) => block_on(delegate(args, json)),
        Some(HostCommand::Rpc { schema: true }) => {
            print!("{}", wonderd::rpc::schema_text());
            Ok(())
        }
        Some(HostCommand::Rpc { schema: false }) => block_on(rpc_bridge()),
        None => {
            println!("{}", Cli::command().render_long_help());
            Ok(())
        }
    };
    if let Err(error) = result {
        let (output, exit) = ErrorOutput::from_error(error.as_ref());
        if json {
            // One line, so it also ends a `pair` event stream cleanly.
            println!("{}", serde_json::to_string(&output).unwrap_or_default());
        } else if supervising {
            host_log(format_args!("cupola: {error}"));
        } else {
            eprintln!("cupola: {error}");
        }
        std::process::exit(exit);
    }
}

fn print_json<T: serde::Serialize>(value: &T) {
    // Serializing these plain types cannot fail.
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

fn block_on<F: std::future::Future<Output = Result<()>>>(future: F) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(future)
}

// ---------------------------------------------------------------- layout

struct Paths {
    data: PathBuf,
    state: PathBuf,
}

impl Paths {
    fn resolve() -> Result<Self> {
        Self::resolve_in(
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
            std::env::var_os("WONDER_DATA_DIR"),
        )
    }
    /// As `resolve`, for this home directory and data directory setting.
    /// `~/.wonder` named after it became the link to `~/.cupola` (as a login
    /// service installed for rollback does) resolves to `~/.cupola`, so
    /// `service` recognizes the job the move rewrote.
    fn resolve_in(home: Option<&Path>, setting: Option<std::ffi::OsString>) -> Result<Self> {
        let paths = Self::at(setting)?;
        #[cfg(unix)]
        if let Some(home) = home {
            let data = wonderd::data_home::through_link(home, &paths.data);
            if data != paths.data {
                return Self::at(Some(data.into()));
            }
        }
        #[cfg(not(unix))]
        let _ = home;
        Ok(paths)
    }
    /// As `resolve`, for a host about to start: the default directory moves
    /// from `~/.wonder` to `~/.cupola` first (see `data_home`). A directory
    /// named `~/.wonder` explicitly, as a service an older release installed
    /// does, counts as the default.
    fn resolve_for_start() -> Result<Self> {
        #[cfg(unix)]
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let legacy = home.join(wonderd::data_home::LEGACY_DATA_DIR);
            let chosen = std::env::var_os("WONDER_DATA_DIR")
                .map(|path| std::path::absolute(PathBuf::from(path)))
                .transpose()?;
            if chosen.is_none() || chosen.as_ref() == Some(&legacy) {
                let data = wonderd::data_home::move_legacy(&home)?;
                if data != legacy {
                    relocate_service(&legacy, &data);
                }
                // The background supervisor and its children agree on it.
                std::env::set_var("WONDER_DATA_DIR", &data);
                return Self::at(Some(data.into()));
            }
        }
        Self::resolve()
    }
    /// One identity per directory: a relative override becomes absolute, so
    /// every command names the same control channel, whatever working
    /// directory it starts in (LaunchServices on macOS, System32 for the
    /// Windows logon task).
    fn at(override_dir: Option<std::ffi::OsString>) -> Result<Self> {
        let data = match override_dir {
            Some(path) => std::path::absolute(PathBuf::from(path))?,
            None => os::default_data_dir()?,
        };
        Ok(Self {
            state: data.join("headless"),
            data,
        })
    }
    fn config(&self) -> PathBuf {
        self.state.join("config.json")
    }
    /// Where the macOS computer helper reads whether control is allowed
    /// (`WONDER_SERVICE_DIR`). The helper trusts no symlink on the way, so
    /// this names the real path of the data directory.
    #[cfg(target_os = "macos")]
    fn service_dir(&self) -> PathBuf {
        wonderd::canonical_path::canonicalize(&self.data)
            .unwrap_or_else(|_| self.data.clone())
            .join("Service")
    }
    fn capability(&self) -> PathBuf {
        self.state.join("loopback-capability")
    }
    fn origin_status(&self) -> PathBuf {
        self.state.join("origin-status.jsonl")
    }
    fn supervisor_pid(&self) -> PathBuf {
        self.state.join("supervisor.pid")
    }
    fn lock(&self) -> PathBuf {
        self.state.join("supervisor.lock")
    }
    fn logs(&self) -> PathBuf {
        self.data.join("logs")
    }
    #[cfg(unix)]
    fn display_authority(&self) -> PathBuf {
        self.state.join("Xauthority")
    }
}

/// The data directory a login service names. For one release, the default
/// directory goes by its old link `~/.wonder`: reinstalled, the previous
/// release then still finds its data and Keychain items, and this release
/// counts that name as the default (`Paths::resolve_for_start`).
#[cfg(unix)]
fn service_data_dir(paths: &Paths) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let legacy = home.join(wonderd::data_home::LEGACY_DATA_DIR);
        if paths.data == home.join(wonderd::data_home::DATA_DIR) && legacy.is_symlink() {
            return legacy;
        }
    }
    paths.data.clone()
}

/// After the data directory moved, points the login service's files (its
/// log and, on macOS, its launch settings) at the new place, so the service
/// matches what `service install` writes. Until then it works through the
/// link, so a failure is only logged.
#[cfg(unix)]
fn relocate_service(legacy: &Path, target: &Path) {
    #[cfg(target_os = "macos")]
    let result = macos::relocate_service(legacy, target);
    #[cfg(not(target_os = "macos"))]
    let result = relocate_unit_file(legacy, target);
    if let Err(error) = result {
        host_log(format_args!(
            "Could not point the login service at {}: {error}",
            target.display()
        ));
    }
}

fn load_config(paths: &Paths) -> Result<Value> {
    let text = std::fs::read_to_string(paths.config())
        .map_err(|_| "not initialized; run `cupola init` first")?;
    let config = serde_json::from_str(&text)?;
    validate_config(&config)?;
    Ok(config)
}

fn capability(paths: &Paths) -> Result<String> {
    Ok(std::fs::read_to_string(paths.capability())
        .map_err(|_| "not initialized; run `cupola init` first")?
        .trim()
        .to_owned())
}

fn listen_addr(config: &Value) -> String {
    config["listenAddr"]
        .as_str()
        .unwrap_or("127.0.0.1:3777")
        .to_owned()
}

// ---------------------------------------------------------------- init

/// Makes the data directory, and what it holds, private to this user. Init
/// and every start run it, so an installation made before the directory was
/// secured (or whose ACL changed since) is repaired before the host serves
/// its capability; it fails closed.
fn secure_dirs(paths: &Paths) -> Result<()> {
    std::fs::create_dir_all(&paths.state)?;
    os::restrict_dir(&paths.data)?;
    os::restrict_dir(&paths.state)
}

fn init(args: &InitArgs, as_json: bool) -> Result<()> {
    init_at(&Paths::resolve()?, args, as_json)
}

fn init_at(paths: &Paths, args: &InitArgs, as_json: bool) -> Result<()> {
    secure_dirs(paths)?;
    let _lock = supervisor_lock(paths)?;
    let name = args.name.as_deref().map(init_name).transpose()?;
    // Set up again, the host keeps every setting whose flag is left out,
    // including its home network choice (`cupola lan`).
    let previous = load_config(paths).ok();
    let config = init_config(previous.as_ref(), args, name.is_some());
    validate_config(&config)?;
    write_private(
        &paths.config(),
        serde_json::to_string_pretty(&config)?.as_bytes(),
    )?;
    // `--computer` asks for view and control; `computer control off` limits it.
    #[cfg(target_os = "macos")]
    if args.computer {
        control_preference::write(&paths.service_dir(), true)?;
    }
    if !paths.capability().exists() {
        // The loopback capability authorizes this machine's owner (pairing
        // confirmation); it never leaves the host.
        let mut bytes = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
            .map_err(|_| "random generator unavailable")?;
        write_private(&paths.capability(), hex::encode(bytes).as_bytes())?;
    }
    // The host shows the name the owner chose last (`host_name::current`):
    // `--name` saves it where `config set host-name` and the phone do.
    if let Some(name) = &name {
        save_host_name(paths, name)?;
    }
    if as_json {
        print_json(&InitOutput {
            data_dir: paths.data.display().to_string(),
            host_name: name,
            tunnel: if config["tunnel"] == "none" {
                Tunnel::None
            } else {
                Tunnel::Tailscale
            },
            origin: config["origin"].as_str().map(str::to_owned),
            listen_addr: listen_addr(&config),
            https_port: https_port(&config),
            computer: config["computer"] == true,
        });
        return Ok(());
    }
    println!("Initialized Cupola host in {}", paths.data.display());
    println!("{}", init_next_step(previous.as_ref(), &config));
    Ok(())
}

/// The new config: each flag given replaces its setting; the rest stay as
/// `previous` had them (or take their defaults on a first setup).
fn init_config(previous: Option<&Value>, args: &InitArgs, named: bool) -> Value {
    let mut config = previous
        .filter(|previous| previous.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    config["version"] = json!(1);
    if let Some(tunnel) = args.tunnel {
        config["tunnel"] = json!(tunnel.as_str());
        // A new tunnel drops the address routed for the old one.
        config["origin"] = json!(args.origin);
    } else if config["tunnel"].is_null() {
        config["tunnel"] = json!(Tunnel::Tailscale.as_str());
    }
    if let Some(origin) = &args.origin {
        config["origin"] = json!(origin);
    }
    if let Some(listen) = &args.listen {
        config["listenAddr"] = json!(listen);
    } else if config["listenAddr"].is_null() {
        config["listenAddr"] = json!("127.0.0.1:3777");
    }
    if let Some(port) = args.https_port {
        config["httpsPort"] = json!(port);
    } else if config["httpsPort"].is_null() {
        config["httpsPort"] = json!(443);
    }
    if args.computer || args.no_computer {
        config["computer"] = json!(args.computer);
    } else if config["computer"].is_null() {
        config["computer"] = json!(false);
    }
    if named {
        // The stored name replaces the one older setups kept here (the
        // fallback `WONDER_HOST_NAME` on Linux and Windows).
        if let Some(config) = config.as_object_mut() {
            config.remove("hostName");
        }
    }
    config
}

fn https_port(config: &Value) -> u16 {
    config["httpsPort"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .unwrap_or(443)
}

/// `--name`, held to the rules the host's rename applies.
fn init_name(name: &str) -> Result<String> {
    let name = name.trim();
    let max = wonder_store::HOST_NAME_MAX_CHARS;
    if name.is_empty() || name.chars().count() > max || name.chars().any(char::is_control) {
        return Err(format!("--name must be 1 to {max} characters on one line.").into());
    }
    Ok(name.to_owned())
}

/// Saves the owner's chosen name in the host's store. Only `init` writes it
/// directly: it holds the lock, so no host is running.
fn save_host_name(paths: &Paths, name: &str) -> Result<()> {
    let url = format!(
        "sqlite://{}?mode=rwc",
        paths.data.join("wonder.sqlite3").display()
    );
    block_on(async {
        let store = wonder_store::Store::connect(&url).await?;
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        store.set_host_name(Some(name), &now).await?;
        Ok(())
    })
}

/// Where a phone reaches this computer away from home.
fn away_route(config: &Value) -> String {
    match config["origin"].as_str() {
        Some(origin) if config["tunnel"] == "none" => origin.to_owned(),
        _ => format!("Tailscale on port {}", https_port(config)),
    }
}

/// What `init` says to do next. Set up again, the host only needs starting:
/// its key and paired phones are untouched (the lock means it is stopped).
/// A changed away-from-home address says which phones must pair again.
fn init_next_step(previous: Option<&Value>, config: &Value) -> String {
    let Some(previous) = previous else {
        return if cfg!(windows) {
            // Same-Wi-Fi connections are not available on Windows.
            "Next: `cupola start`, then `cupola pair` and scan the code with Cupola on your \
             iPhone. The iPhone connects through Tailscale; install it on both devices first."
        } else if cfg!(all(unix, not(target_os = "macos"))) {
            // Linux has a boot-time service; elsewhere `start` is the next step.
            "Next: `cupola service install` (starts the host now and at boot), then \
             `cupola pair` and scan the code with Cupola on your iPhone, on the same \
             Wi-Fi as this computer."
        } else {
            "Next: `cupola start`, then `cupola pair` and scan the code with Cupola on your \
             iPhone, on the same Wi-Fi as this computer."
        }
        .into();
    };
    let start = "Next: `cupola start` to apply these settings.";
    let (old, new) = (away_route(previous), away_route(config));
    if old == new {
        return format!("{start} Paired phones stay paired; there is no need to pair again.");
    }
    let phones = if home_port(config).is_some() {
        "Paired phones still connect on the same Wi-Fi as this computer; to reach it \
         away from home, a phone paired before needs `cupola pair` again."
    } else {
        "Phones paired before cannot reach it at the new address: run `cupola pair` \
         again for each."
    };
    format!("{start} Away from home, this computer moves from {old} to {new}. {phones}")
}

// ---------------------------------------------------------------- start / stop

fn start(foreground: bool, as_json: bool) -> Result<()> {
    if foreground && as_json {
        return Err("--json is not available with --foreground; use `cupola status --json`".into());
    }
    let paths = Paths::resolve_for_start()?;
    let config = prepare_start(&paths)?;
    #[cfg(all(unix, not(target_os = "macos")))]
    if !foreground && config["tunnel"].as_str() == Some("tailscale") {
        tailscale_serve_permission()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if !foreground && service_installed() {
        // One owner: with the service installed, systemd runs the host.
        systemctl_ok(&["start", UNIT_NAME])?;
        let log_path = log_path(&paths);
        let pid = service_pid();
        if as_json {
            print_json(&StartOutput {
                state: "starting",
                supervisor_pid: pid.unwrap_or(0),
                log_path: log_path.display().to_string(),
            });
            return Ok(());
        }
        println!("Starting Cupola host through its service.");
        println!("Logs: {}", log_path.display());
        return Ok(());
    }
    if !foreground {
        #[cfg(target_os = "macos")]
        return macos::start(&paths, as_json);
        #[cfg(not(target_os = "macos"))]
        {
            std::fs::create_dir_all(paths.logs())?;
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(paths.logs().join("wonder.log"))?;
            let pid = os::spawn_background(log)?;
            let log_path = log_path(&paths);
            if as_json {
                print_json(&StartOutput {
                    state: "starting",
                    supervisor_pid: pid,
                    log_path: log_path.display().to_string(),
                });
                return Ok(());
            }
            println!("Starting Cupola host (pid {pid}).");
            println!("Logs: {}", log_path.display());
            return Ok(());
        }
    }
    supervise(&paths, &config)
}

/// What `start` checks and repairs before it launches a supervisor; the
/// background supervisor runs it again as `start --foreground`.
fn prepare_start(paths: &Paths) -> Result<Value> {
    let config = load_config(paths)?;
    if let Some(pid) = running_supervisor(paths) {
        return Err(format!("already running (pid {pid}); `cupola stop` first").into());
    }
    secure_dirs(paths)?;
    Ok(config)
}

fn running_supervisor(paths: &Paths) -> Option<i32> {
    // The PID is display-only. Stop goes to the private socket, never kill(pid).
    if supervisor_lock(paths).is_ok() {
        return None;
    }
    std::fs::read_to_string(paths.supervisor_pid())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn stop(as_json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    let report = |state| {
        if as_json {
            print_json(&StopOutput { state });
        } else if state == StopState::Stopped {
            println!("Stopped Cupola host.");
        } else {
            println!("Cupola host is not running.");
        }
    };
    report(stop_host(&paths)?);
    Ok(())
}

/// Stops the host, through its service when systemd runs it (so systemd
/// does not count the exit as a failure to restart).
fn stop_host(paths: &Paths) -> Result<StopState> {
    #[cfg(all(unix, not(target_os = "macos")))]
    if service_running() {
        systemctl_ok(&["stop", UNIT_NAME])?;
        return Ok(StopState::Stopped);
    }
    if supervisor_lock(paths).is_ok() {
        return Ok(StopState::NotRunning);
    }
    os::send_stop(paths)?;
    let deadline = Instant::now() + HOST_STOP;
    while Instant::now() < deadline {
        if supervisor_lock(paths).is_ok() {
            return Ok(StopState::Stopped);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("host did not stop within {} seconds", HOST_STOP.as_secs()).into())
}

/// How long a stopping daemon has to finish before the supervisor kills
/// what is left: 2 s for open connections, then its provider bridges, all
/// at once (each up to 10 s plus 2 s to reap, `BRIDGE_EXIT_GRACE` in
/// wonder-app-server) and Codex, with room to spare.
const DAEMON_STOP_GRACE: Duration = Duration::from_secs(30);

/// How long `cupola stop` (and systemd, `TimeoutStopSec`) waits for the
/// whole host: the daemon's grace, then the supervisor's own cleanup.
const HOST_STOP: Duration = Duration::from_secs(DAEMON_STOP_GRACE.as_secs() + 15);

fn validate_config(config: &Value) -> Result<()> {
    let listen: std::net::SocketAddr = listen_addr(config)
        .parse()
        .map_err(|_| "--listen must be a numeric loopback address and port")?;
    if !listen.ip().is_loopback() || listen.port() == 0 {
        return Err("--listen must use a loopback address and a nonzero port".into());
    }
    let port = config["httpsPort"].as_u64().unwrap_or(443);
    if port == 0 || port > u16::MAX.into() {
        return Err("--https-port must be between 1 and 65535".into());
    }
    match config["tunnel"].as_str() {
        Some("tailscale") => (),
        Some("none") => {
            let origin = config["origin"]
                .as_str()
                .ok_or("--tunnel none needs --origin https://host")?;
            let url = reqwest::Url::parse(origin)?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.path() != "/"
                || origin.ends_with('/')
            {
                return Err(
                    "--origin must be https://host[:port], with no path or credentials".into(),
                );
            }
        }
        _ => return Err("--tunnel must be tailscale or none".into()),
    }
    let home = &config["homeNetwork"];
    if !home.is_null()
        && (!home["enabled"].is_boolean()
            || !home["port"]
                .as_u64()
                .is_some_and(|port| (1..=u64::from(u16::MAX)).contains(&port)))
    {
        return Err("homeNetwork needs `enabled` and a port between 1 and 65535; run `cupola lan on` or `cupola lan off`".into());
    }
    Ok(())
}

/// The home network port the host listens on. Same Wi-Fi connections are on
/// unless `cupola lan off` saved otherwise; Windows has no home network
/// listener yet, so there only `cupola lan on` could turn it on (and refuses).
fn home_port(config: &Value) -> Option<u16> {
    let home = &config["homeNetwork"];
    if home.is_null() {
        return (!cfg!(windows)).then_some(cli::DEFAULT_LAN_PORT);
    }
    if home["enabled"].as_bool() != Some(true) {
        return None;
    }
    home["port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
}

fn which(name: &str) -> Option<PathBuf> {
    let names = os::executable_candidates(name);
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
            .find(|path| path.is_file())
    })
}

/// The `opencode` the OpenCode bridge will run: `locateOpenCode` in
/// services/acp-runtime/catalog.mjs, so `providers` and doctor agree with it.
fn opencode_binary() -> Option<PathBuf> {
    locate_opencode(
        std::env::var_os("WONDER_OPENCODE_BIN"),
        std::env::var_os("PATH"),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// The Cursor CLI (`agent`) the Cursor bridge will run: `locateCursor` in
/// services/acp-runtime/catalog.mjs.
fn cursor_binary() -> Option<PathBuf> {
    locate_provider_cli(
        "cursor",
        std::env::var_os("WONDER_CURSOR_BIN"),
        std::env::var_os("PATH"),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn locate_opencode(
    binary: Option<std::ffi::OsString>,
    path: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    locate_provider_cli("opencode", binary, path, home)
}

fn locate_provider_cli(
    provider: &str,
    binary: Option<std::ffi::OsString>,
    path: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let executable = |path: &Path| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        path.is_file()
    };
    if let Some(binary) = binary.filter(|binary| !binary.is_empty()) {
        return Some(PathBuf::from(binary)).filter(|path| executable(path));
    }
    let mut dirs: Vec<PathBuf> = path
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    dirs.retain(|dir| !dir.as_os_str().is_empty());
    #[cfg(windows)]
    let names: Vec<PathBuf> = match provider {
        "cursor" => vec!["agent.exe".into()],
        _ => vec![
            "opencode.exe".into(),
            ["node_modules", "opencode-ai", "bin", "opencode"]
                .iter()
                .collect(),
        ],
    };
    #[cfg(not(windows))]
    let names = [PathBuf::from(if provider == "cursor" {
        "agent"
    } else {
        "opencode"
    })];
    #[cfg(not(windows))]
    {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
        if let Some(home) = home {
            dirs.extend([home.join(".opencode/bin"), home.join(".local/bin")]);
        }
    }
    #[cfg(windows)]
    let _ = home;
    dirs.iter()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| executable(path))
}

/// The native Codex binary. npm installs `codex` as a Node shim; Wonder
/// verifies (and runs) the native binary, whose `codex-code-mode-host` helper
/// sits beside it in the platform package.
#[cfg(unix)]
fn codex_binary() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let bundled = wonderd::provider_runtime::locate_codex();
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    let found = wonderd::canonical_path::canonicalize(which("codex")?).ok()?;
    if found.extension().and_then(|e| e.to_str()) != Some("js") {
        return Some(found);
    }
    npm_codex_binary(&found, std::env::consts::ARCH)
}

/// Windows: a native `codex.exe` on PATH, else the npm global package beside
/// npm's `codex.cmd` shim (`%APPDATA%\npm\node_modules\@openai\codex`).
#[cfg(windows)]
fn codex_binary() -> Option<PathBuf> {
    let found = which("codex")?;
    match found.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("exe") => {
            wonderd::canonical_path::canonicalize(found).ok()
        }
        _ => {
            let shim = found
                .parent()?
                .join("node_modules/@openai/codex/bin/codex.js");
            npm_codex_binary(&shim, std::env::consts::ARCH)
        }
    }
}

fn npm_codex_binary(shim: &Path, arch: &str) -> Option<PathBuf> {
    let package = shim.parent()?.parent()?;
    let (platform, target) = match (std::env::consts::OS, arch) {
        ("macos", "x86_64") => ("codex-darwin-x64", "x86_64-apple-darwin"),
        ("macos", "aarch64") => ("codex-darwin-arm64", "aarch64-apple-darwin"),
        ("windows", "x86_64") => ("codex-win32-x64", "x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => ("codex-win32-arm64", "aarch64-pc-windows-msvc"),
        (_, "x86_64") => ("codex-linux-x64", "x86_64-unknown-linux-musl"),
        (_, "aarch64") => ("codex-linux-arm64", "aarch64-unknown-linux-musl"),
        _ => return None,
    };
    let binary = format!("codex{}", std::env::consts::EXE_SUFFIX);
    // npm may hoist its optional platform package beside @openai/codex.
    [
        package
            .join("node_modules/@openai")
            .join(platform)
            .join("vendor"),
        package.parent()?.join(platform).join("vendor"),
        package.join("vendor"),
    ]
    .into_iter()
    .flat_map(|vendor| {
        [
            vendor.join(target).join("bin").join(&binary),
            vendor.join(target).join("codex").join(&binary),
        ]
    })
    .find(|path| path.is_file())
}

fn sibling(name: &str) -> Option<PathBuf> {
    let exe = executable_path().ok()?;
    let path = exe
        .parent()?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

fn executable_path() -> std::io::Result<PathBuf> {
    // Darwin may return the PATH/helper symlink from current_exe(). Resources
    // always belong to the real signed bundle, never the symlink's directory.
    #[cfg(unix)]
    return wonderd::canonical_path::canonicalize(std::env::current_exe()?);
    // Windows keeps what current_exe() says: canonicalizing a mapped network
    // drive gives `\\?\UNC\…`, which children such as cmd.exe do not accept
    // as a working directory or script path. Comparisons with an installed
    // release folder normalize both sides (`canonical_path::simplified`).
    #[cfg(windows)]
    std::env::current_exe()
}

fn node_binary() -> Option<PathBuf> {
    node_binary_for(executable_path().ok().as_deref())
}

/// `WONDER_NODE_BIN`, then the Node bundled beside `exe` (Cupola.app's
/// `Contents/Resources/node`), then PATH. CLI startup does not put the
/// bundled Node on PATH, so everything that runs Node resolves it here.
fn node_binary_for(exe: Option<&Path>) -> Option<PathBuf> {
    std::env::var_os("WONDER_NODE_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            let path = exe?.parent()?.join("../Resources/node/bin/node");
            path.is_file().then_some(path)
        })
        .or_else(|| which("node"))
}

/// Locates the bundled Claude runtime bridge, installed next to the binaries
/// as `../share/wonder/claude-runtime/main.mjs` (or `claude-runtime/` beside).
fn runtime_bridge(variable: &str, directory: &str) -> Option<PathBuf> {
    runtime_bridge_for(variable, directory, executable_path().ok().as_deref())
}

fn runtime_bridge_for(variable: &str, directory: &str, exe: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(variable) {
        return Some(PathBuf::from(path));
    }
    let dir = exe?.parent()?;
    [
        dir.join("../Resources").join(directory).join("main.mjs"),
        dir.join("../share/wonder").join(directory).join("main.mjs"),
        dir.join(directory).join("main.mjs"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn daemon_command(paths: &Paths, config: &Value) -> Result<Command> {
    let wonderd = std::env::var_os("WONDER_DAEMON_BIN")
        .map(PathBuf::from)
        .or_else(|| sibling("wonderd"))
        .ok_or("wonderd was not found next to the cupola command")?;
    let mut command = Command::new(wonderd);
    command
        .env("WONDER_DATA_DIR", &paths.data)
        .env("WONDER_LOG_DIR", paths.logs())
        .env("WONDER_LOOPBACK_CAPABILITY", capability(paths)?)
        .env("WONDER_LISTEN_ADDR", listen_addr(config))
        .env("WONDER_PUBLIC_ORIGIN_FILE", paths.origin_status());
    match home_port(config) {
        Some(port) => command.env("WONDER_LAN_PORT", port.to_string()),
        // A value exported in the shell that starts the host would turn the
        // listener on behind `wonder lan off`.
        None => command.env_remove("WONDER_LAN_PORT"),
    };
    if let Some(name) = config["hostName"].as_str() {
        command.env("WONDER_HOST_NAME", name);
    }
    // The computer helper refuses Take control unless this directory says
    // the owner allowed it (`cupola computer control on`).
    #[cfg(target_os = "macos")]
    command.env("WONDER_SERVICE_DIR", paths.service_dir());
    // Windows: agents and the daemon resolve `~` through HOME.
    #[cfg(windows)]
    if std::env::var_os("HOME").is_none() {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            command.env("HOME", profile);
        }
    }
    #[cfg(windows)]
    if config["computer"].as_bool() == Some(true) {
        command.env("WONDER_COMPUTER_USE_ENABLED", "1").env(
            "WONDER_COMPUTER_USE_BIN",
            os::computer_helper_launcher(paths)?,
        );
    }
    #[cfg(unix)]
    if config["computer"].as_bool() == Some(true) {
        let helper = computer_helper_launcher(paths)?;
        command
            .env("WONDER_COMPUTER_USE_ENABLED", "1")
            .env("WONDER_COMPUTER_USE_BIN", helper);
        #[cfg(not(target_os = "macos"))]
        match existing_display() {
            // The desktop already running here (a boat box's :0): share it as is.
            Some(display) => {
                let mut environment: std::collections::BTreeMap<_, _> =
                    std::env::vars_os().collect();
                environment.insert("DISPLAY".into(), display.into());
                block_on(async {
                    let environment = wonderd::linux_desktop::refresh(environment).await;
                    for name in wonderd::linux_desktop::SESSION_KEYS {
                        command.env_remove(name);
                        if let Some(value) = environment.get(std::ffi::OsStr::new(name)) {
                            command.env(name, value);
                        }
                    }
                    Ok(())
                })?;
            }
            None => {
                command
                    .env("XAUTHORITY", paths.display_authority())
                    .env("DISPLAY", COMPUTER_DISPLAY);
            }
        }
    }
    if std::env::var_os("WONDER_CODEX_BIN").is_none() {
        if let Some(codex) = codex_binary() {
            command.env("WONDER_CODEX_BIN", codex);
        }
    }
    for (variable, directory) in [
        ("WONDER_CLAUDE_BRIDGE", "claude-runtime"),
        ("WONDER_OPENCODE_BRIDGE", "acp-runtime"),
    ] {
        if let Some(bridge) = runtime_bridge(variable, directory) {
            command.env(variable, bridge);
        }
    }
    if std::env::var_os("WONDER_NODE_BIN").is_none() {
        if let Some(node) = node_binary() {
            command.env("WONDER_NODE_BIN", node);
        }
    }
    command.stdin(Stdio::null());
    Ok(command)
}

// ---------------------------------------------------------------- computer view

#[cfg(unix)]
const COMPUTER_DISPLAY: &str = ":1";
/// `WONDER_COMPUTER_DISPLAY=:0` shares an X11 desktop that is already running
/// instead of starting the private Xvfb the phone otherwise sees.
#[cfg(unix)]
fn existing_display() -> Option<String> {
    std::env::var("WONDER_COMPUTER_DISPLAY")
        .ok()
        .filter(|display| display.starts_with(':'))
}

#[cfg(unix)]
const COMPUTER_SCREEN: &str = "1280x800x24";

#[cfg(unix)]
/// The virtual display must not be readable by another Linux account.
struct DisplayAuthority(PathBuf);

#[cfg(unix)]
impl DisplayAuthority {
    fn create(paths: &Paths) -> Result<Self> {
        let hostname = os::host_name_bytes()?;
        let mut cookie = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut cookie)
            .map_err(|_| "random generator unavailable")?;
        // Xauthority: FamilyLocal, followed by four length-prefixed fields.
        // Write the cookie directly so it never appears in process arguments.
        let mut record = 256u16.to_be_bytes().to_vec();
        for field in [&hostname[..], b"1", b"MIT-MAGIC-COOKIE-1", &cookie] {
            record.extend_from_slice(&(field.len() as u16).to_be_bytes());
            record.extend_from_slice(field);
        }
        let path = paths.display_authority();
        write_private(&path, &record)?;
        Ok(Self(path))
    }
}

#[cfg(unix)]
impl Drop for DisplayAuthority {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Linux and Windows use native sharing where the installation includes it.
/// Python remains an explicit choice and the default where native is absent
/// (archives built without an SDK, source installations without the helper).
#[cfg(not(target_os = "macos"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComputerBackend {
    Python,
    Native,
}
#[cfg(not(target_os = "macos"))]
fn parse_computer_backend(
    value: Option<&str>,
    native_unavailable: bool,
) -> Result<ComputerBackend> {
    match value {
        None if !native_unavailable => Ok(ComputerBackend::Native),
        None | Some("python") => Ok(ComputerBackend::Python),
        Some("native") => Ok(ComputerBackend::Native),
        _ => Err("Screen sharing backend is invalid. Set WONDER_COMPUTER_BACKEND to python or native, then restart Cupola.".into()),
    }
}
#[cfg(not(target_os = "macos"))]
fn computer_backend(paths: &Paths) -> Result<ComputerBackend> {
    if let Some(value) = std::env::var_os("WONDER_COMPUTER_BACKEND") {
        return parse_computer_backend(
            Some(
                value
                    .to_str()
                    .ok_or("WONDER_COMPUTER_BACKEND must be python or native")?,
            ),
            false,
        );
    }
    let saved = match std::fs::read_to_string(paths.state.join("computer-backend")) {
        Ok(value) => Some(value),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let native_unavailable = executable_path()?
        .parent()
        .is_some_and(packaged_native_unavailable);
    parse_computer_backend(saved.as_deref().map(str::trim), native_unavailable)
}
#[cfg(not(target_os = "macos"))]
fn persist_computer_backend(paths: &Paths) -> Result<()> {
    if std::env::var_os("WONDER_COMPUTER_BACKEND").is_some() {
        let backend = computer_backend(paths)?;
        write_private(
            &paths.state.join("computer-backend"),
            match backend {
                ComputerBackend::Python => b"python\n",
                ComputerBackend::Native => b"native\n",
            },
        )?;
    }
    Ok(())
}
/// An archive's `computer-backends.json` says whether it includes native
/// sharing. A source installation has no manifest; it has native sharing only
/// when the helper was installed beside the binaries.
#[cfg(not(target_os = "macos"))]
fn packaged_native_unavailable(dir: &Path) -> bool {
    match std::fs::read(dir.join("computer-backends.json")) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .ok()
            .is_some_and(|manifest| manifest["native"]["included"].as_bool() == Some(false)),
        Err(_) => !native_helper_path(dir).is_file(),
    }
}
#[cfg(not(target_os = "macos"))]
fn native_helper_path(dir: &Path) -> PathBuf {
    dir.join(format!(
        "wonder-computer-helper{}",
        std::env::consts::EXE_SUFFIX
    ))
}
#[cfg(not(target_os = "macos"))]
fn native_computer_helper(dir: &Path) -> Result<PathBuf> {
    let path = native_helper_path(dir);
    if !path.is_file() {
        if packaged_native_unavailable(dir) {
            return Err("Native screen sharing is not included for this platform in this Cupola installation. Set WONDER_COMPUTER_BACKEND=python, install the documented Python screen sharing requirements, and restart Cupola.".into());
        }
        return Err("Native screen sharing is not installed. Reinstall the complete Cupola archive, or set WONDER_COMPUTER_BACKEND=python and restart Cupola.".into());
    }
    Ok(path)
}

/// The Python environment `cupola` keeps beside an installation's versions
/// (`<prefix>/computer-venv`), when it exists. Debian and Ubuntu refuse
/// `pip install` into the system Python, so Python screen sharing lives in a
/// virtual environment that updates and `uninstall.sh` leave alone / remove
/// with the installation.
#[cfg(all(unix, not(target_os = "macos")))]
fn installed_computer_python(exe: &Path) -> Option<PathBuf> {
    let versions = exe.parent()?.parent()?;
    if versions.file_name()? != "versions" {
        return None;
    }
    let python = versions.parent()?.join("computer-venv/bin/python");
    python.is_file().then_some(python)
}

/// The exact commands that make Python screen sharing work for this installation.
#[cfg(all(unix, not(target_os = "macos")))]
fn python_setup_fix(exe: &Path) -> String {
    let requirements = exe
        .parent()
        .map(|dir| dir.join("computer-linux/requirements.txt"))
        .unwrap_or_else(|| PathBuf::from("computer-linux/requirements.txt"));
    let venv = exe
        .parent()
        .and_then(Path::parent)
        .filter(|versions| versions.file_name().is_some_and(|name| name == "versions"))
        .and_then(Path::parent)
        .map(|prefix| prefix.join("computer-venv"))
        .unwrap_or_else(|| PathBuf::from("~/computer-venv"));
    format!(
        "Needs the person: run `sudo apt-get install -y python3-venv` (Debian and Ubuntu), then \
         `python3 -m venv {venv} && {venv}/bin/pip install -r {requirements}`, then restart Cupola. \
         Cupola uses that environment by itself.",
        venv = venv.display(),
        requirements = requirements.display()
    )
}

/// What to do when this archive has only Python screen sharing and its requirements are missing.
#[cfg(all(unix, not(target_os = "macos")))]
fn python_backend_fix() -> String {
    executable_path()
        .map(|exe| python_setup_fix(&exe))
        .unwrap_or_else(|_| NO_NATIVE_PYTHON_FIX.into())
}
#[cfg(windows)]
fn python_backend_fix() -> String {
    NO_NATIVE_PYTHON_FIX.into()
}
#[cfg(not(target_os = "macos"))]
const NO_NATIVE_PYTHON_FIX: &str = "Set WONDER_COMPUTER_BACKEND=python, install the documented Python screen sharing requirements, and restart Cupola. Native screen sharing is not included for this platform.";
#[cfg(not(target_os = "macos"))]
const GENERIC_PYTHON_FIX: &str = "Reinstall the complete Cupola archive; for Python, install computer-linux/requirements.txt or computer-windows/requirements.txt with WONDER_COMPUTER_PYTHON. Choose the backend with WONDER_COMPUTER_BACKEND=python or native, then restart Cupola.";

/// Resolves the installed native helper, or writes the selected Python launcher.
#[cfg(all(unix, not(target_os = "macos")))]
fn computer_helper_launcher(paths: &Paths) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("WONDER_COMPUTER_USE_BIN") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err("WONDER_COMPUTER_USE_BIN does not name a file".into());
        }
        return Ok(path);
    }
    let exe = executable_path()?;
    let dir = exe.parent().ok_or("no executable directory")?;
    if computer_backend(paths)? == ComputerBackend::Native {
        return native_computer_helper(dir);
    }
    let script = [
        dir.join("../share/wonder/computer-linux/wonder_computer_linux.py"),
        dir.join("computer-linux/wonder_computer_linux.py"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or("the Linux computer helper is not installed (share/wonder/computer-linux)")?;
    let python = std::env::var("WONDER_COMPUTER_PYTHON")
        .ok()
        .or_else(|| installed_computer_python(&exe).map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "python3".into());
    let launcher = paths.state.join("computer-helper");
    let body = computer_helper_script(&python, &wonderd::canonical_path::canonicalize(script)?);
    write_private(&launcher, body.as_bytes())?;
    os::make_executable(&launcher)?;
    Ok(launcher)
}

#[cfg(target_os = "macos")]
fn computer_helper_launcher(_: &Paths) -> Result<PathBuf> {
    macos::helper_launcher()
}

/// wonderd passes helper modes such as `--discover-sources` as arguments.
#[cfg(all(unix, not(target_os = "macos")))]
fn computer_helper_script(python: &str, script: &Path) -> String {
    format!(
        "#!/bin/sh\nexec {} {} \"$@\"\n",
        shell_quote(python),
        shell_quote(&script.to_string_lossy())
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
/// The virtual display and window manager the phone sees.
fn display_commands(authority: &Path) -> Vec<Command> {
    let mut xvfb = Command::new("Xvfb");
    xvfb.args([
        COMPUTER_DISPLAY,
        "-screen",
        "0",
        COMPUTER_SCREEN,
        "-nolisten",
        "tcp",
    ])
    .arg("-auth")
    .arg(authority);
    let mut wm = Command::new("openbox");
    wm.env("DISPLAY", COMPUTER_DISPLAY)
        .env("XAUTHORITY", authority);
    [xvfb, wm]
        .into_iter()
        .map(|mut command| {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command
        })
        .collect()
}

// ---------------------------------------------------------------- tailscale

fn tailscale() -> Command {
    let binary = std::env::var_os("TAILSCALE_BIN")
        .map(PathBuf::from)
        .or_else(|| which("tailscale"))
        .or_else(installed_tailscale)
        .unwrap_or_else(|| PathBuf::from("tailscale"));
    let mut command = Command::new(binary);
    // The Mac app's binary answers as the CLI only when TERM is set; without
    // it (a host the login service opened) it tries to start the GUI and
    // prints "The Tailscale GUI failed to start" instead of status JSON.
    if cfg!(target_os = "macos") && std::env::var_os("TERM").is_none() {
        command.env("TERM", "dumb");
    }
    if let Some(socket) = std::env::var_os("TAILSCALE_SOCKET") {
        command.arg("--socket").arg(socket);
    }
    command.stdin(Stdio::null());
    command
}

/// A Wayland login: it has no X11 screen to share (Xwayland shows only X11 apps).
#[cfg(all(unix, not(target_os = "macos")))]
fn session_is_wayland(session_type: Option<&str>, wayland_display: bool) -> bool {
    session_type.is_some_and(|kind| kind.eq_ignore_ascii_case("wayland")) || wayland_display
}

/// The socket an X server listens on for `:0` or `:0.0`.
#[cfg(all(unix, not(target_os = "macos")))]
fn x11_socket(display: &str) -> PathBuf {
    let number = display.trim_start_matches(':');
    let number = number.split('.').next().unwrap_or(number);
    Path::new("/tmp/.X11-unix").join(format!("X{number}"))
}

/// What Computer view needs on this Linux, judged from the display it will
/// share: a desktop named by `WONDER_COMPUTER_DISPLAY` needs nothing installed,
/// Cupola's private display needs Xvfb and openbox. xdotool and xclip only
/// add control and copy/paste, so their absence is a warning.
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_display_checks(
    shared_display: Option<&str>,
    wayland: bool,
    x_server_running: impl Fn(&str) -> bool,
    installed: impl Fn(&str) -> bool,
) -> Vec<Check> {
    const NEEDS_X11: &str = "Computer view needs an X11 session";
    const APT: &str = "Needs the person: run `sudo apt-get install";
    let mut checks = Vec::new();
    match shared_display {
        Some(display) if wayland => checks.push(Check {
            id: "computer",
            status: CheckStatus::Failed,
            summary: format!("{NEEDS_X11}. This is a Wayland session, so {display} shows only X11 apps"),
            fix: Some("Sign out, choose an X11 session (on Ubuntu, \"Ubuntu on Xorg\" on the login screen), then reopen Computer view. Cupola picks up the new session automatically.".into()),
        }),
        Some(display) if !x_server_running(display) => checks.push(Check {
            id: "computer",
            status: CheckStatus::Failed,
            summary: format!("No X11 display is running at {display}"),
            fix: Some("Sign in to an X11 desktop on this computer, then reopen Computer view. If you selected a manually managed display with WONDER_COMPUTER_DISPLAY, check its address and XAUTHORITY and restart the service after changing those settings.".into()),
        }),
        Some(display) => checks.push(Check {
            id: "computer",
            status: CheckStatus::Ok,
            summary: format!("Sharing the X11 display at {display}"),
            fix: None,
        }),
        None => {
            let missing: Vec<&str> = ["Xvfb", "openbox"]
                .into_iter()
                .filter(|tool| !installed(tool))
                .collect();
            checks.push(Check {
                id: "computer",
                status: match (missing.is_empty(), wayland) {
                    (false, _) => CheckStatus::Failed,
                    (true, true) => CheckStatus::Warning,
                    (true, false) => CheckStatus::Ok,
                },
                summary: match (missing.is_empty(), wayland) {
                    (false, _) => format!("Missing for the virtual screen: {}", missing.join(", ")),
                    (true, true) => format!("{NEEDS_X11} to show this desktop. This is a Wayland session, so Cupola shows its own virtual display instead"),
                    (true, false) => "Virtual screen tools are installed".into(),
                },
                fix: (!missing.is_empty()).then(|| format!("{APT} xvfb openbox` (it asks for their password).")),
            });
        }
    }
    let absent: Vec<&str> = ["xdotool", "xclip"]
        .into_iter()
        .filter(|tool| !installed(tool))
        .collect();
    if !absent.is_empty() {
        let effect = match (absent.contains(&"xdotool"), absent.contains(&"xclip")) {
            (true, true) => {
                "the computer can be viewed, but not controlled and copy and paste won't work"
            }
            (true, false) => "the computer can be viewed, but not controlled",
            _ => "copy and paste between the phone and this computer won't work",
        };
        checks.push(Check {
            id: "computer-input",
            status: CheckStatus::Warning,
            summary: format!("Missing {}: {effect}", absent.join(", ")),
            fix: Some(format!(
                "{APT} {}` (it asks for their password).",
                absent.join(" ")
            )),
        });
    }
    checks
}

/// Where the Mac app and the Windows MSI install the CLI; neither always puts
/// it on PATH (the MSI not for a session that was open during install).
fn installed_tailscale() -> Option<PathBuf> {
    let path = if cfg!(windows) {
        PathBuf::from(std::env::var_os("ProgramFiles")?)
            .join("Tailscale")
            .join("tailscale.exe")
    } else {
        PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale")
    };
    path.is_file().then_some(path)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ServeIdentity {
    name: String,
    node: String,
}

impl ServeIdentity {
    fn origin(&self, config: &Value) -> String {
        let port = config["httpsPort"].as_u64().unwrap_or(443);
        if port == 443 {
            format!("https://{}", self.name)
        } else {
            format!("https://{}:{port}", self.name)
        }
    }
}

fn tailscale_output(args: &[&str]) -> Result<process::Output> {
    // process::output also checks this while waiting. Check before spawning so
    // cancellation cannot start another command in a multi-command probe.
    if STOPPING.load(Ordering::SeqCst) {
        return Err("Tailscale check interrupted".into());
    }
    process::output(tailscale().args(args))
}

/// Linux tailscaled lets only root or its operator change Serve. Without that,
/// the host could never publish its route, so say so before starting one.
/// Tailscale being off or signed out is not checked here: the host starts
/// anyway, serves the home network, and keeps retrying Tailscale.
/// An unreadable preference is not a refusal.
#[cfg(all(unix, not(target_os = "macos")))]
fn tailscale_serve_permission() -> Result<()> {
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        return Ok(());
    }
    let Ok(output) = tailscale_output(&["debug", "prefs"]) else {
        return Ok(());
    };
    let prefs: Value = match serde_json::from_slice(&output.stdout) {
        Ok(prefs) if output.status.success() => prefs,
        _ => return Ok(()),
    };
    match user_name(uid) {
        Some(user) => serve_permission(&prefs, &user),
        None => Ok(()),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn serve_permission(prefs: &Value, user: &str) -> Result<()> {
    if prefs["OperatorUser"].as_str() == Some(user) {
        return Ok(());
    }
    Err(format!(
        "Tailscale lets only root or its operator publish Cupola, and {user} is not \
         its operator. Run `sudo tailscale set --operator={user}` once, then retry."
    )
    .into())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn user_name(uid: libc::uid_t) -> Option<String> {
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    let mut buffer = vec![0 as libc::c_char; 4096];
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    name.to_str().ok().map(str::to_owned)
}

/// Linux needs root to sign Tailscale in; the Mac and Windows apps do not.
const TAILSCALE_UP: &str = if cfg!(all(unix, not(target_os = "macos"))) {
    "sudo tailscale up"
} else {
    "tailscale up"
};

/// Why Tailscale cannot carry the host's route yet, in the person's words,
/// with the step only they can take. Doctor shows both; the supervisor logs
/// the summary and keeps retrying.
#[derive(Debug)]
struct TailscaleNotReady {
    summary: String,
    fix: String,
}

impl std::fmt::Display for TailscaleNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary)
    }
}

impl std::error::Error for TailscaleNotReady {}

/// Linux runs tailscaled from the command line; the Mac and Windows apps
/// have their own Connect switch.
const TAILSCALE_APP: bool = !cfg!(all(unix, not(target_os = "macos")));

fn tailscale_not_ready(summary: &str, fix: &str) -> Box<dyn std::error::Error> {
    Box::new(TailscaleNotReady {
        summary: summary.into(),
        fix: fix.into(),
    })
}

fn tailscale_turned_off() -> Box<dyn std::error::Error> {
    if TAILSCALE_APP {
        tailscale_not_ready(
            "Tailscale is turned off: open Tailscale and Connect",
            "open Tailscale and click Connect.",
        )
    } else {
        tailscale_not_ready(
            &format!("Tailscale is turned off: run `{TAILSCALE_UP}`"),
            &format!("run `{TAILSCALE_UP}`."),
        )
    }
}

fn tailscale_identity() -> Result<ServeIdentity> {
    let output = tailscale_output(&["status", "--json"]).map_err(|error| {
        let missing = error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        if missing {
            tailscale_not_ready(
                "Tailscale is not installed",
                &format!("install it from https://tailscale.com/download and sign in with `{TAILSCALE_UP}`."),
            )
        } else {
            error
        }
    })?;
    tailscale_status_identity(output.status.success(), &output.stdout)
}

/// Reads `tailscale status --json`. The Mac app's command answers with no
/// output at all while Tailscale is stopped, and tailscaled reports each
/// backend state separately; each gets its own explanation.
fn tailscale_status_identity(succeeded: bool, stdout: &[u8]) -> Result<ServeIdentity> {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Err(if TAILSCALE_APP {
            tailscale_turned_off()
        } else {
            tailscale_not_ready(
                &format!("Tailscale is not running: start tailscaled, then run `{TAILSCALE_UP}`"),
                &format!("start tailscaled (`sudo systemctl start tailscaled`), then run `{TAILSCALE_UP}`."),
            )
        });
    }
    let status: Value = serde_json::from_slice(stdout)
        .map_err(|error| format!("Tailscale's status could not be read ({error})"))?;
    match status["BackendState"].as_str() {
        Some("Running") => (),
        Some("Stopped") => return Err(tailscale_turned_off()),
        Some("NeedsLogin") => {
            return Err(if TAILSCALE_APP {
                tailscale_not_ready(
                    "This computer is not signed in to Tailscale: open Tailscale and sign in",
                    "open Tailscale and sign in.",
                )
            } else {
                tailscale_not_ready(
                    &format!("This computer is not signed in to Tailscale: run `{TAILSCALE_UP}`"),
                    &format!("run `{TAILSCALE_UP}` and sign in."),
                )
            })
        }
        Some("NeedsMachineAuth") => {
            return Err(tailscale_not_ready(
                "Tailscale is waiting for the tailnet admin to approve this computer",
                "approve this computer in the Tailscale admin console (Machines).",
            ))
        }
        Some("Starting") => {
            return Err(tailscale_not_ready(
                "Tailscale is still connecting; the host keeps retrying",
                "wait a moment; if it stays like this, check the Tailscale app.",
            ))
        }
        _ if !succeeded => return Err(tailscale_turned_off()),
        _ => {
            return Err(tailscale_not_ready(
                "Tailscale is not running on this computer",
                &format!("open Tailscale and Connect, or run `{TAILSCALE_UP}`."),
            ))
        }
    }
    let name = status["Self"]["DNSName"]
        .as_str()
        .map(|name| name.trim_end_matches('.'))
        .filter(|name| {
            name.ends_with(".ts.net")
                && name.split('.').all(|label| {
                    !label.is_empty()
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        })
        .ok_or("MagicDNS name unavailable; enable MagicDNS for the tailnet")?
        .to_owned();
    let node = status["Self"]["ID"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("Tailscale node identity unavailable; no changes made")?
        .to_owned();
    Ok(ServeIdentity { name, node })
}

fn tailscale_serve_status() -> Result<Value> {
    let existing = tailscale_output(&["serve", "status", "--json"])?;
    if !existing.status.success() {
        return Err("could not inspect Tailscale Serve; no changes made".into());
    }
    Ok(serde_json::from_slice(&existing.stdout)?)
}

/// Reconcile only the original node/name, configured port and loopback target.
/// A successful mutation is not evidence of readiness: read it back first.
fn tailscale_serve(
    config: &Value,
    owner: Option<&ServeIdentity>,
    recovering: impl FnOnce(),
) -> Result<ServeIdentity> {
    let identity = tailscale_identity()?;
    if owner.is_some_and(|owner| owner != &identity) {
        return Err("Tailscale identity changed; no changes made".into());
    }
    let port = config["httpsPort"].as_u64().unwrap_or(443);
    let target = format!("http://{}", listen_addr(config));
    let existing = tailscale_serve_status()?;
    if !serve_already_matches(&existing, &identity.name, port, &target)? {
        recovering();
        let served = tailscale_output(&[
            "serve",
            "--bg",
            "--yes",
            &format!("--https={port}"),
            &target,
        ])?;
        if !served.status.success() {
            return Err(format!(
                "Tailscale Serve failed: {}",
                String::from_utf8_lossy(&served.stderr).trim()
            )
            .into());
        }
        if !serve_already_matches(&tailscale_serve_status()?, &identity.name, port, &target)? {
            return Err("Tailscale Serve recovery could not be verified".into());
        }
    }
    // Identity may change while Serve status is being read, even when no
    // mutation was necessary. Never republish that earlier identity as ready.
    if tailscale_identity()? != identity {
        return Err("Tailscale identity changed during check; origin unavailable".into());
    }
    Ok(identity)
}

fn serve_already_matches(status: &Value, name: &str, port: u64, target: &str) -> Result<bool> {
    // A valid JSON scalar or an unexpected map shape is not a free port.
    if !status.is_object()
        || ["TCP", "Web", "AllowFunnel", "Foreground"]
            .iter()
            .any(|field| status.get(field).is_some_and(|value| !value.is_object()))
    {
        return Err("malformed Tailscale Serve status; no changes made".into());
    }
    let valid_port = |key: &str| {
        key.parse::<u16>()
            .is_ok_and(|port| port != 0 && port.to_string() == key)
    };
    let valid_host_port = |key: &str| {
        key.rsplit_once(':')
            .is_some_and(|(host, port)| !host.is_empty() && valid_port(port))
    };
    if status["TCP"].as_object().is_some_and(|entries| {
        entries
            .iter()
            .any(|(key, value)| !valid_port(key) || !value.is_object())
    }) || status["Web"].as_object().is_some_and(|entries| {
        entries
            .iter()
            .any(|(key, value)| !valid_host_port(key) || !value["Handlers"].is_object())
    }) || status["AllowFunnel"].as_object().is_some_and(|entries| {
        entries
            .iter()
            .any(|(key, value)| !valid_host_port(key) || !value.is_boolean())
    }) || status
        .as_object()
        .unwrap()
        .keys()
        .any(|key| !matches!(key.as_str(), "TCP" | "Web" | "AllowFunnel" | "Foreground"))
    {
        return Err("unsupported or malformed Tailscale Serve status; no changes made".into());
    }
    // Foreground sessions can also own the selected port.
    for session in status["Foreground"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
    {
        if serve_already_matches(session, name, port, target)? {
            return Err("Tailscale HTTPS port belongs to a foreground session".into());
        }
    }
    let key = format!("{name}:{port}");
    let suffix = format!(":{port}");
    if status["AllowFunnel"].as_object().is_some_and(|entries| {
        entries.iter().any(|(key, value)| {
            !value.is_boolean() || (key.ends_with(&suffix) && value == &json!(true))
        })
    }) {
        return Err("this HTTPS port has Funnel enabled or unknown; no changes made".into());
    }
    if status["Web"].as_object().is_some_and(|entries| {
        entries
            .keys()
            .any(|other| other != &key && other.ends_with(&suffix))
    }) {
        return Err("Tailscale HTTPS port belongs to another name; no changes made".into());
    }
    let tcp = &status["TCP"][port.to_string()];
    let web = &status["Web"][&key];
    if tcp.is_null() && web.is_null() {
        return Ok(false);
    }
    let sole_handler = web["Handlers"]
        .as_object()
        .is_some_and(|handlers| handlers.len() == 1);
    if tcp == &json!({"HTTPS": true})
        && sole_handler
        && web["Handlers"]["/"] == json!({"Proxy": target})
    {
        return Ok(true);
    }
    Err("Tailscale HTTPS port is already in use; choose another --https-port".into())
}

/// Without Tailscale there is no route to take down, and nothing to warn about.
fn tailscale_installed() -> bool {
    std::env::var_os("TAILSCALE_BIN").is_some_and(|binary| Path::new(&binary).is_file())
        || which("tailscale").is_some()
        || installed_tailscale().is_some()
}

/// Takes down Cupola's own Tailscale route: the same node name, HTTPS port
/// and loopback target `tailscale_serve` publishes. Anything else on the
/// port, or a route already gone, is left alone. `Ok(true)` means removed.
fn tailscale_unpublish(config: &Value) -> Result<bool> {
    if config["tunnel"].as_str() != Some("tailscale") || !tailscale_installed() {
        return Ok(false);
    }
    let identity = tailscale_identity()?;
    let port = config["httpsPort"].as_u64().unwrap_or(443);
    let target = format!("http://{}", listen_addr(config));
    if !serve_already_matches(&tailscale_serve_status()?, &identity.name, port, &target)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    let removed = tailscale_output(&["serve", &format!("--https={port}"), "off"])?;
    if !removed.status.success() {
        return Err(format!(
            "Tailscale Serve could not remove the route: {}",
            String::from_utf8_lossy(&removed.stderr).trim()
        )
        .into());
    }
    if serve_already_matches(&tailscale_serve_status()?, &identity.name, port, &target)
        .unwrap_or(false)
    {
        return Err("Tailscale Serve still lists the route after removing it".into());
    }
    Ok(true)
}

/// `cupola unpublish`: the host must be stopped, or its route monitor would
/// publish the route again.
fn unpublish(as_json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    if running_supervisor(&paths).is_some() {
        return Err(HOST_RUNNING.into());
    }
    let removed = tailscale_unpublish(&load_config(&paths)?)?;
    if as_json {
        print_json(&json!({ "removed": removed }));
    } else if removed {
        println!("Removed Cupola's Tailscale route.");
    } else {
        println!("Cupola has no Tailscale route to remove.");
    }
    Ok(())
}

// Healthy checks run 30 seconds after completion. Failures retry after 5, 10,
// 20, 40, then 60 seconds; success resets the backoff. Test subprocesses use
// shorter intervals, with the same scheduling and 15-second command deadline.
const SERVE_POLL: Duration = Duration::from_millis(if cfg!(test) { 200 } else { 30_000 });
const SERVE_RETRY: Duration = Duration::from_millis(if cfg!(test) { 100 } else { 5_000 });
const SERVE_RETRY_MAX: Duration = Duration::from_millis(if cfg!(test) { 400 } else { 60_000 });

type ServeProbeResult = std::result::Result<ServeIdentity, String>;

enum ServeEvent {
    Recovering,
    Complete(ServeProbeResult),
}

struct ServeMonitor {
    config: Value,
    owner: Option<ServeIdentity>,
    pending: Option<std::thread::JoinHandle<()>>,
    events: Option<std::sync::mpsc::Receiver<ServeEvent>>,
    next_probe: Instant,
    retry: Duration,
}

impl ServeMonitor {
    fn new(config: &Value) -> Self {
        Self {
            config: config.clone(),
            owner: None,
            pending: None,
            events: None,
            next_probe: Instant::now(),
            retry: SERVE_RETRY,
        }
    }

    fn poll(&mut self) -> Option<ServeEvent> {
        let event = self.events.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(event) => Some(event),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(ServeEvent::Complete(Err(
                "Tailscale check worker failed".into(),
            ))),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
        });
        if let Some(ServeEvent::Complete(result)) = event {
            let _ = self.pending.take().unwrap().join();
            self.events = None;
            let delay = if let Ok(identity) = &result {
                self.owner = Some(identity.clone());
                self.retry = SERVE_RETRY;
                SERVE_POLL
            } else {
                let delay = self.retry;
                self.retry = (self.retry * 2).min(SERVE_RETRY_MAX);
                delay
            };
            self.next_probe = Instant::now() + delay;
            return Some(ServeEvent::Complete(result));
        }
        if event.is_some() {
            return event;
        }
        if self.pending.is_none() && Instant::now() >= self.next_probe {
            let config = self.config.clone();
            let owner = self.owner.clone();
            // Each attempt emits at most Recovering + Complete, in order.
            // Capacity two also lets Drop join without draining the receiver.
            let (tx, rx) = std::sync::mpsc::sync_channel(2);
            self.events = Some(rx);
            self.pending = Some(std::thread::spawn(move || {
                let result = tailscale_serve(&config, owner.as_ref(), || {
                    let _ = tx.send(ServeEvent::Recovering);
                })
                .map_err(|error| error.to_string());
                let _ = tx.send(ServeEvent::Complete(result));
            }));
        }
        None
    }
}

impl Drop for ServeMonitor {
    fn drop(&mut self) {
        // Join the sole worker before publishing stopped, including on child
        // failure. output observes cancellation every 20ms and reaps its group.
        STOPPING.store(true, Ordering::SeqCst);
        if let Some(task) = self.pending.take() {
            let _ = task.join();
        }
    }
}

/// One line of the origin status file. `homePort` is the home network port the
/// running daemon was started with (null before it starts, or with the home
/// network off), and `tunnel` the main route it was started with, so `cupola
/// routes` describes the running host rather than a changed config.json.
fn origin_line(
    state: &str,
    origin: Option<&str>,
    home_port: Option<u16>,
    tunnel: Option<&str>,
) -> String {
    let mut line = json!({
        "state": state,
        "origin": origin,
        "homePort": home_port,
        "tunnel": tunnel,
    })
    .to_string();
    line.push('\n');
    line
}

fn append_origin_status(paths: &Paths, line: &str) {
    if let Ok(mut file) = os::append_private(&paths.origin_status()) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// The time at the start of every host log line, local with its offset.
fn log_stamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S%.3f %z")
        .to_string()
}

/// One entry in the host log. The host's stdout and stderr are the log file
/// when it runs in the background or as a service.
fn host_log(message: impl std::fmt::Display) {
    let _ = std::io::stderr()
        .lock()
        .write_all(stamped_lines(&log_stamp(), &message.to_string()).as_bytes());
}

/// `message` with `stamp` before each of its lines, so an error carrying a
/// tool's multi-line output still gives every log line a time.
fn stamped_lines(stamp: &str, message: &str) -> String {
    let message = message.trim_end_matches(['\r', '\n']);
    let mut out = String::new();
    for line in message.split('\n') {
        out.push_str(stamp);
        out.push(' ');
        out.push_str(line.trim_end_matches('\r'));
        out.push('\n');
    }
    out
}

/// How the supervisor answers a host daemon that crashed. On macOS nothing
/// else restarts it: the login job only runs `open` once, at login, and must
/// keep doing so for LaunchServices to own privacy permissions. systemd and
/// the Windows service restart the whole host themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CrashRestarts {
    /// Crashes replaced within `window`; the next one stops the host.
    limit: usize,
    window: Duration,
    /// The wait before the first replacement, doubled for each later one.
    first_delay: Duration,
    max_delay: Duration,
}

impl CrashRestarts {
    const NEVER: Self = Self {
        limit: 0,
        window: Duration::ZERO,
        first_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    };
    /// Five replacements in ten minutes, waiting 1, 2, 4, 8 and 16 seconds.
    const MAC: Self = Self {
        limit: 5,
        window: Duration::from_secs(600),
        first_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(30),
    };

    fn for_host() -> Self {
        if cfg!(target_os = "macos") {
            Self::MAC
        } else {
            Self::NEVER
        }
    }
}

/// The recent crashes one supervisor has replaced.
struct CrashHistory {
    policy: CrashRestarts,
    crashes: std::collections::VecDeque<Instant>,
}

impl CrashHistory {
    fn new(policy: CrashRestarts) -> Self {
        Self {
            policy,
            crashes: Default::default(),
        }
    }

    /// Forgets every crash: a restart someone asked for starts afresh.
    fn clear(&mut self) {
        self.crashes.clear();
    }

    /// Crashes still counted against the limit.
    fn recent(&self) -> usize {
        self.crashes.len()
    }

    /// The wait before replacing a daemon that crashed at `now`, or `None`
    /// once it has crashed too often to keep trying.
    fn restart_after(&mut self, now: Instant) -> Option<Duration> {
        let window = self.policy.window;
        self.crashes
            .retain(|crash| now.saturating_duration_since(*crash) < window);
        if self.crashes.len() >= self.policy.limit {
            return None;
        }
        let delay = self
            .policy
            .first_delay
            .saturating_mul(1 << self.crashes.len().min(16))
            .min(self.policy.max_delay);
        self.crashes.push_back(now);
        Some(delay)
    }
}

/// A failure exit, as systemd's `Restart=on-failure` counts one: a clean exit
/// or a stop signal someone sent on purpose is not a crash.
fn crashed(status: std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if matches!(status.signal(), Some(libc::SIGTERM | libc::SIGINT)) {
            return false;
        }
    }
    !status.success()
}

fn supervise(paths: &Paths, config: &Value) -> Result<()> {
    supervise_with_rescue(
        paths,
        config,
        CrashRestarts::for_host(),
        DAEMON_STOP_GRACE,
        rescue::Options::host(config),
    )
}

/// As `supervise_with_rescue`, without the recovery endpoint or the watchdog.
#[cfg(test)]
fn supervise_with(
    paths: &Paths,
    config: &Value,
    restarts: CrashRestarts,
    stop_grace: Duration,
) -> Result<()> {
    supervise_with_rescue(paths, config, restarts, stop_grace, rescue::Options::OFF)
}

/// `stop_grace` is how long a stopping daemon gets before it is killed.
fn supervise_with_rescue(
    paths: &Paths,
    config: &Value,
    restarts: CrashRestarts,
    stop_grace: Duration,
    mut rescue_options: rescue::Options,
) -> Result<()> {
    host_log(format_args!(
        "Cupola host {} starting (pid {}, data {})",
        wonderd::RELEASE_VERSION,
        std::process::id(),
        paths.data.display()
    ));
    let _lock = supervisor_lock(paths)?;
    std::fs::create_dir_all(paths.logs())?;
    let listener = ControlListener::bind(paths)?;
    write_private(
        &paths.supervisor_pid(),
        std::process::id().to_string().as_bytes(),
    )?;
    STOPPING.store(false, Ordering::SeqCst);
    os::install_stop_handlers();
    // A stale origin from a previous run must not be offered to a phone.
    write_private(
        &paths.origin_status(),
        origin_line("starting", None, None, config["tunnel"].as_str()).as_bytes(),
    )?;
    let shared = rescue::Shared::new();
    // The recovery endpoint is a convenience: a host that cannot open it
    // still runs.
    let endpoint = rescue_options.bind.take().and_then(|bind| {
        match rescue::Endpoint::start(paths, bind, shared.clone()) {
            Ok(endpoint) => Some(endpoint),
            Err(error) => {
                host_log(format_args!("Recovery endpoint unavailable: {error}"));
                None
            }
        }
    });
    let outcome = supervise_children(
        paths,
        config,
        &listener,
        restarts,
        stop_grace,
        rescue::Supervision {
            shared,
            watchdog: rescue_options.watchdog,
            served: endpoint.is_some(),
        },
    );
    drop(endpoint);
    append_origin_status(paths, &origin_line("stopped", None, None, None));
    drop(listener);
    let _ = std::fs::remove_file(paths.supervisor_pid());
    // A failure is logged by whoever prints it (`main`, or the Mac service
    // entry point), stamped as well.
    if outcome.is_ok() {
        host_log("Cupola host stopped");
    }
    outcome
}

fn supervise_children(
    paths: &Paths,
    config: &Value,
    listener: &ControlListener,
    restarts: CrashRestarts,
    stop_grace: Duration,
    rescue: rescue::Supervision,
) -> Result<()> {
    let rescue::Supervision {
        shared,
        watchdog,
        served: rescue_served,
    } = rescue;
    let shared = &shared;
    let mut serve =
        (config["tunnel"].as_str() == Some("tailscale")).then(|| ServeMonitor::new(config));
    // Tailscale may be off or signed out when the host starts. The host
    // starts anyway, serves the home network, and publishes the Tailscale
    // route once a check passes, retrying with backoff until then.
    let mut origin = match serve {
        Some(_) => None,
        None => Some(
            config["origin"]
                .as_str()
                .ok_or("origin missing")?
                .to_owned(),
        ),
    };
    if listener.stopping() {
        return Ok(());
    }
    // Keep the cookie until all display clients and the server have stopped.
    #[cfg(unix)]
    let _authority = if cfg!(target_os = "linux")
        && config["computer"].as_bool() == Some(true)
        && existing_display().is_none()
    {
        Some(DisplayAuthority::create(paths)?)
    } else {
        None
    };
    // Every child owns its process group; Drop also cleans up failed startup.
    let mut children = Vec::new();
    #[cfg(unix)]
    if cfg!(target_os = "linux")
        && config["computer"].as_bool() == Some(true)
        && existing_display().is_none()
    {
        for mut command in display_commands(&paths.display_authority()) {
            children.push(OwnedChild::spawn_logged(&mut command)?);
            std::thread::sleep(Duration::from_millis(800));
            if children
                .last()
                .is_some_and(|child| child.exited().unwrap_or(true))
            {
                return Err(
                    "the virtual display could not start; check Xvfb/openbox installation".into(),
                );
            }
        }
    }
    children.push(OwnedChild::spawn_daemon(&mut daemon_command(
        paths, config,
    )?)?);
    let mut watchdog = rescue::Watchdog::new(watchdog, config);
    watchdog.restarted(Instant::now());
    let home = home_port(config);
    // The main route's own state; what is published also says "starting"
    // while a crashed daemon waits to be replaced.
    let mut route_state = if origin.is_some() {
        "ready"
    } else {
        "starting"
    };
    let mut published_state = route_state;
    append_origin_status(
        paths,
        &origin_line(
            published_state,
            origin.as_deref(),
            home,
            config["tunnel"].as_str(),
        ),
    );
    if let Some(origin) = &origin {
        host_log(format_args!("Serving at {origin}"));
    }
    // Each distinct reason is logged once, not on every retry.
    let mut logged_error = None;
    let mut crashes = CrashHistory::new(restarts);
    // Set while a crashed daemon waits to be replaced; the loop keeps
    // answering a stop meanwhile, and a stop cancels the replacement.
    let mut restart_at: Option<Instant> = None;
    // Set once the daemon crashed too often, until someone restarts it. The
    // supervisor stays up for the recovery endpoint meanwhile.
    let mut gave_up = false;
    // A failed child stops the host unless `restarts` replaces a crashed
    // daemon. A service manager may restart the entire unit; continuing with
    // half a display or a changed PID hides failures.
    while !listener.stopping() {
        let daemon_running = restart_at.is_none() && !gave_up;
        let mut exited = None;
        for (index, child) in children.iter().enumerate() {
            if child.exited()? {
                exited = Some(index);
                break;
            }
        }
        // A daemon that is alive but no longer answering is a crash: it is
        // stopped the hard way below, then replaced under the same limits.
        let mut wedged = false;
        match watchdog.poll(Instant::now()) {
            Some(rescue::WatchdogEvent::Answering) if daemon_running => {
                shared.set_state(RescueState::Running);
            }
            Some(rescue::WatchdogEvent::Wedged) if daemon_running && exited.is_none() => {
                wedged = true;
                exited = children.len().checked_sub(1);
            }
            _ => (),
        }
        if let Some(index) = exited {
            // The daemon is always the last child. `finish` stops whatever
            // it left in its session, provider bridges in process groups of
            // their own included, before any replacement starts.
            let daemon = index + 1 == children.len() && daemon_running;
            let status = children.remove(index).finish()?;
            if !daemon || !(wedged || crashed(status)) {
                return Err("a host process exited; inspect the host logs".into());
            }
            let reason = if wedged {
                "it stopped answering".to_owned()
            } else {
                status.to_string()
            };
            let Some(delay) = crashes.restart_after(Instant::now()) else {
                let message = format!(
                    "the host stopped unexpectedly {} times within {} minutes, so it was not restarted again; inspect the host logs",
                    restarts.limit + 1,
                    restarts.window.as_secs() / 60,
                );
                if restarts.limit == 0 || !rescue_served {
                    return Err(if restarts.limit == 0 {
                        "a host process exited; inspect the host logs".into()
                    } else {
                        message.into()
                    });
                }
                // Stay up: a paired phone can still ask for a restart.
                host_log(format_args!(
                    "{message}. The host stays stopped until it is restarted from a paired phone or with `cupola service restart`."
                ));
                shared.record_exit(reason, restarts.limit + 1);
                shared.set_state(RescueState::GaveUp);
                gave_up = true;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            host_log(format_args!(
                "The host {} ({reason}); restarting it in {:.1} s",
                if wedged {
                    "was not answering"
                } else {
                    "stopped unexpectedly"
                },
                delay.as_secs_f32()
            ));
            shared.record_exit(reason, crashes.recent());
            shared.set_state(RescueState::Starting);
            restart_at = Some(Instant::now() + delay);
        }
        // A phone asked for a restart: now, whatever the daemon is doing.
        // Not a crash, so it counts nothing against the limit, and a host
        // that gave up gets its crash count back.
        if shared.take_restart_request() && !listener.stopping() {
            host_log("Restarting the host at a paired phone's request");
            if gave_up {
                crashes.clear();
                shared.record_exit("restarted from a phone".into(), 0);
            } else if restart_at.is_none() {
                // Ask the daemon to stop and give it the grace its stop gets,
                // unless it already stopped answering.
                if let Some(mut daemon) = children.pop() {
                    daemon.terminate_leader();
                    let grace = if watchdog.failing() {
                        Duration::from_secs(2)
                    } else {
                        stop_grace
                    };
                    let deadline = Instant::now() + grace;
                    while Instant::now() < deadline
                        && !daemon.exited().unwrap_or(true)
                        && !listener.stopping()
                    {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    let _ = daemon.finish();
                }
            }
            gave_up = false;
            restart_at = Some(Instant::now());
            shared.set_state(RescueState::Starting);
        }
        if restart_at.is_some_and(|at| Instant::now() >= at) {
            restart_at = None;
            children.push(OwnedChild::spawn_daemon(&mut daemon_command(
                paths, config,
            )?)?);
            watchdog.restarted(Instant::now());
            host_log(format_args!(
                "Restarted the host (pid {})",
                children.last().map(OwnedChild::id).unwrap_or_default()
            ));
        }
        if let Some(event) = serve.as_mut().and_then(ServeMonitor::poll) {
            route_state = match event {
                ServeEvent::Recovering => "starting",
                ServeEvent::Complete(Ok(identity)) => {
                    origin = Some(identity.origin(config));
                    logged_error = None;
                    "ready"
                }
                ServeEvent::Complete(Err(error)) => {
                    if logged_error.as_ref() != Some(&error) {
                        host_log(format_args!(
                            "Tailscale route unavailable: {error}. The host keeps running and retries."
                        ));
                        logged_error = Some(error);
                    }
                    "unavailable"
                }
            };
        }
        let state = if gave_up {
            "gave-up"
        } else if restart_at.is_some() && route_state == "ready" {
            "starting"
        } else {
            route_state
        };
        if state != published_state {
            let published = (state == "ready").then_some(origin.as_deref()).flatten();
            if let Some(origin) = published.filter(|_| serve.is_some()) {
                host_log(format_args!("Tailscale route ready at {origin}"));
            }
            append_origin_status(
                paths,
                &origin_line(state, published, home, config["tunnel"].as_str()),
            );
            published_state = state;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // A deliberate stop, never a crash: nothing is replaced from here on.
    // Only the daemon's own PID is signalled. It drains its connections and
    // stops its provider bridges and Codex (in its process group) itself, so
    // they can save first; what is left after `stop_grace` is killed when
    // `children` drops. The display helpers stop at once. The daemon is the
    // last child, unless a crashed one awaits replacement.
    shared.set_state(RescueState::Stopped);
    let daemon = match restart_at {
        None if !gave_up => children.len().checked_sub(1),
        _ => None,
    };
    for (index, child) in children.iter_mut().enumerate() {
        if Some(index) == daemon {
            child.terminate_leader();
        } else {
            child.terminate();
        }
    }
    let deadline = Instant::now() + stop_grace;
    while Instant::now() < deadline {
        if children.iter().all(|child| child.exited().unwrap_or(false)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

// ---------------------------------------------------------------- local API

/// How long the CLI waits for an ordinary local API answer.
const API_TIMEOUT: Duration = Duration::from_secs(10);
/// Allow provider discovery/reconnect to finish runtime startup.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(60);

/// The CLI's connection to the daemon: Wonder RPC over `/api/v1/rpc` with
/// the loopback capability, the same protocol any other client uses.
struct Local {
    base: String,
    capability: String,
    connection: tokio::sync::Mutex<Option<wonderd::rpc::client::Client>>,
    timeout: Duration,
    provider_timeout: Duration,
    /// Connections opened so far: a subscription lives on one, so a call
    /// that replaced it has ended the subscription.
    connections: std::sync::atomic::AtomicU64,
    /// True only for a non-JSON pair command with terminal stdin AND stdout.
    pair_interactive: bool,
    /// Which link `cupola pair` asks the daemon for.
    pair_via: PairVia,
}

/// The link `cupola pair` offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PairVia {
    /// `--route tailscale`: the main route only, never same Wi-Fi.
    Main,
    /// `--route home`: a home network address.
    Home,
    /// No `--route`: the main route, or same Wi-Fi while the main route is
    /// not ready and `home` (same Wi-Fi connections) is on.
    Any { home: bool },
}

/// The daemon took longer than the CLI waits. It is running (it accepted
/// the connection), so this is never "not running".
#[derive(Debug)]
struct TimedOut;

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cupola host did not answer in time. Retry, or check `cupola status`.")
    }
}

impl std::error::Error for TimedOut {}

fn not_running() -> Box<dyn std::error::Error> {
    "Cupola host is not running; `cupola start` first".into()
}

impl Local {
    fn connect() -> Result<Self> {
        let paths = Paths::resolve()?;
        let config = load_config(&paths)?;
        Self::at(
            format!("http://{}", listen_addr(&config)),
            capability(&paths)?,
        )
    }

    fn at(base: String, capability: String) -> Result<Self> {
        Self::with_timeouts(base, capability, API_TIMEOUT, PROVIDER_TIMEOUT)
    }

    fn with_timeouts(
        base: String,
        capability: String,
        timeout: Duration,
        provider_timeout: Duration,
    ) -> Result<Self> {
        Ok(Self {
            base,
            capability,
            connection: tokio::sync::Mutex::new(None),
            connections: std::sync::atomic::AtomicU64::new(0),
            timeout,
            provider_timeout,
            pair_interactive: false,
            pair_via: PairVia::Main,
        })
    }

    fn rpc_url(&self) -> String {
        format!("{}/api/v1/rpc", self.base.replacen("http://", "ws://", 1))
    }

    /// Calls a method with the ordinary timeout.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.send(method, params, self.timeout).await
    }

    /// A provider account method, which may start the account's runtime.
    async fn provider(&self, method: &str, params: Value) -> Result<Value> {
        self.send(method, params, self.provider_timeout)
            .await
            .map_err(|error| -> Box<dyn std::error::Error> {
                if error.downcast_ref::<TimedOut>().is_some() {
                    "This computer is still starting the provider. Retry in a minute, or \
                     check `cupola status`."
                        .into()
                } else {
                    error
                }
            })
    }

    async fn send(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let attempt = async {
            let mut connection = self.connection.lock().await;
            // A connection that closed while idle (a prompt left unanswered)
            // is replaced; one that closes mid-call is an error, never a
            // silent second try.
            if connection.as_ref().is_none_or(|c| c.is_closed()) {
                let client = wonderd::rpc::client::Client::connect(
                    &self.rpc_url(),
                    &[("x-wonder-loopback-capability", &self.capability)],
                )
                .await
                .map_err(|_| not_running())?;
                client.initialize("wonder-cli").await.map_err(rpc_error)?;
                *connection = Some(client);
                self.connections
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let client = connection.as_ref().ok_or_else(not_running)?;
            client.call(method, params).await.map_err(rpc_error)
        };
        tokio::time::timeout(timeout, attempt)
            .await
            .map_err(|_| -> Box<dyn std::error::Error> { Box::new(TimedOut) })?
    }
}

impl Local {
    /// The next notification on the current connection (`events.subscribe`
    /// streams), or `None` once it closed.
    async fn notification(&self) -> Option<wonderd::rpc::Notification> {
        let connection = self.connection.lock().await;
        connection.as_ref()?.notification().await
    }

    /// Which connection is current (see `connections`).
    fn connection_number(&self) -> u64 {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn rpc_error(error: wonderd::rpc::client::CallError) -> Box<dyn std::error::Error> {
    match error {
        wonderd::rpc::client::CallError::Rpc(error) => Box::new(ApiError {
            status: error.http_status(),
            reason: error
                .data
                .as_ref()
                .and_then(|data| data.body.as_ref())
                .and_then(|body| body["error"].as_str())
                .map(str::to_owned),
            message: error.message,
        }),
        wonderd::rpc::client::CallError::Closed => not_running(),
    }
}

/// A daemon refusal. The daemon's own `detail` (or plain-text body) is
/// already user language, so it is what the person sees.
#[derive(Debug)]
struct ApiError {
    /// The route's HTTP status, when a route refused.
    status: Option<u16>,
    /// The route's machine-readable `error`, when its body has one.
    reason: Option<String>,
    message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

fn api_status(error: &(dyn std::error::Error + 'static)) -> Option<u16> {
    error.downcast_ref::<ApiError>().and_then(|e| e.status)
}

fn not_found(error: &(dyn std::error::Error + 'static)) -> bool {
    api_status(error) == Some(404)
}

/// The daemon refused only because the typed verification text differs from
/// the phone's; any other refusal (a 403 included) ends pairing.
fn code_mismatch(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<ApiError>().is_some_and(|e| {
        e.status == Some(403) && e.reason.as_deref() == Some(wonderd::PAIRING_CODE_MISMATCH)
    })
}

// ---------------------------------------------------------------- chat

/// `wonder chat`: one message through the host's own orchestration (the
/// RPC calls any client makes), then the turn's events until it ends.
async fn chat(args: cli::ChatArgs, as_json: bool) -> Result<()> {
    let local = Local::connect()?;
    match args.command {
        Some(ChatCommand::Worktree { thread, branch }) => {
            let moved = local
                .call(
                    "threads.moveToWorktree",
                    json!({"threadId": thread, "branch": branch}),
                )
                .await?;
            if as_json {
                println!("{moved}");
            } else {
                println!(
                    "Moved to branch {} in {}. Your next message runs there.",
                    moved["branch"].as_str().unwrap_or_default(),
                    moved["cwd"].as_str().unwrap_or_default()
                );
            }
            Ok(())
        }
        Some(ChatCommand::Queue { thread }) => chat_queue(&local, &thread, as_json).await,
        Some(ChatCommand::SendNow { id, thread }) => {
            chat_send_now(&local, &id, &thread, as_json).await
        }
        Some(ChatCommand::Approve { id }) => chat_answer(&local, &id, true, as_json).await,
        Some(ChatCommand::Deny { id }) => chat_answer(&local, &id, false, as_json).await,
        None => {
            let mut printer = ChatPrinter::new(as_json);
            let limit = Duration::from_secs(args.timeout);
            let status = chat_run(&local, &args, limit, &mut |event| printer.print(event)).await?;
            if status != ChatStatus::Completed {
                let _ = std::io::stdout().flush();
                std::process::exit(status.exit_code());
            }
            Ok(())
        }
    }
}

/// `cupola delegate`: a task for a new child of a thread.
async fn delegate(args: cli::DelegateArgs, as_json: bool) -> Result<()> {
    let local = Local::connect()?;
    let mut params = json!({
        "threadId": args.parent,
        "idempotencyKey": args.key.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        "prompt": args.prompt,
        "provider": args.provider.as_str(),
    });
    for (key, value) in [
        ("model", args.model),
        ("effort", args.effort),
        ("title", args.title),
        ("folder", args.folder.or(args.worktree)),
    ] {
        if let Some(value) = value {
            params[key] = value.into();
        }
    }
    if let Some(cap) = args.max_children {
        params["maxChildren"] = cap.into();
    }
    let child = local.call("threads.delegate", params).await?;
    if as_json {
        print_json(&child);
    } else {
        println!("{}", child["threadId"].as_str().unwrap_or_default());
    }
    Ok(())
}

/// Prints chat events: one JSON object per line, or the reply on stdout
/// with everything else on stderr.
struct ChatPrinter {
    json: bool,
    /// Reply text was printed since the last complete message.
    streamed: bool,
    /// The cursor is not at the start of a line on stdout.
    open_line: bool,
}

impl ChatPrinter {
    fn new(json: bool) -> Self {
        Self {
            json,
            streamed: false,
            open_line: false,
        }
    }

    fn end_line(&mut self) {
        if self.open_line {
            println!();
            self.open_line = false;
        }
    }

    fn print(&mut self, event: &ChatEvent) {
        if self.json {
            println!("{}", serde_json::to_string(event).unwrap_or_default());
            let _ = std::io::stdout().flush();
            return;
        }
        match event {
            ChatEvent::Sent {
                thread_id,
                created,
                queued,
                ..
            } => {
                let thread = if *created { "new thread" } else { "thread" };
                eprintln!("Sent in {thread} {thread_id}.");
                if *queued {
                    eprintln!("It waits in the queue until the work before it finishes.");
                }
            }
            ChatEvent::Text { text } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                self.streamed = true;
                self.open_line = !text.ends_with('\n');
            }
            ChatEvent::Message { text } => {
                if !self.streamed {
                    print!("{text}");
                    self.open_line = !text.ends_with('\n');
                }
                self.streamed = false;
                self.end_line();
            }
            ChatEvent::Tool {
                category,
                state,
                detail,
            } => {
                self.end_line();
                match detail {
                    Some(detail) => eprintln!("· {category} {state}: {detail}"),
                    None => eprintln!("· {category} {state}"),
                }
            }
            ChatEvent::Approval {
                approval_id,
                summary,
                ..
            } => {
                self.end_line();
                eprintln!("Waiting for approval {approval_id}: {summary}");
                eprintln!("  Answer it in Cupola on your iPhone; the reply continues here.");
            }
            ChatEvent::ApprovalResolved { approval_id, .. } => {
                eprintln!("Approval {approval_id} answered.");
            }
            ChatEvent::State { .. } => {}
            ChatEvent::Notice { message } => {
                self.end_line();
                eprintln!("{message}");
            }
            ChatEvent::End { status, detail, .. } => {
                self.end_line();
                let said = match status {
                    ChatStatus::Completed => return,
                    ChatStatus::Failed => "The reply failed",
                    ChatStatus::Interrupted => "The reply was stopped before it finished",
                    ChatStatus::TimedOut => {
                        "Stopped following; the reply is still running. Read it in Cupola"
                    }
                };
                match detail {
                    Some(detail) => eprintln!("{said}: {detail}"),
                    None => eprintln!("{said}."),
                }
            }
        }
    }
}

/// The send's delivery states that end a turn, and how.
fn chat_terminal(state: &str) -> Option<ChatStatus> {
    match state {
        "completed" => Some(ChatStatus::Completed),
        "interrupted" => Some(ChatStatus::Interrupted),
        "failed" | "uncertain" | "safe_to_retry" => Some(ChatStatus::Failed),
        _ => None,
    }
}

/// What one host event means for the message being followed.
#[derive(Debug, PartialEq)]
enum ChatStep {
    Emit(ChatEvent),
    /// Approvals on the thread changed; read them again (with the
    /// decision when one was answered).
    Approvals(Option<String>),
    End(ChatStatus),
}

/// The message `wonder chat` follows, and what it learned of its turn.
struct ChatFollow {
    thread: String,
    message: String,
    turn: Option<String>,
}

impl ChatFollow {
    /// Classifies one `hostEventEnvelope`. The message is followed by its
    /// turn once the host names it: a message sent into a running reply
    /// (Send now) joins that turn, whose events and end carry the message
    /// that started it. Until then (it waits in the queue) only the
    /// message's own state counts, since the thread streams the turn ahead.
    fn classify(&mut self, envelope: &Value) -> Option<ChatStep> {
        let mine = envelope["messageId"].as_str() == Some(self.message.as_str());
        let turn = envelope["turnId"].as_str();
        let our_turn = turn.is_some() && turn == self.turn.as_deref();
        let event = &envelope["event"];
        let data = &event["data"];
        let kind = event["type"].as_str()?;
        if kind == "message_state" {
            let state = data["state"].as_str()?;
            if mine {
                // Back in the queue (a Send now that missed the reply) it
                // has no turn, whatever the envelope still names; any other
                // state names the turn it is in.
                if state == "accepted_by_wonder" {
                    self.turn = None;
                } else if let Some(turn) = turn {
                    self.turn = Some(turn.to_owned());
                }
            } else if !(our_turn && chat_terminal(state).is_some()) {
                return None;
            }
            return Some(match chat_terminal(state) {
                Some(status) => ChatStep::End(status),
                None => ChatStep::Emit(ChatEvent::State {
                    state: state.to_owned(),
                }),
            });
        }
        let here = mine || envelope["conversationId"].as_str() == Some(self.thread.as_str());
        let counts = match (turn, self.turn.as_deref()) {
            (Some(_), Some(_)) => our_turn,
            (None, Some(_)) => envelope["messageId"].as_str().is_none() || mine,
            (_, None) => mine,
        };
        if !here || !counts {
            return None;
        }
        let text = |key: &str| data[key].as_str().map(str::to_owned);
        Some(match kind {
            "assistant_delta" => ChatStep::Emit(ChatEvent::Text {
                text: text("text")?,
            }),
            "assistant_completed" => ChatStep::Emit(ChatEvent::Message {
                text: text("text")?,
            }),
            "activity" => {
                let category = match data["category"].as_str()? {
                    "command" => "command",
                    "file change" => "fileChange",
                    "MCP tool" => "tool",
                    "plan" => "plan",
                    "reasoning" => "reasoning",
                    _ => return None,
                };
                ChatStep::Emit(ChatEvent::Tool {
                    category: category.into(),
                    state: text("state")?,
                    detail: text("detail"),
                })
            }
            "approval_opened" => ChatStep::Approvals(None),
            "approval_resolved" => ChatStep::Approvals(text("decision")),
            "terminal_error" => ChatStep::Emit(ChatEvent::Notice {
                message: text("message")?,
            }),
            _ => return None,
        })
    }
}

/// The Project named `wanted`: its ID, else its name in any case.
fn chat_project(projects: &Value, wanted: &str) -> Result<Value> {
    let list = projects["projects"].as_array().cloned().unwrap_or_default();
    if let Some(project) = list.iter().find(|p| p["id"] == wanted) {
        return Ok(project.clone());
    }
    let named: Vec<&Value> = list
        .iter()
        .filter(|p| {
            p["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(wanted.trim()))
        })
        .collect();
    match named.as_slice() {
        [project] => Ok((*project).clone()),
        [] => {
            let names: Vec<&str> = list.iter().filter_map(|p| p["name"].as_str()).collect();
            Err(if names.is_empty() {
                "This computer has no Projects yet. Add one in Cupola on your iPhone.".into()
            } else {
                format!(
                    "No Project is named “{wanted}”. Projects here: {}.",
                    names.join(", ")
                )
                .into()
            })
        }
        several => {
            let ids: Vec<&str> = several.iter().filter_map(|p| p["id"].as_str()).collect();
            Err(format!(
                "Several Projects are named “{wanted}”. Name one by its ID: {}.",
                ids.join(", ")
            )
            .into())
        }
    }
}

/// The provider and model to use: as given, a given model's own provider,
/// else the provider's default model in the host's catalog (`models.list`).
fn chat_model(
    models: &Value,
    provider: Option<&str>,
    model: Option<&str>,
    fallback: Option<&str>,
) -> Result<(String, String)> {
    let list = models["models"].as_array().cloned().unwrap_or_default();
    if let Some(model) = model {
        let family = match provider {
            Some(provider) => provider.to_owned(),
            None => list
                .iter()
                .find(|m| m["id"] == model)
                .and_then(|m| m["agentFamily"].as_str())
                .map(str::to_owned)
                .ok_or_else(|| {
                    format!("No provider here offers the model “{model}”. Name it with --provider.")
                })?,
        };
        return Ok((family, model.to_owned()));
    }
    let family = provider.or(fallback).unwrap_or("codex");
    let offered: Vec<&Value> = list.iter().filter(|m| m["agentFamily"] == family).collect();
    let chosen = offered
        .iter()
        .find(|m| m["isDefault"] == true)
        .or(offered.first())
        .and_then(|m| m["id"].as_str())
        .ok_or_else(|| {
            format!(
                "This computer offers no {family} model to choose by default. Name one with \
                 --model, or check `cupola providers`."
            )
        })?;
    Ok((family.to_owned(), chosen.to_owned()))
}

/// A send or thread creation, retried once with the same key if the host
/// did not answer in time: the key makes the retry the same message.
async fn chat_submit(local: &Local, method: &str, params: Value) -> Result<Value> {
    match local
        .send(method, params.clone(), local.provider_timeout)
        .await
    {
        Err(error) if error.downcast_ref::<TimedOut>().is_some() => {
            local.provider(method, params).await
        }
        other => other,
    }
}

/// Sends the message and follows its turn, emitting each event, until the
/// turn ends or `limit` passes.
async fn chat_run(
    local: &Local,
    args: &cli::ChatArgs,
    limit: Duration,
    emit: &mut dyn FnMut(&ChatEvent),
) -> Result<ChatStatus> {
    let deadline = tokio::time::Instant::now() + limit;
    let prompt = args.prompt.clone().unwrap_or_default();
    if prompt.trim().is_empty() {
        return Err("The message is empty.".into());
    }
    let provider = args.provider.map(Provider::as_str);
    // Follow from before the send, so no event of the turn falls between.
    let subscribed = local.call("events.subscribe", json!({})).await?;
    let mut stream = ChatStream::new(&subscribed, local.connection_number());
    let key = uuid::Uuid::new_v4().to_string();
    let (project_id, thread, receipt, created) = match &args.thread {
        Some(thread) => {
            let mut params = json!({"threadId": thread, "idempotencyKey": key, "body": prompt});
            if provider.is_some() || args.model.is_some() {
                let models = local.call("models.list", Value::Null).await?;
                let (family, model) = chat_model(&models, provider, args.model.as_deref(), None)?;
                params["projectModel"] = json!({"family": family, "model": model});
            }
            let receipt = chat_submit(local, "threads.send", params).await?;
            (None, thread.clone(), receipt, false)
        }
        None => {
            let project = if args.scratch {
                json!({"id": "scratch", "name": "No project"})
            } else {
                let projects = local.call("projects.list", Value::Null).await?;
                chat_project(&projects, args.project.as_deref().unwrap_or_default())?
            };
            let models = local.call("models.list", Value::Null).await?;
            let (family, model) = chat_model(
                &models,
                provider,
                args.model.as_deref(),
                project["lastFamily"].as_str(),
            )?;
            let project_id = project["id"].as_str().unwrap_or_default().to_owned();
            let created = chat_submit(
                local,
                "threads.create",
                json!({"projectId": project_id, "idempotencyKey": key, "body": prompt,
                       "family": family, "model": model}),
            )
            .await?;
            let thread = created["conversation"]["conversationId"]
                .as_str()
                .ok_or("The host started the thread but did not name it.")?
                .to_owned();
            (Some(project_id), thread, created["receipt"].clone(), true)
        }
    };
    let message = receipt["wonderMessageId"]
        .as_str()
        .ok_or("The host did not confirm the message.")?
        .to_owned();
    emit(&ChatEvent::Sent {
        project_id,
        thread_id: thread.clone(),
        message_id: message.clone(),
        created,
        queued: receipt["queued"] == true,
    });
    let mut follow = ChatFollow {
        thread,
        message,
        turn: receipt["codexTurnId"].as_str().map(str::to_owned),
    };
    let end = |emit: &mut dyn FnMut(&ChatEvent), status: ChatStatus, detail: Option<String>| {
        emit(&ChatEvent::End {
            status,
            exit_code: status.exit_code(),
            detail,
        });
        status
    };
    if let Some(status) = receipt["deliveryState"].as_str().and_then(chat_terminal) {
        return Ok(end(emit, status, None));
    }
    let mut announced: Vec<String> = Vec::new();
    let mut last_tool: Option<ChatEvent> = None;
    // Set when the stream must be followed again: `Some(false)` resumes
    // after the cursor, `Some(true)` (resync) follows from now.
    let mut resume = None;
    if chat_call(
        deadline,
        chat_approvals(local, &follow, &mut announced, None, emit),
    )
    .await?
    .is_none()
    {
        resume = Some(false);
    }
    let mut noticed = false;
    loop {
        if let Some(resync) = resume.take() {
            // Resume after the cursor, or (resync) follow from now first and
            // then read the thread: an end that committed in between is in
            // one or the other, never lost.
            let after = (!resync).then(|| stream.cursor.clone());
            if !chat_resubscribe(local, after.as_ref(), deadline, &mut stream).await? {
                let detail = "The host stopped answering; the reply may still be running.";
                return Ok(end(emit, ChatStatus::TimedOut, Some(detail.into())));
            }
            if resync {
                if !noticed {
                    noticed = true;
                    emit(&ChatEvent::Notice {
                        message: "The host's event stream restarted; some of the reply may be \
                                  missing here. The thread in Cupola has all of it."
                            .into(),
                    });
                }
                let Some(read) = chat_call(deadline, chat_message(local, &follow)).await? else {
                    resume = Some(true);
                    continue;
                };
                if let Some(message) = read {
                    if let Some(status) = message["state"].as_str().and_then(chat_terminal) {
                        let detail = message["deliveryError"].as_str().map(str::to_owned);
                        return Ok(end(emit, status, detail));
                    }
                    // The turn it joined or started while the stream was down.
                    follow.turn = match message["state"].as_str() {
                        Some("accepted_by_wonder") => None,
                        _ => message["codexTurnId"]
                            .as_str()
                            .map(str::to_owned)
                            .or(follow.turn.take()),
                    };
                }
            }
            // An approval opened or answered in the gap is not replayed
            // when its event was read before the connection dropped.
            let approvals = chat_approvals(local, &follow, &mut announced, None, emit);
            if chat_call(deadline, approvals).await?.is_none() {
                resume = Some(false);
            }
            continue;
        }
        // A call that opened a new connection ended the subscription with
        // the old one: nothing more arrives on this one.
        let next = if stream.connection == local.connection_number() {
            match tokio::time::timeout_at(deadline, local.notification()).await {
                Ok(next) => next,
                Err(_) => return Ok(end(emit, ChatStatus::TimedOut, None)),
            }
        } else {
            None
        };
        let note = match next {
            Some(note) if !stream.owns(&note) => continue,
            Some(note) if note.method == "event" => note,
            // The host cannot replay from our cursor (`resync`).
            Some(note) if note.method == "events.ended" => {
                resume = Some(true);
                continue;
            }
            Some(_) => continue,
            // The connection closed or was replaced: the journal still
            // holds every event after our cursor.
            None => {
                resume = Some(false);
                continue;
            }
        };
        stream.advance(&note.params["event"]);
        let turn = follow.turn.clone();
        let step = follow.classify(&note.params["event"]);
        // Approvals are announced for this message's turn only, so read
        // them again when the host names (or changes) it.
        let reread = follow.turn != turn && !matches!(step, Some(ChatStep::End(_)));
        if reread {
            let approvals = chat_approvals(local, &follow, &mut announced, None, emit);
            if chat_call(deadline, approvals).await?.is_none() {
                resume = Some(false);
            }
        }
        match step {
            None => {}
            Some(ChatStep::Emit(event)) => {
                // Reasoning and output progress repeat per token; say it once.
                let tool = matches!(event, ChatEvent::Tool { .. });
                if !(tool && last_tool.as_ref() == Some(&event)) {
                    emit(&event);
                }
                last_tool = tool.then_some(event);
            }
            Some(ChatStep::Approvals(decision)) => {
                let approvals = chat_approvals(local, &follow, &mut announced, decision, emit);
                if chat_call(deadline, approvals).await?.is_none() {
                    resume = Some(false);
                }
            }
            Some(ChatStep::End(status)) => {
                let detail = match status {
                    ChatStatus::Completed => None,
                    _ => chat_call(deadline, chat_message(local, &follow))
                        .await
                        .ok()
                        .flatten()
                        .flatten()
                        .and_then(|m| m["deliveryError"].as_str().map(str::to_owned)),
                };
                return Ok(end(emit, status, detail));
            }
        }
    }
}

/// One host call while following, by the deadline. `None` when the
/// connection dropped, or the host did not answer by the deadline or the
/// call's own timeout: the stream is then followed again. A refusal is an
/// error.
async fn chat_call<T>(
    deadline: tokio::time::Instant,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<Option<T>> {
    match tokio::time::timeout_at(deadline, work).await {
        Ok(Ok(value)) => Ok(Some(value)),
        Ok(Err(error)) if error.downcast_ref::<ApiError>().is_some() => Err(error),
        Ok(Err(_)) | Err(_) => Ok(None),
    }
}

/// Where `wonder chat`'s event subscription is: its ID, the connection it
/// lives on, and the cursor of the last event read, to resume from after a
/// lost connection.
struct ChatStream {
    id: Option<String>,
    connection: u64,
    cursor: Value,
}

impl ChatStream {
    fn new(subscribed: &Value, connection: u64) -> Self {
        Self {
            id: subscribed["subscriptionId"].as_str().map(str::to_owned),
            connection,
            cursor: subscribed["cursor"].clone(),
        }
    }

    /// Notifications of an earlier subscription (one that ended) are not
    /// this stream's.
    fn owns(&self, note: &wonderd::rpc::Notification) -> bool {
        match (note.params["subscriptionId"].as_str(), self.id.as_deref()) {
            (Some(theirs), Some(ours)) => theirs == ours,
            _ => true,
        }
    }

    fn advance(&mut self, envelope: &Value) {
        if envelope["sequence"].is_u64() {
            self.cursor =
                json!({"hostEpoch": envelope["hostEpoch"], "sequence": envelope["sequence"]});
        }
    }
}

/// Subscribes again, after `cursor` or from now, retrying while the host
/// restarts. False when the deadline passed first.
async fn chat_resubscribe(
    local: &Local,
    cursor: Option<&Value>,
    deadline: tokio::time::Instant,
    stream: &mut ChatStream,
) -> Result<bool> {
    let params = match cursor {
        Some(cursor) if cursor.is_object() => json!({"after": cursor}),
        _ => json!({}),
    };
    while tokio::time::Instant::now() < deadline {
        let subscribe = local.call("events.subscribe", params.clone());
        if let Ok(Ok(subscribed)) = tokio::time::timeout_at(deadline, subscribe).await {
            *stream = ChatStream::new(&subscribed, local.connection_number());
            return Ok(true);
        }
        tokio::time::sleep_until(
            (tokio::time::Instant::now() + Duration::from_millis(500)).min(deadline),
        )
        .await;
    }
    Ok(false)
}

/// The followed message as the thread itself has it.
async fn chat_message(local: &Local, follow: &ChatFollow) -> Result<Option<Value>> {
    let snapshot = local
        .call("threads.read", json!({"threadId": follow.thread}))
        .await?;
    Ok(snapshot["messages"].as_array().and_then(|messages| {
        messages
            .iter()
            .find(|m| m["messageId"] == follow.message.as_str())
            .cloned()
    }))
}

/// Announces the waiting approvals of the followed message's turn not yet
/// printed, and the printed ones that were answered. While the message
/// waits in the queue, the thread's approvals are the turn ahead's.
async fn chat_approvals(
    local: &Local,
    follow: &ChatFollow,
    announced: &mut Vec<String>,
    decision: Option<String>,
    emit: &mut dyn FnMut(&ChatEvent),
) -> Result<()> {
    let list = local.call("approvals.list", Value::Null).await?;
    let thread: Vec<&Value> = list
        .as_array()
        .map(|list| {
            list.iter()
                .filter(|a| a["conversationId"] == follow.thread.as_str())
                .collect()
        })
        .unwrap_or_default();
    let ids: Vec<&str> = thread
        .iter()
        .filter_map(|a| a["approvalId"].as_str())
        .collect();
    let waiting = thread
        .iter()
        .filter(|a| match (a["params"]["turnId"].as_str(), &follow.turn) {
            (_, None) => false,
            (Some(theirs), Some(ours)) => theirs == ours,
            (None, Some(_)) => true,
        });
    announced.retain(|id| {
        let open = ids.contains(&id.as_str());
        if !open {
            emit(&ChatEvent::ApprovalResolved {
                approval_id: id.clone(),
                decision: decision.clone(),
            });
        }
        open
    });
    for approval in waiting {
        let Some(id) = approval["approvalId"].as_str() else {
            continue;
        };
        if announced.iter().any(|seen| seen == id) {
            continue;
        }
        announced.push(id.to_owned());
        let kind = approval["method"].as_str().unwrap_or_default();
        emit(&ChatEvent::Approval {
            approval_id: id.to_owned(),
            kind: kind.to_owned(),
            summary: approval_summary(kind, &approval["params"]),
            decisions: approval["params"]["availableDecisions"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        });
    }
    Ok(())
}

/// What an approval asks, in the person's words.
fn approval_summary(kind: &str, params: &Value) -> String {
    let field = |key: &str| params[key].as_str().filter(|v| !v.is_empty());
    let asked = match kind {
        "item/commandExecution/requestApproval" => match field("command") {
            Some(command) => format!("run `{command}`"),
            None => "run a command".into(),
        },
        "item/fileChange/requestApproval" => match field("grantRoot") {
            Some(root) => format!("change files in {root}"),
            None => "change files".into(),
        },
        "item/permissions/requestApproval" => "use more access on this computer".into(),
        "item/tool/requestUserInput" => {
            return params["notificationQuestion"]["body"]
                .as_str()
                .map(|q| format!("The agent asks: {q}"))
                .unwrap_or_else(|| "The agent asks a question.".into())
        }
        "mcpServer/elicitation/request" => "fill in a form for a tool".into(),
        _ => "use a tool".into(),
    };
    match field("reason") {
        Some(reason) => format!("The agent wants to {asked}. {reason}"),
        None => format!("The agent wants to {asked}."),
    }
}

/// `wonder chat queue`.
async fn chat_queue(local: &Local, thread: &str, as_json: bool) -> Result<()> {
    let queue = local
        .call("threads.queue", json!({"threadId": thread}))
        .await?;
    let queue = queue.as_array().cloned().unwrap_or_default();
    if as_json {
        print_json(&cli::ChatQueueOutput {
            thread_id: thread.to_owned(),
            queue,
        });
        return Ok(());
    }
    if queue.is_empty() {
        println!("No messages are waiting in this thread.");
    }
    for item in queue {
        let body = item["body"].as_str().unwrap_or_default();
        let first: String = body
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(60)
            .collect();
        println!("{}  {first}", item["id"].as_str().unwrap_or("?"));
    }
    Ok(())
}

/// `wonder chat send-now`: moves a queued message into the running turn,
/// as the iPhone's "Send now" does.
async fn chat_send_now(local: &Local, id: &str, thread: &str, as_json: bool) -> Result<()> {
    let queue = local
        .call("threads.queue", json!({"threadId": thread}))
        .await?;
    let item = queue
        .as_array()
        .and_then(|queue| queue.iter().find(|m| m["id"] == id))
        .ok_or("That message is not waiting in this thread's queue. See `cupola chat queue`.")?;
    let snapshot = local
        .call("threads.read", json!({"threadId": thread}))
        .await?;
    let turn = snapshot["thread"]["turns"]
        .as_array()
        .and_then(|turns| turns.iter().rev().find(|t| t["status"] == "inProgress"))
        .and_then(|t| t["id"].as_str())
        .ok_or("Nothing is running in this thread; the message is sent next on its own.")?;
    local
        .call(
            "queue.edit",
            json!({"threadId": thread, "messageId": id,
                   "expectedRevision": item["revision"], "expectedTurnId": turn}),
        )
        .await?;
    if as_json {
        print_json(&cli::ChatActionOutput {
            id: id.to_owned(),
            state: "sentNow",
        });
    } else {
        println!("Sent into the running reply.");
    }
    Ok(())
}

/// `wonder chat approve|deny`. The host lets only a paired device answer,
/// with its signature; the owner's answer is refused, and this says so.
async fn chat_answer(local: &Local, id: &str, allow: bool, as_json: bool) -> Result<()> {
    let list = local.call("approvals.list", Value::Null).await?;
    let approval = list
        .as_array()
        .and_then(|list| list.iter().find(|a| a["approvalId"] == id))
        .ok_or("No approval with that ID is waiting. It may have been answered already.")?;
    let offered = approval["params"]["availableDecisions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let preferred: &[&str] = if allow {
        &["accept", "approved", "allow"]
    } else {
        &["decline", "denied", "deny", "cancel"]
    };
    let decision = preferred
        .iter()
        .find(|d| offered.iter().any(|o| o == **d))
        .unwrap_or(&preferred[0]);
    let issued_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    let answered = local
        .call(
            "approvals.resolve",
            json!({"approvalId": id, "decision": decision,
                   "actionNonce": approval["actionNonce"], "expectedState": "pending",
                   "idempotencyKey": uuid::Uuid::new_v4().to_string(),
                   "issuedAtMs": issued_at_ms, "signature": ""}),
        )
        .await;
    match answered {
        Err(error) if api_status(error.as_ref()) == Some(401) => Err(Box::new(Unavailable(
            "Approvals are answered on a paired iPhone: the host accepts a decision only \
             with the signature of a paired device, which this computer's owner does not \
             have. Answer it in Cupola on your iPhone."
                .into(),
        ))),
        Err(error) => Err(error),
        Ok(_) => {
            let state = if allow { "approved" } else { "denied" };
            if as_json {
                print_json(&cli::ChatActionOutput {
                    id: id.to_owned(),
                    state,
                });
            } else {
                println!("Answered: {state}.");
            }
            Ok(())
        }
    }
}

/// `wonder rpc`: Wonder RPC on standard input and output, one JSON-RPC
/// message per line, as the local owner. It relays lines to the daemon's
/// `/api/v1/rpc` socket with the loopback capability, so stdio clients get
/// exactly the WebSocket protocol. At end of input it waits for the answers
/// still owed, then exits.
async fn rpc_bridge() -> Result<()> {
    let local = Local::connect()?;
    relay(
        &local,
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}

async fn relay(
    local: &Local,
    input: impl tokio::io::AsyncBufRead + Unpin,
    mut output: impl tokio::io::AsyncWrite + Unpin,
) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::AsyncWriteExt;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let mut request = local.rpc_url().into_client_request()?;
    request
        .headers_mut()
        .insert("x-wonder-loopback-capability", local.capability.parse()?);
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|_| not_running())?;
    let (mut sink, mut stream) = socket.split();
    let mut input = BoundedLines::new(input, wonderd::rpc::MAX_REQUEST_BYTES);
    let mut input_open = true;
    let mut owed = Owed::default();
    loop {
        if !input_open && owed.is_empty() {
            break;
        }
        tokio::select! {
            line = input.next_line(), if input_open => match line? {
                Some(InputLine::Text(line)) if line.trim().is_empty() => {}
                Some(InputLine::Text(line)) => {
                    owed.sent(&line);
                    sink.send(Message::Text(line.into())).await?;
                }
                // The host would close the whole session on a frame this
                // large, so the relay answers it as the host answers one
                // just over the limit, and keeps the session.
                Some(InputLine::TooLarge) => {
                    let error = wonderd::rpc::too_large();
                    let answer = json!({"jsonrpc": "2.0", "id": null, "error": error});
                    output.write_all(answer.to_string().as_bytes()).await?;
                    output.write_all(b"\n").await?;
                    output.flush().await?;
                }
                None => input_open = false,
            },
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    owed.answered(text.as_str());
                    output.write_all(text.as_bytes()).await?;
                    output.write_all(b"\n").await?;
                    output.flush().await?;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    return Err("The connection to the Cupola host closed.".into())
                }
            },
        }
    }
    let _ = sink.close().await;
    Ok(())
}

/// A line of `wonder rpc` input, or the fact that one was over the limit.
#[derive(Debug, PartialEq)]
enum InputLine {
    Text(String),
    TooLarge,
}

/// Reads input lines holding at most `limit` bytes of one: the rest of an
/// oversized line is read and dropped. Cancel-safe: a partial line stays in
/// `line` when `tokio::select!` drops a pending `next_line`.
struct BoundedLines<R> {
    reader: R,
    line: Vec<u8>,
    over: bool,
    limit: usize,
}

impl<R: tokio::io::AsyncBufRead + Unpin> BoundedLines<R> {
    fn new(reader: R, limit: usize) -> Self {
        Self {
            reader,
            line: Vec::new(),
            over: false,
            limit,
        }
    }

    async fn next_line(&mut self) -> std::io::Result<Option<InputLine>> {
        use tokio::io::AsyncBufReadExt;
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if self.line.is_empty() && !self.over {
                    return Ok(None);
                }
                return self.take().map(Some);
            }
            let end = available.iter().position(|&byte| byte == b'\n');
            let part = &available[..end.unwrap_or(available.len())];
            // One byte over for the `\r` of a `\r\n` ending.
            if !self.over && self.line.len() + part.len() > self.limit + 1 {
                self.over = true;
                self.line = Vec::new();
            }
            if !self.over {
                self.line.extend_from_slice(part);
            }
            let used = part.len() + usize::from(end.is_some());
            self.reader.consume(used);
            if end.is_some() {
                return self.take().map(Some);
            }
        }
    }

    fn take(&mut self) -> std::io::Result<InputLine> {
        let mut line = std::mem::take(&mut self.line);
        if std::mem::take(&mut self.over) {
            return Ok(InputLine::TooLarge);
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.len() > self.limit {
            return Ok(InputLine::TooLarge);
        }
        String::from_utf8(line).map(InputLine::Text).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )
        })
    }
}

/// The answers `wonder rpc` still owes its client, counted per request ID.
/// Frames the host cannot read are answered under a `null` ID, so they are
/// owed under `null` and never retire a readable request's answer.
#[derive(Default)]
struct Owed(std::collections::HashMap<String, usize>);

impl Owed {
    fn key(id: Option<wonderd::rpc::RequestId>) -> String {
        serde_json::to_string(&id).unwrap_or_default()
    }

    /// Counts a line sent to the host, as the host itself reads it.
    fn sent(&mut self, line: &str) {
        let id = match wonderd::rpc::read_frame(line) {
            wonderd::rpc::Incoming::Call(id, _) => Some(id),
            wonderd::rpc::Incoming::Invalid(id, _) => id,
            wonderd::rpc::Incoming::Notification => return,
        };
        *self.0.entry(Self::key(id)).or_default() += 1;
    }

    /// Retires the answer a host frame carries; notifications carry none.
    fn answered(&mut self, frame: &str) {
        let Ok(value) = serde_json::from_str::<Value>(frame) else {
            return;
        };
        if value.get("method").is_some() {
            return;
        }
        let id = value
            .get("id")
            .and_then(|id| serde_json::from_value(id.clone()).ok());
        let key = Self::key(id);
        if let Some(count) = self.0.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                self.0.remove(&key);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

async fn status(as_json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    let supervisor = running_supervisor(&paths);
    let local = Local::connect()?;
    let host = local.call("host.status", Value::Null).await;
    let providers = local.call("host.providers", Value::Null).await.ok();
    let devices = local.call("devices.list", Value::Null).await.ok();
    if as_json {
        print_json(&StatusOutput {
            supervisor_pid: supervisor,
            host: host.ok(),
            providers,
            devices,
        });
        return Ok(());
    }
    match (supervisor, &host) {
        (Some(pid), Ok(_)) => println!("Running (supervisor pid {pid})"),
        (Some(pid), Err(_)) => {
            println!("Starting (supervisor pid {pid}); daemon not answering yet")
        }
        (None, Ok(_)) => println!("Daemon answering, but not supervised by cupola"),
        (None, Err(_)) => {
            println!("Not running. Start with `cupola start`.");
            return Ok(());
        }
    }
    if let Ok(host) = host {
        write_host_status(&mut std::io::stdout(), &host, providers.as_ref())?;
    }
    if let Some(Value::Array(list)) = providers.as_ref().map(|p| &p["providers"]) {
        for provider in list {
            println!(
                "Provider: {:<6} {}",
                provider["id"].as_str().unwrap_or("?"),
                if provider["running"].as_bool() == Some(true) {
                    "running".to_owned()
                } else {
                    provider["detail"]
                        .as_str()
                        .unwrap_or("not running")
                        .to_owned()
                }
            );
        }
    }
    if let Some(Value::Array(list)) = devices.as_ref() {
        let active = list.iter().filter(|d| d["revokedAt"].is_null()).count();
        println!("Phones:   {active} paired");
    }
    Ok(())
}

fn write_host_status(
    output: &mut impl Write,
    host: &Value,
    providers: Option<&Value>,
) -> std::io::Result<()> {
    // Runtime absence does not establish an authentication failure or a
    // reconnect: a newly initialized computer has never connected an agent.
    // Keep failures/recovery and unknown provider reports unchanged.
    let waiting_for_agent = host["execution"]["reason"] == "runtime_unavailable"
        && providers
            .and_then(|value| value["providers"].as_array())
            .is_some_and(|list| {
                !list.is_empty()
                    && list.iter().all(|provider| {
                        provider["running"] == false
                            && provider["starting"] == false
                            && provider["incompatible"] == false
                    })
            });
    writeln!(
        output,
        "Name:     {}",
        host["hostName"].as_str().unwrap_or("?")
    )?;
    writeln!(
        output,
        "State:    {}",
        if waiting_for_agent {
            "waiting for agent"
        } else {
            host["state"].as_str().unwrap_or("?")
        }
    )?;
    writeln!(
        output,
        "Origin:   {}",
        host["publicOrigin"]
            .as_str()
            .unwrap_or("(tunnel not ready)")
    )?;
    if waiting_for_agent {
        writeln!(output, "Agents:   No agent is connected yet. Run `cupola providers` for installation and sign-in steps, or `cupola doctor` to check this computer.")?;
    } else if let Some(detail) = host["execution"]["detail"].as_str() {
        writeln!(output, "Agents:   {detail}")?;
    }
    Ok(())
}

fn log_path(paths: &Paths) -> PathBuf {
    paths.logs().join("wonder.log")
}

/// The last `count` lines of `text`, oldest first.
fn last_lines(text: &str, count: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(count)..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

fn logs(count: usize, follow: bool, as_json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    let path = log_path(&paths);
    if follow && as_json {
        return Err("`cupola logs --follow` prints text only; drop --json or --follow.".into());
    }
    let (lines, mut offset) = match log_tail(&path, count) {
        Ok(tail) => tail,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "No host log yet at {}. Start the host with `cupola start`; \
                 `cupola start --foreground` prints its log in the terminal instead.",
                path.display()
            )
            .into())
        }
        Err(error) => return Err(format!("{}: {error}", path.display()).into()),
    };
    if as_json {
        print_json(&LogsOutput {
            path: path.display().to_string(),
            lines,
        });
        return Ok(());
    }
    for line in &lines {
        println!("{line}");
    }
    if !follow {
        return Ok(());
    }
    // Poll rather than watch: one small file, and it survives truncation.
    let mut carry = Utf8Carry::default();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        follow_log(&path, &mut offset, &mut carry, &mut std::io::stdout())?;
    }
}

/// The most `logs` reads to find its last lines: the host log is never
/// rotated, so it can be far larger than what the command shows.
const LOG_TAIL_BYTES: u64 = 4 << 20;

/// The last `count` lines of the log, read from its end (at most
/// [`LOG_TAIL_BYTES`]), and the byte offset that reading stopped at.
fn log_tail(path: &Path, count: usize) -> std::io::Result<(Vec<String>, u64)> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let end = file.metadata()?.len();
    let mut start = end;
    let mut tail = Vec::new();
    while start > 0
        && end - start < LOG_TAIL_BYTES
        && tail.iter().filter(|&&byte| byte == b'\n').count() <= count
    {
        let step = (64 * 1024).min(start).min(LOG_TAIL_BYTES - (end - start));
        start -= step;
        file.seek(SeekFrom::Start(start))?;
        let mut chunk = vec![0; step as usize];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&tail);
        tail = chunk;
    }
    // A read that starts mid-file drops the line it cut.
    let whole = if start > 0 {
        tail.iter()
            .position(|&byte| byte == b'\n')
            .map_or(&[][..], |at| &tail[at + 1..])
    } else {
        &tail[..]
    };
    Ok((last_lines(&String::from_utf8_lossy(whole), count), end))
}

/// Prints what was appended to the log since `offset` (a raw byte offset),
/// starting again from the top only when the file got shorter.
fn follow_log(
    path: &Path,
    offset: &mut u64,
    carry: &mut Utf8Carry,
    output: &mut impl Write,
) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Ok(());
    };
    let len = file.metadata()?.len();
    if len < *offset {
        *offset = 0;
        *carry = Utf8Carry::default();
    }
    if len == *offset {
        return Ok(());
    }
    file.seek(SeekFrom::Start(*offset))?;
    let mut chunk = Vec::new();
    // Bounded per poll; the rest arrives on the next one.
    (&mut file)
        .take((len - *offset).min(LOG_TAIL_BYTES))
        .read_to_end(&mut chunk)?;
    *offset += chunk.len() as u64;
    write!(output, "{}", carry.decode(&chunk))?;
    output.flush()?;
    Ok(())
}

/// Decodes UTF-8 across reads: a character split between two reads waits
/// for its last byte; a byte that can never be text prints as U+FFFD.
#[derive(Default)]
struct Utf8Carry(Vec<u8>);

impl Utf8Carry {
    fn decode(&mut self, chunk: &[u8]) -> String {
        self.0.extend_from_slice(chunk);
        let mut text = String::new();
        let mut rest = &self.0[..];
        while let Err(error) = std::str::from_utf8(rest) {
            let (valid, after) = rest.split_at(error.valid_up_to());
            text.push_str(std::str::from_utf8(valid).unwrap_or_default());
            match error.error_len() {
                Some(bad) => {
                    text.push(char::REPLACEMENT_CHARACTER);
                    rest = &after[bad..];
                }
                None => {
                    // An incomplete character at the end: keep it.
                    let kept = after.len();
                    let used = self.0.len() - kept;
                    self.0.drain(..used);
                    return text;
                }
            }
        }
        text.push_str(std::str::from_utf8(rest).unwrap_or_default());
        self.0.clear();
        text
    }
}

async fn device_action(command: DevicesCommand, as_json: bool) -> Result<()> {
    let output = change_device(&Local::connect()?, command).await?;
    if as_json {
        print_json(&output);
        return Ok(());
    }
    println!("{}", device_action_text(&output));
    Ok(())
}

fn device_action_text(output: &DeviceActionOutput) -> String {
    let id = &output.device_id;
    match output.state {
        DeviceState::Revoked => format!(
            "Disconnected {id}. It can no longer reach this computer; \
             `cupola devices forget {id}` removes it from the list."
        ),
        DeviceState::Forgotten => format!("Removed {id} from the list."),
        DeviceState::Renamed => format!(
            "Renamed {id} to \"{}\".",
            output.label.as_deref().unwrap_or_default()
        ),
    }
}

async fn change_device(local: &Local, command: DevicesCommand) -> Result<DeviceActionOutput> {
    let (id, state, params) = match command {
        DevicesCommand::Revoke { id } => {
            let params = json!({ "deviceId": id });
            (id, DeviceState::Revoked, params)
        }
        DevicesCommand::Forget { id } => {
            let params = json!({ "deviceId": id });
            (id, DeviceState::Forgotten, params)
        }
        DevicesCommand::Rename { id, name } => {
            let params = json!({ "deviceId": id, "label": name.join(" ") });
            (id, DeviceState::Renamed, params)
        }
    };
    let method = match state {
        DeviceState::Revoked => "devices.revoke",
        DeviceState::Forgotten => "devices.forget",
        DeviceState::Renamed => "devices.rename",
    };
    let reply = match local.call(method, params).await {
        Ok(reply) => reply,
        Err(error) if not_found(error.as_ref()) => {
            // Revoke and rename reach only connected phones; a disconnected
            // one is still listed, so say which it is (and never reconnect it).
            let disconnected = state != DeviceState::Forgotten
                && local
                    .call("devices.list", Value::Null)
                    .await
                    .is_ok_and(|devices| {
                        devices.as_array().into_iter().flatten().any(|d| {
                            d["id"].as_str() == Some(id.as_str()) && !d["revokedAt"].is_null()
                        })
                    });
            return Err(match state {
                _ if disconnected => format!(
                    "{id} is already disconnected. `cupola devices forget {id}` removes it \
                     from the list."
                ),
                DeviceState::Revoked | DeviceState::Renamed => {
                    format!("No phone with ID {id}. Run `cupola devices` to see the IDs.")
                }
                DeviceState::Forgotten => format!(
                    "No disconnected phone with ID {id}. Disconnect it first with \
                     `cupola devices revoke {id}`, or run `cupola devices` to see the IDs."
                ),
            }
            .into());
        }
        Err(error) => return Err(error),
    };
    Ok(DeviceActionOutput {
        device_id: id,
        state,
        label: reply["label"].as_str().map(str::to_owned),
    })
}

/// This computer's own sign-in for `provider`, which the provider runs
/// itself (Codex prints a code to enter in any browser).
fn own_sign_in_command(provider: Provider) -> Result<Command> {
    own_sign_in_command_for(provider, executable_path().ok().as_deref())
}

/// Claude Code's sign-in script and the Node that runs it, as `cupola
/// doctor` finds them (the copies bundled beside `exe` included).
fn claude_manage(exe: Option<&Path>) -> Option<(PathBuf, PathBuf)> {
    let manage = runtime_bridge_for("WONDER_CLAUDE_BRIDGE", "claude-runtime", exe)
        .map(|bridge| bridge.with_file_name("manage.mjs"))
        .filter(|manage| manage.is_file())?;
    Some((node_binary_for(exe)?, manage))
}

fn own_sign_in_command_for(provider: Provider, exe: Option<&Path>) -> Result<Command> {
    let missing = || -> Box<dyn std::error::Error> {
        format!(
            "{} is not installed on this computer. `cupola providers` says how to install it.",
            provider.name()
        )
        .into()
    };
    Ok(match provider {
        Provider::Codex => {
            let codex = std::env::var_os("WONDER_CODEX_BIN")
                .map(PathBuf::from)
                .or_else(codex_binary)
                .ok_or_else(missing)?;
            let mut command = Command::new(codex);
            command.args(["login", "--device-auth"]);
            command
        }
        Provider::Claude => {
            let (node, manage) = claude_manage(exe).ok_or_else(missing)?;
            let mut command = Command::new(node);
            command.arg(manage).arg("login");
            command
        }
        Provider::Opencode => {
            let binary = opencode_binary().ok_or_else(missing)?;
            // Windows cannot run npm's Node script by its shebang (catalog.mjs).
            let mut command = if cfg!(windows)
                && !binary
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
            {
                let mut command = Command::new(node_binary_for(exe).ok_or_else(missing)?);
                command.arg(binary);
                command
            } else {
                Command::new(binary)
            };
            command.args(["auth", "login"]);
            command
        }
        Provider::Cursor => {
            let mut command = Command::new(cursor_binary().ok_or_else(missing)?);
            command.arg("login");
            command
        }
    })
}

/// Runs a provider's own sign-in attached to this terminal. With `--json`
/// its output goes to stderr so stdout stays one JSON value.
fn run_in_this_terminal(command: &mut Command, name: &str, as_json: bool) -> Result<()> {
    if as_json {
        command.stdout(std::io::stderr());
    }
    let status = command
        .status()
        .map_err(|error| format!("{name}'s sign-in could not start: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{name}'s sign-in did not finish. Run the command again to retry.").into())
    }
}

async fn provider_login(command: ProvidersCommand, as_json: bool) -> Result<()> {
    let ProvidersCommand::Login { provider } = command;
    let mut command = own_sign_in_command(provider)?;
    run_in_this_terminal(&mut command, provider.name(), as_json)?;
    if let Ok(local) = Local::connect() {
        let _ = local
            .send(
                "providers.reconnect",
                json!({ "provider": provider.as_str() }),
                PROVIDER_TIMEOUT,
            )
            .await;
    }
    if as_json {
        print_json(&ProviderLoginOutput {
            provider,
            login: json!({"state":"succeeded", "method":"terminal"}),
        });
    } else {
        println!("Signed in to {} on this computer.", provider.name());
    }
    Ok(())
}

async fn config(command: ConfigCommand, as_json: bool) -> Result<()> {
    let local = Local::connect()?;
    let output = match command {
        ConfigCommand::Get {
            key: ConfigKey::HostName,
        } => host_name_output(&local.call("host.name", Value::Null).await?),
        ConfigCommand::Set {
            key: ConfigKey::HostName,
            value,
        } => host_name_output(&local.call("host.rename", json!({ "name": value })).await?),
        ConfigCommand::Get {
            key: ConfigKey::VoiceKey,
        } => voice_key_output(&local.call("voice.status", Value::Null).await?),
        // The daemon checks a key with OpenAI before storing it in the
        // Keychain (macOS) or a file only this user can read (Linux).
        ConfigCommand::Set {
            key: ConfigKey::VoiceKey,
            value: Some(_),
        } => {
            return Err(
                "Pass the voice key on standard input, not as an argument, so it \
                        stays out of your shell history: `cupola config set voice-key`."
                    .into(),
            )
        }
        ConfigCommand::Set {
            key: ConfigKey::VoiceKey,
            value: None,
        } => {
            let key = read_secret("OpenAI API key: ")?;
            if key.is_empty() {
                return Err("No key given; nothing changed.".into());
            }
            // Outlast the daemon's own check with OpenAI (15 s), so its
            // answer (rejected, unreachable) is what the person sees.
            voice_key_output(
                &local
                    .send(
                        "voice.setKey",
                        json!({ "apiKey": key }),
                        Duration::from_secs(30),
                    )
                    .await?,
            )
        }
    };
    if as_json {
        print_json(&output);
        return Ok(());
    }
    match (output.key, &output.value) {
        (ConfigKey::HostName, Some(name)) if output.configured => println!("{name}"),
        (ConfigKey::HostName, Some(name)) => println!("{name} (this computer's own name)"),
        (ConfigKey::HostName, None) => println!("(no name)"),
        (ConfigKey::VoiceKey, _) if output.configured => println!("Voice key saved."),
        (ConfigKey::VoiceKey, _) => {
            println!("No voice key. Set one with `cupola config set voice-key` to use voice mode.")
        }
    }
    Ok(())
}

fn host_name_output(reply: &Value) -> ConfigOutput {
    ConfigOutput {
        key: ConfigKey::HostName,
        value: reply["name"].as_str().map(str::to_owned),
        configured: reply["custom"].as_bool().unwrap_or(false),
    }
}

fn voice_key_output(reply: &Value) -> ConfigOutput {
    ConfigOutput {
        key: ConfigKey::VoiceKey,
        value: None,
        configured: reply["configured"].as_bool().unwrap_or(false),
    }
}

/// One line from stdin. When a person is typing, the prompt goes to stderr
/// and the terminal does not echo what they type.
fn read_secret(prompt: &str) -> Result<String> {
    let echo = os::StdinEcho::disable();
    if echo.is_some() {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
    }
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if echo.is_some() {
        drop(echo);
        eprintln!();
    }
    read?;
    Ok(line.trim().to_owned())
}

/// `cupola media prune`: what the preview folder holds that no conversation
/// names any more; deleted only with `--apply`.
/// `cupola voice words list|add|remove`: the dictionary voice mode sends to
/// the transcriber when a session starts.
async fn voice_words(command: VoiceWordsCommand, as_json: bool) -> Result<()> {
    let local = Local::connect()?;
    let (verb, reply) = match command {
        VoiceWordsCommand::List => ("", local.call("voice.dictionary", Value::Null).await?),
        VoiceWordsCommand::Add { words } => (
            "Added",
            local
                .call("voice.updateDictionary", json!({ "add": words }))
                .await?,
        ),
        VoiceWordsCommand::Remove { words } => (
            "Removed",
            local
                .call("voice.updateDictionary", json!({ "remove": words }))
                .await?,
        ),
    };
    if as_json {
        print_json(&reply);
        return Ok(());
    }
    let words: Vec<&str> = reply["words"]
        .as_array()
        .map(|words| words.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if words.is_empty() {
        println!("No voice words. Add some with `cupola voice words add T3 Cupola`.");
    } else {
        if !verb.is_empty() {
            println!("{verb}. Voice words now (used from the next voice session):");
        }
        for word in words {
            println!("{word}");
        }
    }
    Ok(())
}

async fn media_prune(older_than_days: u32, apply: bool, as_json: bool) -> Result<()> {
    let local = Local::connect()?;
    let method = if apply {
        "media.prune"
    } else {
        "media.orphans"
    };
    let report = local
        .call(method, json!({ "olderThanDays": older_than_days }))
        .await?;
    if as_json {
        print_json(&report);
        return Ok(());
    }
    let count = |key: &str| report[key].as_u64().unwrap_or(0);
    let megabytes = |key: &str| count(key) as f64 / 1_000_000.0;
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "Previews still in use: {} files, {:.1} MB",
        count("referencedFiles"),
        megabytes("referencedBytes")
    )?;
    writeln!(
        out,
        "Unused, changed in the last {older_than_days} days (kept): {} files, {:.1} MB",
        count("recentFiles"),
        megabytes("recentBytes")
    )?;
    if apply {
        writeln!(
            out,
            "Deleted: {} files, {:.1} MB",
            count("deletedFiles"),
            megabytes("deletedBytes")
        )?;
    } else {
        writeln!(
            out,
            "Unused for {older_than_days}+ days: {} files, {:.1} MB",
            count("orphanedFiles"),
            megabytes("orphanedBytes")
        )?;
        if count("orphanedFiles") > 0 {
            writeln!(
                out,
                "Nothing was deleted. Run `cupola media prune --apply` to delete them."
            )?;
        }
    }
    Ok(())
}

async fn devices(as_json: bool) -> Result<()> {
    let local = Local::connect()?;
    let devices = local.call("devices.list", Value::Null).await?;
    let Value::Array(list) = devices else {
        return Err("unexpected device list".into());
    };
    if as_json {
        print_json(&DevicesOutput { devices: list });
        return Ok(());
    }
    write_devices(&mut std::io::stdout().lock(), &list, now_ms())?;
    Ok(())
}

/// One phone per line: its ID (what `revoke`, `forget` and `rename` take),
/// its name and when it last reached this computer.
fn write_devices(out: &mut impl Write, list: &[Value], now_ms: u64) -> std::io::Result<()> {
    if list.is_empty() {
        return writeln!(out, "No paired phones. Run `cupola pair`.");
    }
    let width = list
        .iter()
        .map(|device| device["label"].as_str().unwrap_or("?").chars().count())
        .max()
        .unwrap_or(0);
    for device in list {
        let seen = device["lastSeenAt"]
            .as_str()
            .map_or_else(|| "never".to_owned(), |at| relative_time(at, now_ms));
        writeln!(
            out,
            "{}  {:<width$}  last seen {seen}{}",
            device["id"].as_str().unwrap_or("?"),
            device["label"].as_str().unwrap_or("?"),
            if device["revokedAt"].is_null() {
                ""
            } else {
                "  (disconnected)"
            }
        )?;
    }
    writeln!(
        out,
        "Rename one with `cupola devices rename <id> <name>`; disconnect one with \
         `cupola devices revoke <id>`."
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// A host timestamp (milliseconds since 1970, or RFC 3339) as people say it.
fn relative_time(at: &str, now_ms: u64) -> String {
    let then_ms = at.parse::<u64>().ok().or_else(|| {
        chrono::DateTime::parse_from_rfc3339(at)
            .ok()
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
    });
    let Some(then_ms) = then_ms else {
        return at.to_owned();
    };
    let minutes = now_ms.saturating_sub(then_ms) / 60_000;
    let plural =
        |count: u64, unit: &str| format!("{count} {unit}{} ago", if count == 1 { "" } else { "s" });
    match minutes {
        0 => "just now".into(),
        1..=59 => plural(minutes, "minute"),
        60..=1439 => plural(minutes / 60, "hour"),
        1440..=2879 => "yesterday".into(),
        2880..=43199 => plural(minutes / 1440, "day"),
        _ => chrono::DateTime::from_timestamp_millis(then_ms as i64)
            .map(|time| format!("on {}", time.format("%Y-%m-%d")))
            .unwrap_or_else(|| at.to_owned()),
    }
}

/// Names the newest Codex release this build is verified with, so a bare
/// machine gets a command it can run rather than a manifest to look up.
fn codex_install_hint() -> String {
    let manifest: Value =
        serde_json::from_str(include_str!("../../../../compatibility-manifest.json"))
            .unwrap_or_default();
    let version = manifest["codex"]["testedRange"]["newest"]
        .as_str()
        .and_then(|version| version.strip_prefix("codex-cli "))
        .unwrap_or("latest");
    // A distribution's Node owns /usr/lib/node_modules; ~/.local/bin is on
    // a Linux login PATH, and the service copies that PATH.
    let prefix = if cfg!(all(unix, not(target_os = "macos"))) {
        " --prefix ~/.local"
    } else {
        ""
    };
    format!("Not installed. With Node.js installed, run `npm install -g{prefix} @openai/codex@{version}`.")
}

struct LocalRuntimes {
    codex: Option<(PathBuf, Option<String>)>,
    claude_bridge: Option<PathBuf>,
    node: Option<PathBuf>,
    opencode_bridge: Option<PathBuf>,
    opencode_cli: bool,
    cursor_cli: Option<PathBuf>,
}

impl LocalRuntimes {
    fn discover() -> Self {
        let codex = std::env::var_os("WONDER_CODEX_BIN")
            .map(PathBuf::from)
            .or_else(codex_binary)
            .map(|path| {
                let version = process::output(Command::new(&path).arg("--version"))
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
                (path, version)
            });
        Self {
            codex,
            claude_bridge: runtime_bridge("WONDER_CLAUDE_BRIDGE", "claude-runtime"),
            node: node_binary(),
            opencode_bridge: runtime_bridge("WONDER_OPENCODE_BRIDGE", "acp-runtime"),
            opencode_cli: opencode_binary().is_some(),
            cursor_cli: cursor_binary(),
        }
    }

    fn codex(&self) -> LocalRuntime {
        match &self.codex {
            Some((path, version)) => LocalRuntime {
                installed: true,
                path: Some(path.display().to_string()),
                version: version.clone(),
                detail: "Sign in on this machine with `cupola providers login codex` (shows a code to enter in any browser).".into(),
            },
            None => LocalRuntime {
                installed: false,
                path: None,
                version: None,
                detail: codex_install_hint(),
            },
        }
    }

    fn claude(&self) -> LocalRuntime {
        let detail = match (&self.claude_bridge, &self.node) {
            // The bridge path is versioned and pruned by `wonder update`.
            (Some(_), Some(_)) => {
                "Sign in to your Claude subscription with `cupola providers login claude`.".into()
            }
            (None, _) => "Runtime bridge not installed (share/wonder/claude-runtime).".into(),
            (_, None) => "Node.js not found on PATH.".into(),
        };
        LocalRuntime {
            installed: self.claude_bridge.is_some() && self.node.is_some(),
            path: self.claude_bridge.as_ref().map(|p| p.display().to_string()),
            version: None,
            detail,
        }
    }

    fn opencode(&self) -> LocalRuntime {
        LocalRuntime {
            installed: self.opencode_usable(),
            path: self
                .opencode_bridge
                .as_ref()
                .map(|p| p.display().to_string()),
            version: None,
            detail: self.opencode_detail().into(),
        }
    }

    /// The bridge runs on Node and drives the `opencode` CLI.
    fn opencode_usable(&self) -> bool {
        self.opencode_bridge.is_some() && self.node.is_some() && self.opencode_cli
    }

    fn opencode_detail(&self) -> &'static str {
        match (&self.opencode_bridge, &self.node, self.opencode_cli) {
            (None, _, _) => "Runtime missing (share/wonder/acp-runtime).",
            (_, None, _) => "Node.js not found on PATH.",
            (_, _, false) => "Not installed. Install the OpenCode CLI (https://opencode.ai/docs/).",
            _ => OPENCODE_PROVIDERS,
        }
    }

    /// The Cursor bridge is the OpenCode one (acp-runtime), on Node.
    fn cursor(&self) -> LocalRuntime {
        let detail = match (&self.opencode_bridge, &self.node, &self.cursor_cli) {
            (None, _, _) => "Runtime missing (share/wonder/acp-runtime).".into(),
            (_, None, _) => "Node.js not found on PATH.".into(),
            (_, _, None) => cursor_install_hint(),
            _ => CURSOR_SIGN_IN.into(),
        };
        LocalRuntime {
            installed: self.opencode_bridge.is_some()
                && self.node.is_some()
                && self.cursor_cli.is_some(),
            path: self.cursor_cli.as_ref().map(|p| p.display().to_string()),
            version: None,
            detail,
        }
    }
}

/// OpenCode has no Cupola sign-in: it uses the providers configured in
/// OpenCode itself, and its free models need none.
const OPENCODE_PROVIDERS: &str =
    "Uses OpenCode's own providers; add one with `opencode auth login` if you need more models.";

const CURSOR_SIGN_IN: &str =
    "Sign in to Cursor with `cupola providers login cursor` (runs the Cursor CLI's `agent login`).";

fn cursor_install_hint() -> String {
    if cfg!(windows) {
        "Not installed. Install the Cursor CLI (https://cursor.com/cli), then run `agent login`."
            .into()
    } else {
        "Not installed. Install the Cursor CLI with `curl https://cursor.com/install -fsS | bash`, then run `agent login`.".into()
    }
}

async fn providers(as_json: bool) -> Result<()> {
    let local = Local::connect().ok();
    let mut daemon = None;
    let mut signed_in = None;
    if let Some(local) = &local {
        if let Ok(report) = local.call("host.providers", Value::Null).await {
            daemon = Some(report["providers"].clone());
            // The running host knows each provider's sign-in, and lists
            // every provider it offers (Cursor included).
            signed_in = local
                .call("providers.list", json!({}))
                .await
                .ok()
                .and_then(|list| list["providers"].as_array().cloned());
        }
    }
    let runtimes = LocalRuntimes::discover();
    if as_json {
        print_json(&ProvidersOutput {
            daemon,
            providers: signed_in,
            codex: runtimes.codex(),
            claude: runtimes.claude(),
            opencode: runtimes.opencode(),
            cursor: runtimes.cursor(),
        });
        return Ok(());
    }
    let mut out = std::io::stdout().lock();
    match &signed_in {
        Some(list) => write_providers(&mut out, list)?,
        None => write_local_providers(&mut out, &runtimes, daemon.is_some())?,
    }
    Ok(())
}

/// `cupola providers` from the running host: one line per provider with
/// its sign-in, then what the host says about it.
fn write_providers(out: &mut impl Write, list: &[Value]) -> std::io::Result<()> {
    if list.is_empty() {
        return writeln!(out, "This host offers no agent providers.");
    }
    let names: Vec<(&str, &str)> = list
        .iter()
        .map(|entry| {
            let id = entry["id"].as_str().unwrap_or("?");
            (id, provider_display_name(id))
        })
        .collect();
    let width = names.iter().map(|(_, name)| name.len()).max().unwrap_or(0);
    for (entry, (id, name)) in list.iter().zip(names) {
        let account = entry["account"].as_str().filter(|a| !a.is_empty());
        let version = entry["version"].as_str().filter(|v| !v.is_empty());
        let state = match entry["state"].as_str().unwrap_or_default() {
            "signedIn" => match account {
                Some(account) => format!("Signed in as {account}"),
                None => "Signed in".into(),
            },
            "signedOut" => format!("Signed out. {}", provider_sign_in_hint(id)),
            "needsSubscription" => format!(
                "Signed in{}, but the account's plan does not include {name}",
                account.map(|a| format!(" as {a}")).unwrap_or_default()
            ),
            "unsupported" => {
                "This version is not supported yet. Update Cupola on this computer.".into()
            }
            "notInstalled" => provider_install_hint(id),
            "installed" => "Installed".into(),
            _ => "Not available right now. Run `cupola doctor`.".into(),
        };
        let mut line = state;
        if let Some(version) = version {
            line.push_str(&format!(" ({version})"));
        }
        if entry["working"].as_bool() == Some(true) {
            line.push_str(", working now");
        }
        writeln!(out, "{name:<width$}  {line}")?;
        if let Some(detail) = entry["detail"].as_str().filter(|d| !d.is_empty()) {
            writeln!(out, "{:width$}  {detail}", "")?;
        }
    }
    Ok(())
}

fn provider_display_name(id: &str) -> &str {
    match id {
        "codex" => "Codex",
        "claude" => "Claude Code",
        "opencode" => "OpenCode",
        "cursor" => "Cursor",
        other => other,
    }
}

fn provider_sign_in_hint(id: &str) -> String {
    match id {
        "codex" => {
            "Sign in with `cupola providers login codex` (shows a code to enter in any browser)."
                .into()
        }
        "claude" => {
            "Sign in to your Claude subscription with `cupola providers login claude`.".into()
        }
        "opencode" => OPENCODE_PROVIDERS.into(),
        "cursor" => CURSOR_SIGN_IN.into(),
        _ => format!(
            "Sign in with {} on this computer.",
            provider_display_name(id)
        ),
    }
}

fn provider_install_hint(id: &str) -> String {
    match id {
        "codex" => codex_install_hint(),
        "opencode" => "Not installed. Install the OpenCode CLI (https://opencode.ai/docs/).".into(),
        "cursor" => cursor_install_hint(),
        _ => "Not installed on this computer.".into(),
    }
}

/// `cupola providers` without a running host: what is installed here. Sign-in
/// is the host's to report, so this says how to see it.
fn write_local_providers(
    out: &mut impl Write,
    runtimes: &LocalRuntimes,
    host_answered: bool,
) -> std::io::Result<()> {
    if host_answered {
        writeln!(
            out,
            "The host did not report sign-in; showing what is installed here."
        )?;
    } else {
        writeln!(
            out,
            "Cupola host is not running, so sign-in is not shown. Start it with \
             `cupola start` to see it; installed here:"
        )?;
    }
    match &runtimes.codex {
        Some((path, version)) => writeln!(
            out,
            "Codex:    {} ({})",
            path.display(),
            version.as_deref().unwrap_or("version unknown")
        )?,
        None => writeln!(out, "Codex:    {}", codex_install_hint())?,
    }
    match (&runtimes.claude_bridge, &runtimes.node) {
        (Some(bridge), Some(_)) => writeln!(out, "Claude:   runtime at {}", bridge.display())?,
        (None, _) => writeln!(
            out,
            "Claude:   runtime bridge not installed (share/wonder/claude-runtime)."
        )?,
        (_, None) => writeln!(out, "Claude:   Node.js not found on PATH.")?,
    }
    writeln!(out, "OpenCode: {}", runtimes.opencode_detail())?;
    writeln!(out, "Cursor:   {}", runtimes.cursor().detail)
}

// ---------------------------------------------------------------- doctor

// The JSON fix prefix is the agent's boundary between a command it can run
// and a sign-in/admin approval it must hand back to the person.
fn tailscale_doctor_check(
    identity: Result<ServeIdentity>,
    permission: impl FnOnce() -> Result<()>,
) -> Check {
    let (summary, fix) = match identity {
        Ok(_) => match permission() {
            Ok(()) => ("Tailscale is signed in".into(), None),
            Err(error) => (
                "Tailscale does not let this account publish Cupola".into(),
                Some(format!("Needs the person: {error}")),
            ),
        },
        Err(error) => match error.downcast_ref::<TailscaleNotReady>() {
            Some(reason) => (
                reason.summary.clone(),
                Some(format!("Needs the person: {}", reason.fix)),
            ),
            None => (
                format!("Tailscale is not ready: {error}"),
                Some("Needs the person: sign in with `tailscale up` if asked, and turn on MagicDNS and HTTPS certificates for the tailnet in the Tailscale admin console (DNS).".into()),
            ),
        },
    };
    Check {
        id: "tailscale",
        status: if fix.is_none() {
            CheckStatus::Ok
        } else {
            CheckStatus::Failed
        },
        summary,
        fix,
    }
}

/// While an iPhone on the same Wi-Fi can reach this computer, Tailscale only
/// adds reaching it away from home: a missing sign-in is worth a warning, not
/// a failed setup. Otherwise Tailscale is the only way in and stays a failure.
fn optional_tailscale(mut check: Check, same_wifi_reachable: bool) -> Check {
    if same_wifi_reachable && check.status == CheckStatus::Failed {
        check.status = CheckStatus::Warning;
        check.summary = format!(
            "{}. iPhones on the same Wi-Fi still connect; Tailscale is only needed away from home",
            check.summary
        );
    }
    check
}

fn write_checks(out: &mut impl Write, checks: &[Check]) -> std::io::Result<()> {
    for check in checks {
        let mark = match check.status {
            CheckStatus::Ok => "ok  ",
            CheckStatus::Warning => "warn",
            CheckStatus::Failed => "FAIL",
            CheckStatus::Unavailable => "n/a ",
        };
        writeln!(out, "{mark}  {}", check.summary)?;
        if let Some(fix) = &check.fix {
            writeln!(out, "      {fix}")?;
        }
    }
    Ok(())
}

/// Agents (Codex and the others) use apps on this computer through Cua
/// Driver. It is optional, so its absence is a warning that says so, and the
/// fix points at its install guide rather than a script piped to a shell.
fn cua_driver_check(driver: Option<&Path>) -> Check {
    Check {
        id: "cua-driver",
        status: if driver.is_some() {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        summary: match driver {
            Some(path) => format!("Cua Driver at {}", path.display()),
            None => "Cua Driver is not installed (optional): agents can't use apps on this \
                     computer"
                .into(),
        },
        fix: driver.is_none().then(|| {
            "Optional. To let agents use apps here, install Cua Driver from its guide \
             (https://cua.ai/docs/cua-driver/quickstart), then run `cupola doctor` again."
                .into()
        }),
    }
}

/// One independent part of `cupola doctor`: the checks it contributes, in
/// order. A step that cannot report at all fails the whole command.
type DoctorStep<'a> = Box<dyn FnOnce() -> std::result::Result<Vec<Check>, String> + Send + 'a>;

/// Runs every step at once and joins their checks in step order, so the
/// output is the same as running them one after another.
fn run_doctor_steps(steps: Vec<DoctorStep>) -> Result<Vec<Check>> {
    let finished: Vec<_> = std::thread::scope(|scope| {
        let running: Vec<_> = steps.into_iter().map(|step| scope.spawn(step)).collect();
        running
            .into_iter()
            .map(|step| {
                step.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    let mut checks = Vec::new();
    for step in finished {
        checks.extend(step?);
    }
    Ok(checks)
}

async fn doctor(as_json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    let mut checks = Vec::new();
    let check = |id, ok: bool, summary: String, fix: Option<String>| Check {
        id,
        status: if ok {
            CheckStatus::Ok
        } else {
            CheckStatus::Failed
        },
        summary,
        fix: if ok { None } else { fix },
    };
    let config = load_config(&paths);
    checks.push(match &config {
        Ok(_) => check(
            "initialized",
            true,
            format!("Set up in {}", paths.data.display()),
            None,
        ),
        Err(error) => check(
            "initialized",
            false,
            error.to_string(),
            Some("Run `cupola init`.".into()),
        ),
    });
    let daemon = std::env::var_os("WONDER_DAEMON_BIN")
        .map(PathBuf::from)
        .or_else(|| sibling("wonderd"));
    checks.push(check(
        "daemon",
        daemon.is_some(),
        match &daemon {
            Some(path) => format!("wonderd at {}", path.display()),
            None => "wonderd was not found next to the cupola command".into(),
        },
        Some("Install wonderd in the same folder as cupola (see INSTALL.md).".into()),
    ));
    // Each probe below is independent of the others (child processes, a TLS
    // handshake, a local call), so they run at once; the checks still come
    // out in the order the steps are listed.
    let config_ok = config.as_ref().ok();
    let mut steps: Vec<DoctorStep> = Vec::new();
    steps.push(Box::new(|| {
        let running = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .is_ok_and(|runtime| {
                runtime.block_on(async {
                    match Local::connect() {
                        Ok(local) => local.call("host.status", Value::Null).await.is_ok(),
                        Err(_) => false,
                    }
                })
            });
        Ok(vec![Check {
            id: "running",
            status: if running {
                CheckStatus::Ok
            } else {
                CheckStatus::Warning
            },
            summary: if running {
                "The host is running".into()
            } else {
                "The host is not running".into()
            },
            fix: (!running).then(|| "Run `cupola start`.".into()),
        }])
    }));
    #[cfg(all(unix, not(target_os = "macos")))]
    steps.push(Box::new(|| {
        Ok(stale_host_restart(&paths)
            .map(|restart| Check {
                id: "release",
                status: CheckStatus::Warning,
                summary: "The host still runs the release from before the last update".into(),
                fix: Some(format!("Run `{restart}` when work finishes.")),
            })
            .into_iter()
            .collect())
    }));
    if let Some(config) = config_ok {
        if config["tunnel"].as_str() == Some("tailscale") {
            steps.push(Box::new(move || {
                #[cfg(all(unix, not(target_os = "macos")))]
                let permission = tailscale_serve_permission;
                #[cfg(not(all(unix, not(target_os = "macos"))))]
                let permission = || -> Result<()> { Ok(()) };
                Ok(vec![optional_tailscale(
                    tailscale_doctor_check(tailscale_identity(), permission),
                    same_wifi_reachable(config, &home_addresses()),
                )])
            }));
        }
        let paths = &paths;
        steps.push(Box::new(move || {
            Ok(vec![home_network_check(
                config,
                published_status(paths).as_ref(),
                &home_listener_probe(paths),
            )])
        }));
        steps.push(Box::new(move || {
            Ok(firewall_check(config).into_iter().collect())
        }));
        #[cfg(target_os = "macos")]
        steps.push(Box::new(move || {
            Ok(vec![computer_control_check(config, control_state(paths))])
        }));
        if config["computer"].as_bool() == Some(true) {
            #[cfg(not(target_os = "macos"))]
            steps.push(Box::new(move || {
                let mut checks = Vec::new();
                let native_unavailable = executable_path()
                    .ok()
                    .and_then(|exe| exe.parent().map(packaged_native_unavailable))
                    .unwrap_or(false);
                if native_unavailable {
                    checks.push(Check {
                        id: "computer-native",
                        status: CheckStatus::Warning,
                        summary: "This Cupola installation includes Python screen sharing; native screen sharing is unavailable for this platform".into(),
                        fix: Some("Keep WONDER_COMPUTER_BACKEND=python and install the documented Python screen sharing requirements. No separate Cupola helper download is needed.".into()),
                    });
                }
                let result = computer_backend(paths).and_then(|backend| {
                    if backend == ComputerBackend::Native {
                        let exe = executable_path()?;
                        let helper = native_computer_helper(exe.parent().ok_or("no executable directory")?)?;
                        let probe = process::output(Command::new(&helper).arg("--version"))?;
                        if !probe.status.success() { return Err("Native screen sharing could not start. Reinstall the complete Cupola archive and its documented system packages.".into()); }
                        Ok("Native screen sharing is installed; Python is not needed for this backend".to_owned())
                    } else {
                        #[cfg(all(unix, not(target_os = "macos")))]
                        let installed = executable_path().ok().and_then(|exe| installed_computer_python(&exe));
                        #[cfg(not(all(unix, not(target_os = "macos"))))]
                        let installed: Option<PathBuf> = None;
                        let python = std::env::var_os("WONDER_COMPUTER_PYTHON").map(PathBuf::from)
                            .or(installed)
                            .or_else(|| which(if cfg!(windows) {"python"} else {"python3"}))
                            .ok_or("Python for screen sharing was not found. Install Python 3.12+ and the computer helper requirements, or choose the installed native backend.")?;
                        let imports = if cfg!(windows) {"import aiortc, mss, numpy, windows_capture"} else {"import aiortc, mss, numpy"};
                        let probe = process::output(Command::new(&python).args(["-c", imports]))?;
                        if !probe.status.success() { return Err(if cfg!(windows) { "Python screen sharing dependencies are missing. Run that Python's pip install -r computer-windows/requirements.txt.".into() } else { "Python screen sharing dependencies are missing (aiortc, mss, numpy)".into() }); }
                        Ok(format!("Python screen sharing requirements are installed at {}", python.display()))
                    }
                });
                let ok = result.is_ok();
                let summary = result.unwrap_or_else(|error| error.to_string());
                checks.push(check(
                    "computer-backend",
                    ok,
                    summary,
                    (!ok).then(|| {
                        if native_unavailable {
                            python_backend_fix()
                        } else {
                            GENERIC_PYTHON_FIX.into()
                        }
                    }),
                ));
                Ok(checks)
            }));
            #[cfg(target_os = "macos")]
            steps.push(Box::new(|| Ok(macos::doctor_checks())));
            #[cfg(all(unix, not(target_os = "macos")))]
            steps.push(Box::new(|| {
                let configured = existing_display();
                let mut environment: std::collections::BTreeMap<_, _> =
                    std::env::vars_os().collect();
                if let Some(display) = &configured {
                    environment.insert("DISPLAY".into(), display.into());
                }
                let environment = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?
                    .block_on(wonderd::linux_desktop::refresh(environment));
                let current = environment
                    .get(std::ffi::OsStr::new("DISPLAY"))
                    .and_then(|s| s.to_str());
                Ok(linux_display_checks(
                    configured
                        .as_deref()
                        .map(|chosen| current.unwrap_or(chosen)),
                    session_is_wayland(
                        environment
                            .get(std::ffi::OsStr::new("XDG_SESSION_TYPE"))
                            .and_then(|s| s.to_str()),
                        environment.contains_key(std::ffi::OsStr::new("WAYLAND_DISPLAY")),
                    ),
                    |display| current.is_some() && x11_socket(display).exists(),
                    |tool| which(tool).is_some(),
                ))
            }));
            steps.push(Box::new(|| {
                Ok(vec![cua_driver_check(
                    wonderd::computer_runtime::installed_cua_driver().as_deref(),
                )])
            }));
        }
    }
    steps.push(Box::new(|| {
        #[cfg(target_os = "macos")]
        let service: ServiceOutput = macos::service_state().map_err(|error| error.to_string())?;
        #[cfg(not(target_os = "macos"))]
        let service = service_state();
        Ok(vec![Check {
            id: "service",
            status: if service.installed {
                CheckStatus::Ok
            } else {
                CheckStatus::Warning
            },
            summary: match (service.installed, service.starts_at_boot) {
                (true, Some(true)) => "The host starts when this computer boots".into(),
                (true, _) => "The host starts when you sign in".into(),
                (false, _) => "The host does not start automatically".into(),
            },
            fix: (!service.installed).then(|| "Run `cupola service install`.".into()),
        }])
    }));
    steps.push(Box::new(|| {
        let runtimes = LocalRuntimes::discover();
        let any_provider = runtimes.codex.is_some()
            || (runtimes.claude_bridge.is_some() && runtimes.node.is_some())
            || runtimes.opencode_usable()
            || runtimes.cursor().installed;
        Ok(vec![Check {
            id: "providers",
            status: if any_provider {
                CheckStatus::Ok
            } else {
                CheckStatus::Warning
            },
            summary: if any_provider {
                "At least one agent provider is installed".into()
            } else {
                "No agent provider is installed".into()
            },
            fix: (!any_provider).then(|| "Run `cupola providers` for install steps.".into()),
        }])
    }));
    checks.extend(run_doctor_steps(steps)?);
    let ok = checks.iter().all(|c| c.status != CheckStatus::Failed);
    if as_json {
        print_json(&DoctorOutput { ok, checks });
    } else {
        write_checks(&mut std::io::stdout().lock(), &checks)?;
    }
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

// ---------------------------------------------------------------- routes

/// What the running supervisor last published: the newest origin status line,
/// or None when no supervisor runs.
fn published_status(paths: &Paths) -> Option<Value> {
    running_supervisor(paths)?;
    std::fs::read_to_string(paths.origin_status())
        .ok()?
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
}

fn restart_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    let installed = macos::service_state().is_ok_and(|service| service.installed);
    #[cfg(not(target_os = "macos"))]
    let installed = service_state().installed;
    if installed {
        "Restart the host to apply it: `cupola service restart`."
    } else {
        "Restart the host to apply it: `cupola stop`, then `cupola start`."
    }
}

/// The main route: Tailscale, or the owner's own `--origin`.
fn main_route(config: &Value, published: Option<&Value>) -> RouteStatus {
    // The running host's own route, when it says; config.json may have changed since.
    let running = published.and_then(|line| line["tunnel"].as_str());
    let tailscale = running.or(config["tunnel"].as_str()) == Some("tailscale");
    let configured = config["origin"].as_str().map(str::to_owned);
    let (state, origin, detail) = match published.and_then(|line| line["state"].as_str()) {
        None => (RouteState::Stopped, configured, None),
        Some("ready") => (
            RouteState::Ready,
            line_origin(published).or(configured),
            None,
        ),
        Some("unavailable") => (
            RouteState::Unavailable,
            configured,
            Some(if tailscale {
                "Tailscale is not answering on this computer. Check `tailscale status`; the host keeps retrying.".to_owned()
            } else {
                "This address is not available.".to_owned()
            }),
        ),
        Some("gave-up") => (
            RouteState::Stopped,
            configured,
            Some("The host stopped after crashing repeatedly. Restart it from a paired phone, or run `cupola stop`, then `cupola start`.".to_owned()),
        ),
        Some(_) => (RouteState::Starting, configured, None),
    };
    RouteStatus {
        kind: if tailscale {
            RouteKind::Tailscale
        } else {
            RouteKind::Custom
        },
        enabled: true,
        state,
        origins: origin.into_iter().collect(),
        detail,
    }
}

fn line_origin(published: Option<&Value>) -> Option<String> {
    published?["origin"].as_str().map(str::to_owned)
}

/// Why the host's home network listener did not answer at an address and port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ProbeFailure {
    /// Nothing listens there: the host has not opened the port (yet).
    Refused,
    /// No answer before the deadline: a firewall, or a stalled listener.
    Timeout,
    /// A secure connection could not be made, for a reason other than the key.
    TlsError,
    /// A secure connection with another key: another program holds the port.
    WrongKey,
}

/// Whether the host's home network listener answers at an address and port.
type Answers<'a> =
    &'a (dyn Fn(std::net::IpAddr, u16) -> std::result::Result<(), ProbeFailure> + Sync);

/// Asks `answers` about every address at once, so the whole list shares one
/// [`HOME_PROBE_RETRY`] budget instead of spending one per address. Results
/// keep the order of `addresses`.
fn probe_home_addresses(
    addresses: &[std::net::IpAddr],
    port: u16,
    answers: Answers,
) -> Vec<std::result::Result<(), ProbeFailure>> {
    if let [ip] = addresses {
        return vec![answers(*ip, port)];
    }
    std::thread::scope(|scope| {
        let probes: Vec<_> = addresses
            .iter()
            .map(|ip| scope.spawn(move || answers(*ip, port)))
            .collect();
        probes
            .into_iter()
            .map(|probe| {
                probe
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

/// How long a probe keeps retrying a listener that refuses, stalls or fails
/// the handshake: the host may be rebinding after an interface change.
const HOME_PROBE_RETRY: Duration = Duration::from_secs(2);
const HOME_PROBE_ATTEMPT: Duration = Duration::from_millis(750);

/// [`home_listener_attempt`] until it answers, until another key answers, or
/// for about [`HOME_PROBE_RETRY`].
fn home_listener_answers(
    host_key_spki: &[u8],
    address: std::net::SocketAddr,
) -> std::result::Result<(), ProbeFailure> {
    const PAUSE: Duration = Duration::from_millis(200);
    let deadline = Instant::now() + HOME_PROBE_RETRY;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let result = home_listener_attempt(host_key_spki, address, HOME_PROBE_ATTEMPT.min(left));
        let left = deadline.saturating_duration_since(Instant::now());
        match result {
            // Retry only while a pause still leaves an attempt real time.
            Err(failure) if failure != ProbeFailure::WrongKey && left > PAUSE * 2 => {
                std::thread::sleep(PAUSE);
            }
            result => return result,
        }
    }
}

/// One attempt, within `limit`, at a TLS handshake with this computer's host
/// key, as a phone that pinned it would.
fn home_listener_attempt(
    host_key_spki: &[u8],
    address: std::net::SocketAddr,
    limit: Duration,
) -> std::result::Result<(), ProbeFailure> {
    use std::sync::Arc;
    #[derive(Debug)]
    struct HostKeyOnly {
        spki: Vec<u8>,
        provider: Arc<rustls::crypto::CryptoProvider>,
        wrong_key: Arc<AtomicBool>,
    }
    impl rustls::client::danger::ServerCertVerifier for HostKeyOnly {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error>
        {
            // Compare the certificate's own key, as the phone's pin does; the
            // handshake signature below proves the server holds it.
            if certificate_spki(end_entity.as_ref()) == Some(self.spki.as_slice()) {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            } else {
                self.wrong_key.store(true, Ordering::SeqCst);
                Err(rustls::Error::General(
                    "not this computer's host key".into(),
                ))
            }
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
    // One deadline for connecting and the whole handshake.
    let deadline = Instant::now() + limit;
    let remaining =
        || Some(deadline.saturating_duration_since(Instant::now())).filter(|left| !left.is_zero());
    let timed_out = |error: &std::io::Error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    };
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let Ok(config) = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
    else {
        return Err(ProbeFailure::TlsError);
    };
    let wrong_key = Arc::new(AtomicBool::new(false));
    let config = config
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(HostKeyOnly {
            spki: host_key_spki.to_vec(),
            provider,
            wrong_key: wrong_key.clone(),
        }))
        .with_no_client_auth();
    let Ok(mut connection) =
        rustls::ClientConnection::new(Arc::new(config), "wonder.invalid".try_into().unwrap())
    else {
        return Err(ProbeFailure::TlsError);
    };
    let left = remaining().ok_or(ProbeFailure::Timeout)?;
    let mut socket = std::net::TcpStream::connect_timeout(&address, left).map_err(|error| {
        if error.kind() == std::io::ErrorKind::ConnectionRefused {
            ProbeFailure::Refused
        } else {
            ProbeFailure::Timeout
        }
    })?;
    // Drive the handshake one read or write at a time so each waits only for
    // what is left of the deadline.
    while connection.is_handshaking() {
        let left = remaining().ok_or(ProbeFailure::Timeout)?;
        if socket.set_read_timeout(Some(left)).is_err()
            || socket.set_write_timeout(Some(left)).is_err()
        {
            return Err(ProbeFailure::TlsError);
        }
        if connection.wants_write() {
            connection.write_tls(&mut socket).map_err(|error| {
                if timed_out(&error) {
                    ProbeFailure::Timeout
                } else {
                    ProbeFailure::TlsError
                }
            })?;
            continue;
        }
        match connection.read_tls(&mut socket) {
            Ok(0) => return Err(ProbeFailure::TlsError),
            Ok(_) => {}
            Err(error) if timed_out(&error) => return Err(ProbeFailure::Timeout),
            Err(_) => return Err(ProbeFailure::TlsError),
        }
        if connection.process_new_packets().is_err() {
            return Err(if wrong_key.load(Ordering::SeqCst) {
                ProbeFailure::WrongKey
            } else {
                ProbeFailure::TlsError
            });
        }
    }
    Ok(())
}

/// The SubjectPublicKeyInfo of a DER certificate, with its tag and length: the
/// field after the optional version, serial number, signature algorithm,
/// issuer, validity and subject of its TBSCertificate.
fn certificate_spki(certificate: &[u8]) -> Option<&[u8]> {
    /// A DER element's tag, the whole element, its contents and what follows it.
    type Element<'a> = (u8, &'a [u8], &'a [u8], &'a [u8]);
    /// Splits the DER element at the start of `input`.
    fn split(input: &[u8]) -> Option<Element<'_>> {
        let (&tag, after_tag) = input.split_first()?;
        let (&first, after_first) = after_tag.split_first()?;
        let (length, body) = if first < 0x80 {
            (usize::from(first), after_first)
        } else {
            let count = usize::from(first & 0x7f);
            if count == 0 || count > 4 || after_first.len() < count {
                return None;
            }
            let (bytes, body) = after_first.split_at(count);
            let length = bytes
                .iter()
                .fold(0usize, |length, &byte| length << 8 | usize::from(byte));
            (length, body)
        };
        if body.len() < length {
            return None;
        }
        let header = input.len() - body.len();
        let (whole, rest) = input.split_at(header + length);
        Some((tag, whole, &body[..length], rest))
    }
    const SEQUENCE: u8 = 0x30;
    const VERSION: u8 = 0xa0;
    let (SEQUENCE, _, certificate, _) = split(certificate)? else {
        return None;
    };
    let (SEQUENCE, _, mut fields, _) = split(certificate)? else {
        return None;
    };
    if fields.first() == Some(&VERSION) {
        fields = split(fields)?.3;
    }
    for _ in 0..5 {
        fields = split(fields)?.3;
    }
    match split(fields)? {
        (SEQUENCE, spki, _, _) => Some(spki),
        _ => None,
    }
}

/// Probes the running host's home network listeners with the saved host key.
fn home_listener_probe(
    paths: &Paths,
) -> impl Fn(std::net::IpAddr, u16) -> std::result::Result<(), ProbeFailure> + Sync + '_ {
    let spki = std::sync::OnceLock::new();
    move |ip, port| {
        let spki = spki.get_or_init(|| {
            wonderd::lan::load_or_create_host_key(&paths.data)
                .ok()
                .map(|key| key.public_key_spki())
        });
        // Without a readable key nothing can be verified; that is not another program.
        let spki = spki.as_deref().ok_or(ProbeFailure::TlsError)?;
        home_listener_answers(spki, std::net::SocketAddr::new(ip, port))
    }
}

/// What the owner should know when no home address answered, by the most
/// telling failure: only another key means another program holds the port.
fn home_probe_detail(failure: ProbeFailure, port: u16, restart: &str) -> String {
    match failure {
        ProbeFailure::WrongKey => format!(
            "Another program is using port {port} on this computer's home network addresses. \
             Choose another port with `cupola lan on --port <port>`. {restart}"
        ),
        ProbeFailure::TlsError => format!(
            "Something answered on port {port} but could not make a secure connection with \
             this computer's key. Check `cupola logs`. {restart}"
        ),
        ProbeFailure::Timeout => format!(
            "The host did not answer on port {port} within 2 seconds. A firewall may be \
             blocking it; run `cupola doctor`."
        ),
        ProbeFailure::Refused => format!(
            "The host is not listening on port {port} of this computer's home network \
             addresses; it could not open the port. Check `cupola logs`. {restart}"
        ),
    }
}

/// The home network route, from the saved choice, what the running host was
/// started with, and which of this computer's home network addresses its listener
/// answers on.
fn home_route(
    config: &Value,
    published: Option<&Value>,
    addresses: &[std::net::IpAddr],
    answers: Answers,
    restart: &str,
) -> (u16, RouteStatus) {
    let wanted = home_port(config);
    let port = wanted
        .or_else(|| {
            config["homeNetwork"]["port"]
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
        })
        .unwrap_or(cli::DEFAULT_LAN_PORT);
    let serving = published
        .and_then(|line| line["homePort"].as_u64())
        .and_then(|p| u16::try_from(p).ok());
    // Before the daemon starts, the supervisor's lines carry no port yet.
    let daemon_pending = published.is_some_and(|line| {
        line["state"].as_str() == Some("starting") && line["homePort"].is_null()
    });
    let origins = addresses
        .iter()
        .map(|ip| wonderd::lan::origin(*ip, port))
        .collect();
    let (state, origins, detail) = match (wanted, published) {
        (None, Some(_)) if serving.is_some() => (
            RouteState::RestartNeeded,
            Vec::new(),
            Some(format!(
                "Turned off; the running host still accepts home network connections. {restart}"
            )),
        ),
        (None, _) => (RouteState::Off, Vec::new(), None),
        (Some(_), _) if addresses.is_empty() => (
            RouteState::Unavailable,
            Vec::new(),
            Some(
                "This computer has no home network address. Connect it to your Wi-Fi or \
                 Ethernet; a cloud server is reached through Tailscale instead."
                    .to_owned(),
            ),
        ),
        (Some(_), None) => (RouteState::Stopped, origins, None),
        (Some(port), Some(_)) if serving == Some(port) => {
            let mut answering = Vec::new();
            let mut failure = None;
            for (ip, result) in addresses
                .iter()
                .zip(probe_home_addresses(addresses, port, answers))
            {
                match result {
                    Ok(()) => answering.push(wonderd::lan::origin(*ip, port)),
                    Err(next) => failure = failure.max(Some(next)),
                }
            }
            match failure.filter(|_| answering.is_empty()) {
                Some(failure) => (
                    RouteState::Unavailable,
                    Vec::new(),
                    Some(home_probe_detail(failure, port, restart)),
                ),
                None => (RouteState::Ready, answering, None),
            }
        }
        (Some(_), Some(_)) if daemon_pending => (RouteState::Starting, origins, None),
        (Some(_), Some(_)) => (RouteState::RestartNeeded, origins, Some(restart.to_owned())),
    };
    (
        port,
        RouteStatus {
            kind: RouteKind::Home,
            enabled: wanted.is_some(),
            state,
            origins,
            detail,
        },
    )
}

/// Why the relay cannot carry a pairing: it carries only the encrypted
/// connection of a phone that already knows this computer's key.
const RELAY_PAIRING: &str = "Pair on the home network or Tailscale first. After that your \
     iPhone also reaches this computer through the relay, encrypted end to end.";
const RELAY_OFF_HINT: &str = "Run `cupola relay on` to reach this computer from anywhere \
     through Cupola's relay, encrypted end to end.";

/// The relay row from the daemon's relay state (`relay.json` and the
/// owner's setting). A tunnel the stopped host left behind reads as stopped.
fn relay_route(view: &wonderd::relay::RelayView, running: bool) -> RouteStatus {
    use wonderd::relay::RelayState;
    let state = match view.state {
        _ if !view.allowed => RouteState::Off,
        RelayState::Off if view.enabled => RouteState::Starting,
        // Allowed here; a phone has not turned it on yet.
        RelayState::Off => RouteState::Allowed,
        _ if !running => RouteState::Stopped,
        RelayState::Starting => RouteState::Starting,
        RelayState::Ready => RouteState::Ready,
        RelayState::Unavailable => RouteState::Unavailable,
        RelayState::Stopped => RouteState::Stopped,
    };
    let detail = if view.allowed {
        view.detail.clone()
    } else {
        view.detail
            .clone()
            .or_else(|| Some(RELAY_OFF_HINT.to_owned()))
    };
    RouteStatus {
        kind: RouteKind::Relay,
        enabled: view.allowed,
        state,
        origins: view.origin.iter().cloned().collect(),
        detail,
    }
}

/// Every route in preference order (home network, then the main route, then
/// the relay), as a phone tries them. A Tailscale row is listed even when the
/// host uses its own address, so turning it off reads as off.
fn all_routes(
    config: &Value,
    published: Option<&Value>,
    addresses: &[std::net::IpAddr],
    answers: Answers,
    restart: &str,
    relay: RouteStatus,
) -> Vec<RouteStatus> {
    let mut routes = vec![home_route(config, published, addresses, answers, restart).1];
    let main = main_route(config, published);
    let custom = main.kind == RouteKind::Custom;
    routes.push(main);
    if custom {
        routes.push(RouteStatus {
            kind: RouteKind::Tailscale,
            enabled: false,
            state: RouteState::Off,
            origins: Vec::new(),
            detail: None,
        });
    }
    routes.push(relay);
    routes
}

/// The host key pin, while the home network is on or was ever chosen (reading
/// it creates the key, as `cupola lan on` does); otherwise `routes` and
/// `status` never create one.
fn host_key_pin(paths: &Paths, config: &Value) -> Option<String> {
    if config["homeNetwork"].is_null() && home_port(config).is_none() {
        return None;
    }
    wonderd::lan::load_or_create_host_key(&paths.data)
        .ok()
        .map(|key| key.pin())
}

/// Same Wi-Fi connections are on and this computer has a private network
/// address for them to listen on.
fn same_wifi_reachable(config: &Value, addresses: &[std::net::IpAddr]) -> bool {
    home_port(config).is_some() && !addresses.is_empty()
}

fn home_addresses() -> Vec<std::net::IpAddr> {
    wonderd::lan::home_addresses(&wonderd::lan::interface_addresses())
}

fn route_label(kind: RouteKind) -> &'static str {
    match kind {
        RouteKind::Tailscale => "Tailscale",
        RouteKind::Home => "Home network",
        RouteKind::Relay => "Relay",
        RouteKind::Custom => "Your address",
    }
}

fn state_label(state: RouteState) -> &'static str {
    match state {
        RouteState::Ready => "ready",
        RouteState::Starting => "starting",
        RouteState::Unavailable => "unavailable",
        RouteState::Stopped => "host not running",
        RouteState::RestartNeeded => "restart to apply",
        RouteState::Off => "off",
        RouteState::Allowed => "allowed",
    }
}

fn print_route(out: &mut dyn Write, route: &RouteStatus) -> std::io::Result<()> {
    let mut line = format!(
        "{:<13} {:<17} {}",
        route_label(route.kind),
        state_label(route.state),
        route.origins.join(", ")
    );
    line.truncate(line.trim_end().len());
    writeln!(out, "{line}")?;
    if let Some(detail) = &route.detail {
        writeln!(out, "{:<13} {detail}", "")?;
    }
    Ok(())
}

fn write_json<T: serde::Serialize>(out: &mut dyn Write, value: &T) -> Result<()> {
    writeln!(out, "{}", serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn routes(as_json: bool) -> Result<()> {
    routes_at(
        &Paths::resolve()?,
        &home_addresses(),
        restart_hint(),
        as_json,
        &mut std::io::stdout().lock(),
    )
}

/// `cupola routes` for a data dir and this computer's home network addresses.
fn routes_at(
    paths: &Paths,
    addresses: &[std::net::IpAddr],
    restart: &str,
    as_json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let config = load_config(paths)?;
    let published = published_status(paths);
    let output = RoutesOutput {
        running: published.is_some(),
        routes: all_routes(
            &config,
            published.as_ref(),
            addresses,
            &home_listener_probe(paths),
            restart,
            relay_route(&wonderd::relay::view(&paths.data), published.is_some()),
        ),
        host_key_pin: host_key_pin(paths, &config),
    };
    if as_json {
        return write_json(out, &output);
    }
    for route in &output.routes {
        print_route(out, route)?;
    }
    if let Some(pin) = &output.host_key_pin {
        writeln!(out, "{:<13} {pin}", "Computer key")?;
    }
    Ok(())
}

fn save_config(paths: &Paths, config: &Value) -> Result<()> {
    validate_config(config)?;
    write_private(
        &paths.config(),
        serde_json::to_string_pretty(config)?.as_bytes(),
    )
}

fn lan(command: LanCommand, as_json: bool) -> Result<()> {
    lan_at(
        command,
        &Paths::resolve()?,
        &home_addresses(),
        restart_hint(),
        as_json,
        &mut std::io::stdout().lock(),
    )
}

/// `cupola lan` for a data dir and this computer's home network addresses.
fn lan_at(
    command: LanCommand,
    paths: &Paths,
    addresses: &[std::net::IpAddr],
    restart: &str,
    as_json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let mut config = load_config(paths)?;
    match command {
        LanCommand::On { port } => {
            if cfg!(windows) {
                return Err(cli::not_on_this_platform(
                    "lan on",
                    "Connect your iPhone through Tailscale.",
                )
                .into());
            }
            // Made now, so the key a phone pins exists before the host starts.
            wonderd::lan::load_or_create_host_key(&paths.data)?;
            config["homeNetwork"] = json!({ "enabled": true, "port": port });
            save_config(paths, &config)?;
        }
        LanCommand::Off => {
            // Saved even when nothing was chosen yet: no choice means on.
            if config["homeNetwork"].is_null() {
                config["homeNetwork"] = json!({ "port": cli::DEFAULT_LAN_PORT });
            }
            config["homeNetwork"]["enabled"] = json!(false);
            save_config(paths, &config)?;
        }
        LanCommand::Status => {}
    }
    let published = published_status(paths);
    let (port, route) = home_route(
        &config,
        published.as_ref(),
        addresses,
        &home_listener_probe(paths),
        restart,
    );
    let output = LanOutput {
        enabled: route.enabled,
        port,
        state: route.state,
        origins: route.origins,
        host_key_pin: host_key_pin(paths, &config),
        detail: route.detail,
    };
    if as_json {
        return write_json(out, &output);
    }
    let addresses = output
        .origins
        .iter()
        .map(|origin| origin.trim_start_matches("https://"))
        .collect::<Vec<_>>()
        .join(", ");
    let headline = match output.state {
        RouteState::Off => writeln!(out, "Home network connections are off."),
        RouteState::Ready => writeln!(
            out,
            "Home network connections are on. iPhones on this network can connect at {addresses}."
        ),
        RouteState::Starting => {
            writeln!(out, "Home network connections are starting at {addresses}.")
        }
        RouteState::Stopped => writeln!(
            out,
            "Home network connections are on, at {addresses} once the host runs (`cupola start`)."
        ),
        _ if output.enabled => writeln!(out, "Home network connections are on."),
        _ => writeln!(out, "Home network connections are off."),
    };
    headline?;
    if let Some(detail) = &output.detail {
        writeln!(out, "{detail}")?;
    }
    if let Some(pin) = output.host_key_pin.as_ref().filter(|_| output.enabled) {
        writeln!(out, "Computer key: {pin}")?;
    }
    Ok(())
}

/// `cupola computer control on|off|status`. Only the macOS helper reads a
/// saved choice: Windows asks on the computer for each request and shows
/// Stop while controlled, and Linux shares its own virtual screen.
fn computer(command: ComputerCommand, as_json: bool) -> Result<()> {
    let ComputerCommand::Control { command } = command;
    #[cfg(target_os = "macos")]
    return computer_control_at(
        command,
        &Paths::resolve()?,
        as_json,
        &mut std::io::stdout().lock(),
    );
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (command, as_json);
        Err(cli::not_on_this_platform(
            "computer control",
            if cfg!(windows) {
                "Take control asks on this computer each time instead, and shows Stop while your phone is in control."
            } else {
                "Take control works on the screen this computer shares, without a saved choice."
            },
        )
        .into())
    }
}

#[cfg(target_os = "macos")]
fn computer_control_at(
    command: cli::ControlCommand,
    paths: &Paths,
    as_json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    use cli::{ComputerControlOutput, ControlCommand};
    let config = load_config(paths)?;
    match command {
        ControlCommand::On => control_preference::write(&paths.service_dir(), true)?,
        ControlCommand::Off => control_preference::write(&paths.service_dir(), false)?,
        ControlCommand::Status => {}
    }
    let output = ComputerControlOutput {
        viewing: config["computer"].as_bool() == Some(true),
        control: control_state(paths),
    };
    if as_json {
        return write_json(out, &output);
    }
    let check = computer_control_check(&config, output.control);
    writeln!(out, "{}", check.summary)?;
    if let Some(fix) = check.fix {
        writeln!(out, "{fix}")?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn control_state(paths: &Paths) -> cli::ControlState {
    match control_preference::read(&paths.service_dir()) {
        control_preference::ControlPreference::Enabled => cli::ControlState::On,
        control_preference::ControlPreference::Disabled => cli::ControlState::Off,
        control_preference::ControlPreference::Unavailable => cli::ControlState::Unavailable,
    }
}

/// Doctor and `computer control`: what the iPhone may do with this Mac.
#[cfg(target_os = "macos")]
fn computer_control_check(config: &Value, control: cli::ControlState) -> Check {
    use cli::ControlState;
    let viewing = config["computer"].as_bool() == Some(true);
    let (status, summary, fix) = match (viewing, control) {
        (false, _) => (
            CheckStatus::Ok,
            "Computer view is off: your iPhone cannot view or control this computer",
            Some("To share this computer's screen, run `cupola init --computer` with your other setup options."),
        ),
        (true, ControlState::On) => (
            CheckStatus::Ok,
            "Your iPhone can view and take control of this computer",
            None,
        ),
        (true, ControlState::Off) => (
            CheckStatus::Ok,
            "Your iPhone can view this computer; control is off",
            Some("To allow Take control, run `cupola computer control on`."),
        ),
        (true, ControlState::Unavailable) => (
            CheckStatus::Warning,
            "Your iPhone can view this computer, but control is refused: the saved control choice could not be trusted",
            Some("Run `cupola computer control on` (or `off`) to save it again."),
        ),
    };
    Check {
        id: "computer-control",
        status,
        summary: summary.into(),
        fix: fix.map(Into::into),
    }
}

fn relay_command(command: RelayCommand, as_json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    load_config(&paths)?;
    let host = match running_supervisor(&paths) {
        Some(_) => Some(Local::connect()?),
        None => None,
    };
    let output = tokio::runtime::Runtime::new()?.block_on(relay_at(
        &paths,
        host.as_ref(),
        wonderd::relay::Environment::from_env(),
        command,
    ))?;
    if as_json {
        print_json(&output);
    } else {
        println!("{}", relay_text(&output));
    }
    Ok(())
}

/// `cupola relay on|off|status` against `host`, the running host, or the
/// data directory when it is stopped (`environment` reaches the Worker).
async fn relay_at(
    paths: &Paths,
    host: Option<&Local>,
    environment: wonderd::relay::Environment,
    command: RelayCommand,
) -> Result<RelayOutput> {
    let view = match (command, host) {
        (RelayCommand::Status, _) => wonderd::relay::view(&paths.data),
        (RelayCommand::On | RelayCommand::Off, Some(host)) => {
            let allowed = matches!(command, RelayCommand::On);
            let view = host
                .call("relay.set", json!({ "allowed": allowed }))
                .await?;
            serde_json::from_value(view)?
        }
        // The host starts the tunnel when it starts.
        (RelayCommand::On, None) => {
            wonderd::relay::write_settings(
                &paths.data,
                wonderd::relay::RelaySettings { allowed: true },
            )?;
            wonderd::relay::view(&paths.data)
        }
        (RelayCommand::Off, None) => {
            wonderd::relay::Manager::new(paths.data.clone(), environment, None)
                .set_allowed(false)
                .await
                .map_err(Unavailable)?
        }
    };
    let route = relay_route(&view, host.is_some());
    Ok(RelayOutput {
        enabled: route.enabled,
        state: route.state,
        origin: route.origins.first().cloned(),
        detail: route.detail,
    })
}

fn relay_text(output: &RelayOutput) -> String {
    let mut text = format!("Relay {}", state_label(output.state));
    if let Some(origin) = &output.origin {
        text.push_str(&format!(" at {origin}"));
    }
    text.push('.');
    if let Some(detail) = &output.detail {
        text.push(' ');
        text.push_str(detail);
    }
    text
}

/// `pair --route`: the main route pairs as before, and the home network once
/// it is turned on; the relay carries only an already paired phone's sealed
/// connection, so it says so instead of offering a code that cannot work.
fn check_pair_route(route: Option<PairRoute>) -> Result<PairVia> {
    if route == Some(PairRoute::Relay) {
        pair_route_allowed(PairRoute::Relay, &Value::Null)?;
    }
    pair_via(route, &load_config(&Paths::resolve()?)?)
}

/// Only a pair without `--route` may fall back to same Wi-Fi.
fn pair_via(route: Option<PairRoute>, config: &Value) -> Result<PairVia> {
    match route {
        None => Ok(PairVia::Any {
            home: home_port(config).is_some(),
        }),
        Some(route) => {
            pair_route_allowed(route, config)?;
            Ok(if route == PairRoute::Home {
                PairVia::Home
            } else {
                PairVia::Main
            })
        }
    }
}

fn pair_route_allowed(route: PairRoute, config: &Value) -> Result<()> {
    match route {
        PairRoute::Tailscale => {
            if config["tunnel"].as_str() == Some("tailscale") {
                Ok(())
            } else {
                Err(format!(
                    "Tailscale is off for this computer; it is reached at {}. Run `cupola pair` without --route.",
                    config["origin"].as_str().unwrap_or("its own address")
                )
                .into())
            }
        }
        PairRoute::Home => {
            if home_port(config).is_some() {
                Ok(())
            } else {
                Err(Unavailable(
                    "Home network connections are off. Turn them on with `cupola lan on` and \
                     restart the host, or run `cupola pair` without --route."
                        .to_owned(),
                )
                .into())
            }
        }
        PairRoute::Relay => Err(Unavailable(RELAY_PAIRING.to_owned()).into()),
    }
}

/// Doctor: whether the home network route can work, and why not.
fn home_network_check(config: &Value, published: Option<&Value>, answers: Answers) -> Check {
    let addresses = home_addresses();
    let (_, route) = home_route(config, published, &addresses, answers, restart_hint());
    let at = route
        .origins
        .iter()
        .map(|origin| origin.trim_start_matches("https://"))
        .collect::<Vec<_>>()
        .join(", ");
    let (status, summary) = match route.state {
        // `Allowed` is the relay's alone.
        RouteState::Off | RouteState::Allowed => (
            CheckStatus::Ok,
            if cfg!(windows) {
                // `lan on` refuses on Windows.
                "Home network connections are not available on Windows; phones connect through Tailscale"
            } else {
                "Home network connections are off (`cupola lan on` lets an iPhone on the same Wi-Fi connect directly)"
            }
            .to_owned(),
        ),
        RouteState::Ready => (
            CheckStatus::Ok,
            format!("iPhones on this network can connect at {at}"),
        ),
        RouteState::Starting | RouteState::Stopped => (
            CheckStatus::Ok,
            format!("Home network connections are on, at {at}"),
        ),
        RouteState::Unavailable | RouteState::RestartNeeded => (
            CheckStatus::Warning,
            route
                .detail
                .clone()
                .unwrap_or_else(|| "Home network connections are not working".into()),
        ),
    };
    Check {
        id: "home-network",
        status,
        summary,
        fix: None,
    }
}

// ufw and firewalld exist only on Linux; the rules stay testable everywhere.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Firewall {
    Ufw,
    Firewalld,
}

impl Firewall {
    fn name(self) -> &'static str {
        match self {
            Firewall::Ufw => "ufw",
            Firewall::Firewalld => "firewalld",
        }
    }
}

/// The firewall that may stand between the home network and the host: ufw when
/// it is enabled, otherwise firewalld when it runs. Reading the rules needs root,
/// so this never claims a port is open.
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_firewall() -> Option<Firewall> {
    let ufw = std::fs::read_to_string("/etc/ufw/ufw.conf").is_ok_and(|text| {
        text.lines()
            .any(|line| line.trim().eq_ignore_ascii_case("ENABLED=yes"))
    });
    if ufw {
        return Some(Firewall::Ufw);
    }
    let firewalld = Command::new("systemctl")
        .args(["is-active", "--quiet", "firewalld"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    firewalld.then_some(Firewall::Firewalld)
}

/// The private range each home address belongs to: what an allow rule should
/// name, so the port opens to the home network and nothing wider.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn private_ranges(addresses: &[std::net::IpAddr]) -> Vec<&'static str> {
    let mut ranges: Vec<&'static str> = addresses
        .iter()
        .map(|ip| match ip {
            std::net::IpAddr::V4(v4) => match v4.octets() {
                [10, ..] => "10.0.0.0/8",
                [172, ..] => "172.16.0.0/12",
                _ => "192.168.0.0/16",
            },
            std::net::IpAddr::V6(_) => "fc00::/7",
        })
        .collect();
    if ranges.is_empty() {
        ranges.push("192.168.0.0/16");
    }
    ranges.sort_unstable();
    ranges.dedup();
    ranges
}

/// The exact commands that let home network connections through; Wonder never
/// changes the firewall itself.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn firewall_fix(firewall: Firewall, port: u16, addresses: &[std::net::IpAddr]) -> String {
    let commands: Vec<String> = private_ranges(addresses)
        .into_iter()
        .map(|range| match firewall {
            Firewall::Ufw => format!("sudo ufw allow from {range} to any port {port} proto tcp"),
            Firewall::Firewalld => format!(
                "sudo firewall-cmd --permanent --add-rich-rule='rule family={} source address={range} port port={port} protocol=tcp accept'",
                if range.contains(':') { "ipv6" } else { "ipv4" }
            ),
        })
        .chain((firewall == Firewall::Firewalld).then(|| "sudo firewall-cmd --reload".to_owned()))
        .collect();
    format!(
        "If your iPhone cannot connect on the home network, allow the port: {}",
        commands.join(" && ")
    )
}

/// Doctor: the firewall state, and what it means for the home network route.
fn firewall_check(config: &Value) -> Option<Check> {
    #[cfg(target_os = "macos")]
    return Some(mac_firewall_status(
        &mac_firewall_state(),
        home_port(config),
    ));
    #[cfg(all(unix, not(target_os = "macos")))]
    return Some(firewall_status(
        linux_firewall(),
        home_port(config),
        &home_addresses(),
    ));
    // Not checked on Windows yet; only worth saying when it matters.
    #[cfg(windows)]
    return home_port(config).map(|_| Check {
        id: "firewall",
        status: CheckStatus::Unavailable,
        summary: "Cupola cannot check this computer's firewall yet".into(),
        fix: None,
    });
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// What the macOS application firewall says, from `socketfilterfw`, which
/// any user may query.
#[derive(Debug, Default)]
struct MacFirewall {
    enabled: bool,
    block_all: bool,
    /// "Automatically allow downloaded signed software".
    allow_signed: bool,
    /// The host's daemon: its path, whether it carries a Developer ID
    /// signature, and its rule when the firewall lists it.
    daemon: Option<(String, bool, Option<bool>)>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl MacFirewall {
    /// Output that names no firewall state is an error, never "off".
    fn parse(
        global: &str,
        block_all: &str,
        allow_signed: &str,
    ) -> std::result::Result<Self, String> {
        let global = global.to_ascii_lowercase();
        let enabled = global.contains("firewall is enabled")
            || global.contains("state = 1")
            || global.contains("state = 2");
        if !enabled && !global.contains("firewall is disabled") && !global.contains("state = 0") {
            return Err("socketfilterfw did not report the firewall state".into());
        }
        Ok(Self {
            enabled,
            block_all: block_all.to_ascii_lowercase().contains("set to enabled")
                || global.contains("state = 2"),
            allow_signed: allow_signed
                .lines()
                .any(|line| line.contains("downloaded signed software ENABLED")),
            daemon: None,
        })
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// Each listed path and whether incoming connections are allowed:
/// `N : <path>` followed by `(Allow incoming connections)` or `(Block ...)`.
fn mac_firewall_apps(listapps: &str) -> Vec<(String, bool)> {
    let mut apps = Vec::new();
    let mut path: Option<String> = None;
    for line in listapps.lines() {
        let line = line.trim();
        if let Some((number, rest)) = line.split_once(" : ") {
            if number.trim().parse::<u32>().is_ok() {
                path = Some(rest.trim().to_owned());
                continue;
            }
        }
        if let Some(path) = path.take() {
            if line.contains("Allow incoming") {
                apps.push((path, true));
            } else if line.contains("Block incoming") {
                apps.push((path, false));
            }
        }
    }
    apps
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// The rule that covers `executable`: its own entry, or its app bundle's.
fn mac_firewall_rule(apps: &[(String, bool)], executable: &str) -> Option<bool> {
    apps.iter()
        .find(|(path, _)| {
            executable == path
                || executable.starts_with(&format!("{}/", path.trim_end_matches('/')))
        })
        .map(|(_, allowed)| *allowed)
}

#[cfg(target_os = "macos")]
fn mac_firewall_state() -> std::result::Result<MacFirewall, String> {
    let query = |argument: &str| -> std::result::Result<String, String> {
        let output = Command::new("/usr/libexec/ApplicationFirewall/socketfilterfw")
            .arg(argument)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| format!("could not run socketfilterfw: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "socketfilterfw {argument} failed ({})",
                output.status
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let mut firewall = MacFirewall::parse(
        &query("--getglobalstate")?,
        &query("--getblockall")?,
        &query("--getallowsigned")?,
    )?;
    if !firewall.enabled {
        return Ok(firewall);
    }
    let daemon = std::env::var_os("WONDER_DAEMON_BIN")
        .map(PathBuf::from)
        .or_else(|| sibling("wonderd"))
        .map(|path| wonderd::canonical_path::canonicalize(&path).unwrap_or(path));
    if let Some(daemon) = daemon {
        let path = daemon.display().to_string();
        let signed = Command::new("/usr/bin/codesign")
            .args(["-dv", "--verbose=2"])
            .arg(&daemon)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .is_ok_and(|output| {
                String::from_utf8_lossy(&output.stderr)
                    .contains("Authority=Developer ID Application")
            });
        let rule = mac_firewall_rule(&mac_firewall_apps(&query("--listapps")?), &path);
        firewall.daemon = Some((path, signed, rule));
    }
    Ok(firewall)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// Doctor on macOS: whether the application firewall lets an iPhone on the
/// home network reach the host. Cupola never changes the firewall itself.
fn mac_firewall_status(
    firewall: &std::result::Result<MacFirewall, String>,
    home_port: Option<u16>,
) -> Check {
    let check = |status, summary: String, fix: Option<String>| Check {
        id: "firewall",
        status,
        summary,
        fix,
    };
    let firewall = match firewall {
        Ok(firewall) => firewall,
        Err(reason) => {
            return check(
                CheckStatus::Unavailable,
                format!("Cupola could not read the macOS firewall settings: {reason}"),
                None,
            )
        }
    };
    let settings = "System Settings > Network > Firewall > Options";
    if !firewall.enabled {
        return check(CheckStatus::Ok, "The macOS firewall is off".into(), None);
    }
    if home_port.is_none() {
        return check(
            CheckStatus::Ok,
            "The macOS firewall is on; it matters only for home network connections".into(),
            None,
        );
    }
    if firewall.block_all {
        return check(
            CheckStatus::Warning,
            "The macOS firewall blocks all incoming connections, so iPhones cannot connect on the home network".into(),
            Some(format!("Turn off \"Block all incoming connections\" in {settings}.")),
        );
    }
    match &firewall.daemon {
        Some((_, _, Some(true))) => check(
            CheckStatus::Ok,
            "The macOS firewall is on and allows the Cupola host".into(),
            None,
        ),
        Some((path, _, Some(false))) => check(
            CheckStatus::Warning,
            "The macOS firewall blocks the Cupola host, so iPhones cannot connect on the home network".into(),
            Some(format!("In {settings}, set {path} to \"Allow incoming connections\".")),
        ),
        Some((_, true, None)) if firewall.allow_signed => check(
            CheckStatus::Ok,
            "The macOS firewall is on and allows signed apps such as the Cupola host".into(),
            None,
        ),
        Some((path, _, None)) => check(
            CheckStatus::Warning,
            "The macOS firewall is on and has no rule for the Cupola host; it may block home network connections".into(),
            Some(format!(
                "In {settings}, add {path} and set it to \"Allow incoming connections\"."
            )),
        ),
        None => check(
            CheckStatus::Warning,
            "The macOS firewall is on; Cupola could not find its host program to check its rule".into(),
            None,
        ),
    }
}

#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn firewall_status(
    firewall: Option<Firewall>,
    home_port: Option<u16>,
    addresses: &[std::net::IpAddr],
) -> Check {
    match (firewall, home_port) {
        (None, _) => Check {
            id: "firewall",
            status: CheckStatus::Ok,
            summary: "No ufw or firewalld firewall is on".into(),
            fix: None,
        },
        (Some(firewall), None) => Check {
            id: "firewall",
            status: CheckStatus::Ok,
            summary: format!(
                "The {} firewall is on; it matters only for home network connections",
                firewall.name()
            ),
            fix: None,
        },
        (Some(firewall), Some(port)) => Check {
            id: "firewall",
            status: CheckStatus::Warning,
            summary: format!(
                "The {} firewall is on and may block home network connections on port {port}",
                firewall.name()
            ),
            fix: Some(firewall_fix(firewall, port, addresses)),
        },
    }
}

#[cfg(target_os = "macos")]
fn permissions(command: Option<PermissionsCommand>, as_json: bool) -> Result<()> {
    show_permissions(
        command,
        as_json,
        &mut std::io::stdout(),
        macos::grant_permissions,
        macos::permission_status,
    )
}

#[cfg(not(target_os = "macos"))]
fn permissions(command: Option<PermissionsCommand>, as_json: bool) -> Result<()> {
    // Linux and Windows ask for no system permissions, so `status` and
    // `grant` agree: Linux's virtual screen uses its own private X display,
    // and Windows captures only the signed-in user's own session. macOS
    // lists its permissions.
    show_permissions(
        command,
        as_json,
        &mut std::io::stdout(),
        || Ok(()),
        || {
            Ok(PermissionsOutput {
                permissions: Vec::new(),
            })
        },
    )
}

/// Bare `permissions` is `status`: only `grant` asks the system for anything.
fn show_permissions(
    command: Option<PermissionsCommand>,
    as_json: bool,
    out: &mut impl Write,
    grant: impl FnOnce() -> Result<()>,
    status: impl FnOnce() -> Result<PermissionsOutput>,
) -> Result<()> {
    if matches!(command, Some(PermissionsCommand::Grant)) {
        grant()?;
    }
    let output = status()?;
    if as_json {
        writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
    } else if output.permissions.is_empty() {
        let platform = if cfg!(windows) { "Windows" } else { "Linux" };
        writeln!(out, "Cupola needs no system permissions on {platform}.")?;
    } else {
        for permission in output.permissions {
            writeln!(out, "{}", permission.summary)?;
            if let Some(fix) = permission.fix {
                writeln!(out, "  {fix}")?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- service (Windows)

/// What `doctor` reports; a task that cannot be read counts as not installed.
#[cfg(windows)]
fn service_state() -> ServiceOutput {
    ServiceOutput {
        manager: "windows",
        installed: matches!(os::service_status(), Ok(Some(_))),
        running: Paths::resolve()
            .ok()
            .and_then(|paths| running_supervisor(&paths))
            .is_some(),
        starts_at_boot: None,
    }
}

/// Windows: the service is a logon task in the user's own session (see
/// `os_windows.rs`), so the screen helper has a desktop to capture.
#[cfg(windows)]
fn service(command: ServiceCommand, as_json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    match command {
        ServiceCommand::Install => {
            persist_computer_backend(&paths)?;
            os::service(Some(&paths.data))?;
        }
        ServiceCommand::Uninstall => os::service(None)?,
        ServiceCommand::Status => {}
        ServiceCommand::Restart => {
            if os::service_status()?.is_none() {
                return Err("the service is not installed; run `cupola service install`".into());
            }
            stop_host(&paths)?;
            os::service_run()?;
            // The task starts `wonder start`, which returns once the
            // supervisor holds its lock.
            let deadline = Instant::now() + Duration::from_secs(15);
            while running_supervisor(&paths).is_none() {
                if Instant::now() >= deadline {
                    return Err("the service did not start the host within 15 seconds; \
                                check `cupola logs`"
                        .into());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    let output = ServiceOutput {
        manager: "windows",
        installed: os::service_status()?.is_some(),
        running: running_supervisor(&paths).is_some(),
        // A logon task runs only once someone signs in.
        starts_at_boot: None,
    };
    if as_json {
        print_json(&output);
        return Ok(());
    }
    match (command, output.installed) {
        (ServiceCommand::Install, _) => {
            println!("The Cupola host now starts when you sign in to Windows.");
            if !output.running {
                println!("It is not running now; start it with `cupola start`.");
            }
        }
        (ServiceCommand::Uninstall, _) => {
            println!("The Cupola host no longer starts when you sign in.");
            if output.running {
                println!("It is still running; stop it with `cupola stop`.");
            }
        }
        (ServiceCommand::Restart, _) => println!("Restarted the Cupola host."),
        (ServiceCommand::Status, true) => println!(
            "Installed: starts when you sign in. {}",
            if output.running {
                "Running now."
            } else {
                "Not running now."
            }
        ),
        (ServiceCommand::Status, false) => {
            println!("Not installed. Run `cupola service install`.")
        }
    }
    Ok(())
}

fn version(as_json: bool) -> Result<()> {
    let version = VersionOutput::current();
    if as_json {
        print_json(&version);
    } else {
        println!(
            "cupola {} ({} {})",
            version.version, version.platform, version.arch
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- service

#[cfg(all(unix, not(target_os = "macos")))]
const UNIT_NAME: &str = "wonder.service";
#[cfg(all(unix, not(target_os = "macos")))]
fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME")?))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn unit_path() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home()?.join(".config"),
    };
    Ok(config.join("systemd/user").join(UNIT_NAME))
}

/// Where `wonder` and `wonderd` are linked for PATH (`~/.local/bin`).
#[cfg(all(unix, not(target_os = "macos")))]
fn bin_dir() -> Result<PathBuf> {
    match std::env::var_os("WONDER_BIN_DIR") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(installation_receipt()
            .and_then(|r| r["binDir"].as_str().map(PathBuf::from))
            .unwrap_or(home()?.join(".local/bin"))),
    }
}

/// The `wonder` the service runs: the PATH link when it is this binary, so a
/// later `wonder update` switches the service too; otherwise this binary.
#[cfg(all(unix, not(target_os = "macos")))]
fn service_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let link = bin_dir()?.join("cupola");
    let same = wonderd::canonical_path::canonicalize(&link).ok()
        == wonderd::canonical_path::canonicalize(&exe).ok();
    Ok(if same { link } else { exe })
}

/// One quoted unit-file word. `exec` also escapes `$`, which ExecStart expands.
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
fn unit_quote(value: &str, exec: bool) -> Result<String> {
    if value.contains(['\n', '\r']) {
        return Err("A path or setting contains a line break, which systemd cannot use.".into());
    }
    let mut quoted = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    if exec {
        quoted = quoted.replace('$', "$$");
    }
    Ok(format!("\"{quoted}\""))
}

#[cfg(all(unix, any(not(target_os = "macos"), test)))]
fn unit_file(exe: &Path, log: &Path, env: &[(String, String)]) -> Result<String> {
    let log = log.to_string_lossy();
    if log.contains(['\n', '\r']) {
        return Err("The log path contains a line break, which systemd cannot use.".into());
    }
    let log = log.replace('%', "%%");
    let mut text = String::from(
        "# Written by `cupola service install`; remove with `cupola service uninstall`.\n\
         [Unit]\n\
         Description=Cupola host for your iPhone\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n",
    );
    text += &format!(
        "ExecStart={} start --foreground\n",
        unit_quote(&exe.to_string_lossy(), true)?
    );
    for (key, value) in env {
        text += &format!(
            "Environment={}\n",
            unit_quote(&format!("{key}={value}"), false)?
        );
    }
    text += &format!("StandardOutput=append:{log}\nStandardError=append:{log}\n");
    // The supervisor stops its own children on SIGTERM; anything left after
    // the timeout is killed.
    text += &format!(
        "Restart=on-failure\n\
         RestartSec=5\n\
         KillMode=mixed\n\
         TimeoutStopSec={}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        HOST_STOP.as_secs()
    );
    Ok(text)
}

/// `unit` with ExecStart pointed at `exe`, or `None` when it already is. A
/// unit installed from a build folder pins that binary, which an update's
/// switched links would not reach.
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
fn retarget_unit(unit: &str, exe: &Path) -> Result<Option<String>> {
    let exec = format!(
        "ExecStart={} start --foreground",
        unit_quote(&exe.to_string_lossy(), true)?
    );
    let mut found = false;
    let mut changed = false;
    let mut text = String::with_capacity(unit.len());
    for line in unit.split_inclusive('\n') {
        if line.starts_with("ExecStart=") {
            found = true;
            changed |= line.trim_end() != exec;
            text += &exec;
            text.push('\n');
        } else {
            text += line;
        }
    }
    if !found {
        return Err("The Cupola service file has no ExecStart line. Run \
                    `cupola service install` again."
            .into());
    }
    Ok(changed.then_some(text))
}

/// `unit` with its log lines moved from `legacy` to `target`, or `None` when
/// none names `legacy`.
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
fn relocate_unit(unit: &str, legacy: &Path, target: &Path) -> Option<String> {
    let old = format!("append:{}/", legacy.to_string_lossy().replace('%', "%%"));
    let new = format!("append:{}/", target.to_string_lossy().replace('%', "%%"));
    let mut changed = false;
    let mut text = String::with_capacity(unit.len());
    for line in unit.split_inclusive('\n') {
        if (line.starts_with("StandardOutput=") || line.starts_with("StandardError="))
            && line.contains(&old)
        {
            changed = true;
            text += &line.replacen(&old, &new, 1);
        } else {
            text += line;
        }
    }
    changed.then_some(text)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn relocate_unit_file(legacy: &Path, target: &Path) -> Result<()> {
    let unit = unit_path()?;
    let Ok(text) = std::fs::read_to_string(&unit) else {
        return Ok(());
    };
    if let Some(text) = relocate_unit(&text, legacy, target) {
        let tmp = unit.with_extension("tmp");
        write_private(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, &unit)?;
        // The running service keeps its open log; systemd reads the file
        // again for the next start.
        let _ = systemctl(&["daemon-reload"]);
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| -> Box<dyn std::error::Error> {
            Box::new(Unavailable(
                "systemd was not found. Run `cupola start` instead, or start \
                 `cupola start --foreground` from your own service manager."
                    .into(),
            ))
        })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn systemctl_ok(args: &[&str]) -> Result<()> {
    let output = systemctl(args)?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    if detail.contains("Failed to connect to bus") || detail.contains("No medium found") {
        return Err(Box::new(Unavailable(
            "Your user's systemd is not reachable from this shell. Sign in to this \
             account directly (not with `su`), then retry."
                .into(),
        )));
    }
    Err(format!("systemctl --user {} failed: {detail}", args.join(" ")).into())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn service_installed() -> bool {
    unit_path().is_ok_and(|path| path.is_file())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn service_running() -> bool {
    service_installed()
        && systemctl(&["is-active", "--quiet", UNIT_NAME]).is_ok_and(|o| o.status.success())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn service_pid() -> Option<u32> {
    let output = systemctl(&["show", "--property=MainPID", "--value", UNIT_NAME]).ok()?;
    let pid: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    (pid != 0).then_some(pid)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn lingering() -> Option<bool> {
    // logind keeps one file per lingering user here; `loginctl show-user`
    // fails for a user with no session, which says nothing either way.
    let dir = Path::new("/var/lib/systemd/linger");
    if !Path::new("/var/lib/systemd").is_dir() {
        return None;
    }
    let uid = unsafe { libc::getuid() };
    let name = unsafe {
        let entry = libc::getpwuid(uid);
        if entry.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*entry).pw_name)
            .to_string_lossy()
            .into_owned()
    };
    Some(dir.join(name).exists())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn service_state() -> ServiceOutput {
    let installed = service_installed();
    ServiceOutput {
        manager: "systemd",
        installed,
        running: service_running(),
        starts_at_boot: Some(installed && lingering() == Some(true)),
    }
}

/// Marks that `service install` turned lingering on, so uninstall turns
/// off only what it turned on.
#[cfg(all(unix, not(target_os = "macos")))]
fn linger_marker(paths: &Paths) -> PathBuf {
    paths.state.join("service-enabled-linger")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn wait_for_service(running: bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if service_running() == running {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

#[cfg(all(unix, not(target_os = "macos")))]
fn service(command: ServiceCommand, as_json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    let mut note = None;
    match command {
        ServiceCommand::Install => {
            if load_config(&paths)?["tunnel"].as_str() == Some("tailscale") {
                tailscale_serve_permission()?;
            }
            if running_supervisor(&paths).is_some() && !service_running() {
                return Err("The host is already running in the background. Run \
                            `cupola stop` first so the service can start it."
                    .into());
            }
            persist_computer_backend(&paths)?;
            let environment =
                service_environment::collect(std::env::vars(), &service_data_dir(&paths));
            write_private(
                &paths.state.join("launch-environment.json"),
                &serde_json::to_vec(&environment)?,
            )?;
            let env: Vec<_> = environment
                .into_iter()
                .map(|(key, value)| (key, value.as_str().unwrap().to_owned()))
                .collect();
            std::fs::create_dir_all(paths.logs())?;
            let text = unit_file(&service_exe()?, &log_path(&paths), &env)?;
            let unit = unit_path()?;
            let changed = std::fs::read_to_string(&unit).ok().as_deref() != Some(text.as_str());
            let was_running = service_running();
            if changed {
                std::fs::create_dir_all(unit.parent().ok_or("no unit folder")?)?;
                let tmp = unit.with_extension("tmp");
                write_private(&tmp, text.as_bytes())?;
                std::fs::rename(&tmp, &unit)?;
            }
            systemctl_ok(&["daemon-reload"])?;
            systemctl_ok(&["enable", "--now", UNIT_NAME])?;
            if changed && was_running {
                systemctl_ok(&["restart", UNIT_NAME])?;
            }
            if lingering() == Some(false) {
                let enabled = Command::new("loginctl")
                    .arg("enable-linger")
                    .stdin(Stdio::null())
                    .output()
                    .is_ok_and(|o| o.status.success());
                if enabled {
                    write_private(&linger_marker(&paths), b"1")?;
                } else {
                    note = Some(format!(
                        "It starts when you sign in. To start it at boot as well, ask an \
                         administrator to run `sudo loginctl enable-linger {}`.",
                        std::env::var("USER").unwrap_or_else(|_| "$USER".into())
                    ));
                }
            }
            wait_for_service(true);
        }
        ServiceCommand::Uninstall => {
            if service_installed() {
                systemctl_ok(&["disable", "--now", UNIT_NAME])?;
                std::fs::remove_file(unit_path()?)?;
                systemctl_ok(&["daemon-reload"])?;
                let _ = systemctl(&["reset-failed", UNIT_NAME]);
                // The route keeps pointing at whatever listens on the port next.
                if running_supervisor(&paths).is_none() {
                    if let Err(error) = load_config(&paths).and_then(|c| tailscale_unpublish(&c)) {
                        note = Some(format!(
                            "The Tailscale route was left in place ({error}). Run `cupola unpublish` once Tailscale is back."
                        ));
                    }
                }
            }
            if linger_marker(&paths).exists() {
                let output = Command::new("loginctl")
                    .arg("disable-linger")
                    .stdin(Stdio::null())
                    .output();
                let disabled =
                    output.as_ref().is_ok_and(|o| o.status.success()) || lingering() == Some(false);
                if !disabled {
                    // Keep the marker so the next uninstall retries.
                    let reason = match &output {
                        Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_owned(),
                        Err(error) => error.to_string(),
                    };
                    return Err(format!(
                        "Removed the Cupola service, but could not turn off starting at boot \
                         ({reason}). Run `cupola service uninstall` again, or ask an \
                         administrator to run `sudo loginctl disable-linger {}`.",
                        std::env::var("USER").unwrap_or_else(|_| "$USER".into())
                    )
                    .into());
                }
                std::fs::remove_file(linger_marker(&paths))?;
            }
        }
        ServiceCommand::Restart => {
            if !service_installed() {
                return Err("The service is not installed. Run `cupola service install`.".into());
            }
            systemctl_ok(&["restart", UNIT_NAME])?;
            wait_for_service(true);
        }
        ServiceCommand::Status => {}
    }
    let state = service_state();
    if as_json {
        print_json(&state);
        return Ok(());
    }
    match (command, &state) {
        (ServiceCommand::Uninstall, _) => {
            println!("Removed the Cupola service; the host no longer starts automatically.");
            println!("Start it by hand with `cupola start`.");
        }
        (
            _,
            ServiceOutput {
                installed: false, ..
            },
        ) => {
            println!("The Cupola service is not installed. Run `cupola service install`.")
        }
        (_, state) => {
            println!(
                "The Cupola service is installed and {}.",
                if state.running { "running" } else { "stopped" }
            );
            if state.starts_at_boot == Some(true) {
                println!("It starts when this computer boots, before anyone signs in.");
            }
            if !state.running {
                println!("See why with `cupola logs`.");
            }
        }
    }
    if let Some(note) = note {
        println!("{note}");
    }
    Ok(())
}

// ---------------------------------------------------------------- update

/// Homebrew owns its Cellar and Caskroom. A cask moves the signed app outside
/// the prefix, leaving a symlink at Caskroom/cupola/<version>/Wonder.app.
#[cfg(unix)]
fn homebrew_update_command(exe: &Path, prefixes: &[PathBuf]) -> Option<&'static str> {
    let exe = wonderd::canonical_path::canonicalize(exe).ok()?;
    for package in exe.ancestors() {
        if package.file_name().and_then(|name| name.to_str()) != Some("cupola") {
            continue;
        }
        match package.parent()?.file_name()?.to_str() {
            Some("Cellar") => return Some("brew upgrade cupola"),
            Some("Caskroom") => return Some("brew upgrade --cask cupola"),
            _ => {}
        }
    }
    for prefix in prefixes {
        let Ok(versions) = std::fs::read_dir(prefix.join("Caskroom/cupola")) else {
            continue;
        };
        for version in versions.flatten() {
            if wonderd::canonical_path::canonicalize(version.path().join("Cupola.app"))
                .is_ok_and(|app| exe.starts_with(app))
            {
                return Some("brew upgrade --cask cupola");
            }
        }
    }
    None
}

/// Check ownership before fetching a feed, creating a lock or pausing a host.
#[cfg(unix)]
fn ensure_self_update_installation() -> Result<()> {
    let mut prefixes: Vec<PathBuf> = ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if let Some(prefix) = std::env::var_os("HOMEBREW_PREFIX").filter(|p| !p.is_empty()) {
        prefixes.push(prefix.into());
    }
    if let Some(path) = std::env::var_os("PATH") {
        prefixes.extend(std::env::split_paths(&path).filter_map(|bin| {
            if bin.join("brew").is_file() {
                bin.parent().map(Path::to_owned)
            } else {
                None
            }
        }));
    }
    if let Some(command) = homebrew_update_command(&executable_path()?, &prefixes) {
        return Err(Box::new(Unavailable(format!(
            "Homebrew manages this Cupola installation. Use `{command}` to update it."
        ))));
    }
    Ok(())
}

/// `--channel` when given, else the channel this installation follows.
fn followed_channel(chosen: Option<Channel>) -> Channel {
    chosen.unwrap_or_else(|| receipt_channel(installation_receipt().as_ref()))
}

fn receipt_channel(receipt: Option<&Value>) -> Channel {
    match receipt.and_then(|r| r["channel"].as_str()) {
        Some("nightly") => Channel::Nightly,
        _ => Channel::Stable,
    }
}

/// Records the channel an update followed, so a plain `wonder update` keeps
/// following it. Best effort: the update itself stands either way.
#[cfg(all(unix, not(target_os = "macos")))]
fn remember_channel(channel: Channel) {
    if let Some(receipt) = installation_receipt() {
        remember_channel_in(receipt, channel);
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn remember_channel_in(mut receipt: Value, channel: Channel) {
    if receipt["channel"].as_str() == Some(channel.as_str()) {
        return;
    }
    receipt["channel"] = channel.as_str().into();
    let Some(prefix) = receipt["prefix"].as_str().map(PathBuf::from) else {
        return;
    };
    let text = serde_json::to_string_pretty(&receipt).unwrap_or_default() + "\n";
    if write_private(&prefix.join("install.json"), text.as_bytes()).is_err() {
        eprintln!(
            "Could not record the {} channel; pass --channel next time.",
            channel.as_str()
        );
    }
}

/// Only a receipt belonging to the directory this executable actually runs in
/// may supply install paths or a private feed. Never consult the current folder.
fn installation_receipt() -> Option<Value> {
    receipt_for_executable(&executable_path().ok()?)
}

fn receipt_for_executable(exe: &Path) -> Option<Value> {
    for parent in exe.ancestors().skip(1).take(6) {
        let bytes = std::fs::read(parent.join("install.json")).ok();
        let Some(bytes) = bytes else { continue };
        return receipt_in(&bytes, parent);
    }
    receipt_of_app(exe)
}

/// A receipt counts only when it names `dir` as its own prefix.
fn receipt_in(bytes: &[u8], dir: &Path) -> Option<Value> {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let receipt: Value = serde_json::from_slice(bytes).ok()?;
    let prefix = Path::new(receipt["prefix"].as_str()?);
    (receipt["owner"] == "wonder-cli-v1"
        && wonderd::canonical_path::canonicalize(prefix).ok()?
            == wonderd::canonical_path::canonicalize(dir).ok()?)
    .then_some(receipt)
}

/// install.sh puts Cupola.app in ~/Applications and keeps only the receipt in
/// its install folder, so no ancestor of the executable holds one. Take the
/// receipt from the install folder only when it names this very app.
fn receipt_of_app(exe: &Path) -> Option<Value> {
    let folders = [
        std::env::var_os("WONDER_INSTALL_DIR").filter(|dir| !dir.is_empty()),
        std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share/cupola").into()),
    ];
    receipt_of_app_in(exe, folders.into_iter().flatten().map(PathBuf::from))
}

fn receipt_of_app_in(exe: &Path, folders: impl Iterator<Item = PathBuf>) -> Option<Value> {
    let exe = wonderd::canonical_path::canonicalize(exe).ok()?;
    for dir in folders {
        let Ok(bytes) = std::fs::read(dir.join("install.json")) else {
            continue;
        };
        let Some(receipt) = receipt_in(&bytes, &dir) else {
            continue;
        };
        let app = receipt["app"].as_str()?;
        if wonderd::canonical_path::canonicalize(app).is_ok_and(|app| exe.starts_with(app)) {
            return Some(receipt);
        }
    }
    None
}

#[cfg(any(target_os = "macos", windows))]
struct PlatformRelease {
    version: String,
    manifest: Value,
    url: reqwest::Url,
    client: reqwest::Client,
}

#[cfg(any(target_os = "macos", windows))]
async fn platform_release(channel: Channel, version: Option<&str>) -> Result<PlatformRelease> {
    let feed = update_feed()?;
    let url = match version {
        Some(v) if valid_version(v) => feed.join(&format!("versions/{v}.json"))?,
        Some(_) => return Err("Invalid Cupola version.".into()),
        None => feed.join(&format!("{}.json", channel.as_str()))?,
    };
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 5 && trusted_url(attempt.url()).is_ok() {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()?;
    let manifest: Value = client
        .get(url.clone())
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let latest = manifest["version"]
        .as_str()
        .filter(|v| valid_version(v))
        .ok_or("Release feed has no valid version.")?
        .to_owned();
    if version.is_some_and(|requested| requested != latest) {
        return Err("The manifest does not match the requested version.".into());
    }
    Ok(PlatformRelease {
        version: latest,
        manifest,
        url,
        client,
    })
}

#[cfg(any(target_os = "macos", windows))]
impl PlatformRelease {
    async fn download(&self, to: &Path) -> Result<()> {
        let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let artifact = &self.manifest["artifacts"][&target];
        let sha = artifact["sha256"]
            .as_str()
            .filter(|h| {
                h.len() == 64
                    && h.bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            })
            .ok_or_else(|| format!("No SHA256-verified release for {target}."))?;
        let url = self
            .url
            .join(artifact["url"].as_str().ok_or("Missing release URL.")?)?;
        trusted_url(&url)?;
        download_verified(&self.client, &url, sha, to).await
    }
}

#[cfg(any(target_os = "macos", windows))]
fn unpack_platform_archive(archive: &Path, to: &Path) -> Result<()> {
    let listing = process::output(Command::new("tar").arg("-tf").arg(archive))?;
    if !listing.status.success() {
        return Err("Release archive cannot be read.".into());
    }
    for name in String::from_utf8(listing.stdout)?.lines() {
        if name.starts_with(['/', '\\'])
            || name.contains(':')
            || name.split(['/', '\\']).any(|p| p == "..")
        {
            return Err("Unsafe release archive path; nothing changed.".into());
        }
    }
    std::fs::create_dir_all(to)?;
    // Native bsdtar on macOS and Windows rejects symlink traversal on extraction.
    let unpacked = process::output(
        Command::new("tar")
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(to),
    )?;
    if !unpacked.status.success() {
        return Err("Release archive could not be extracted safely.".into());
    }
    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
enum UpdatePlan {
    Stopped,
    Restart { local: Box<Local>, request: Value },
    Deferred,
}

#[cfg(any(target_os = "macos", windows))]
async fn update_plan(paths: &Paths, yes: bool, as_json: bool) -> Result<UpdatePlan> {
    if running_supervisor(paths).is_none() {
        return Ok(UpdatePlan::Stopped);
    }
    update_running_plan(Local::connect()?, yes, as_json).await
}

#[cfg(any(target_os = "macos", windows))]
async fn update_running_plan(local: Local, yes: bool, as_json: bool) -> Result<UpdatePlan> {
    use std::io::IsTerminal;
    loop {
        let work = local.call("update.work", Value::Null).await.ok();
        if work
            .as_ref()
            .is_some_and(|w| w["blocksUpdate"].as_bool() == Some(true))
        {
            if !as_json {
                eprintln!("Update deferred until computer sharing and message sending finish.");
            }
            return Ok(UpdatePlan::Deferred);
        }
        let count = work
            .as_ref()
            .and_then(|w| w["running"].as_u64())
            .unwrap_or(1);
        let consented = if yes {
            None
        } else if count == 0 {
            Some(0)
        } else {
            if as_json || !std::io::stdin().is_terminal() {
                if !as_json {
                    eprintln!("Agents are working. Retry when they finish, or use --yes to pause and restart.");
                }
                return Ok(UpdatePlan::Deferred);
            }
            eprint!("Agents are working in {count} conversation(s). Pause and restart now? [y/N] ");
            std::io::stderr().flush()?;
            if !pairing_answer().await? {
                return Ok(UpdatePlan::Deferred);
            }
            Some(count)
        };
        match prepare_for_update(&local, consented).await {
            Ok(request) => {
                return Ok(UpdatePlan::Restart {
                    local: Box::new(local),
                    request,
                })
            }
            Err(error) if error.downcast_ref::<WorkStarted>().is_some() => {
                if as_json || !std::io::stdin().is_terminal() {
                    return Ok(UpdatePlan::Deferred);
                }
                continue;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(any(target_os = "macos", windows))]
fn verify_cli_version(exe: &Path, expected: &str) -> Result<()> {
    let result = process::output(Command::new(exe).args(["version", "--json"]))?;
    let reported: Value = serde_json::from_slice(&result.stdout).unwrap_or(Value::Null);
    if !result.status.success() || reported["version"].as_str() != Some(expected) {
        return Err(
            "Release executable does not run as the requested version; nothing changed.".into(),
        );
    }
    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
fn start_updated(exe: &Path) -> Result<()> {
    let result = start_command_output(Command::new(exe).args(["start", "--json"]))?;
    if !result.status.success() {
        return Err(format!(
            "Updated host could not start: {}",
            String::from_utf8_lossy(&result.stdout)
        )
        .into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn start_command_output(command: &mut Command) -> Result<process::Output> {
    // LaunchServices starts the Mac host outside the setup process group.
    process::output(command)
}

/// A Windows start command deliberately leaves a background host alive. Setup
/// commands' kill-on-close job would kill that host as soon as `start` returns.
/// Bound this short-lived launcher separately, without changing setup ownership.
#[cfg(windows)]
fn start_command_output(command: &mut Command) -> Result<process::Output> {
    use std::io::{Read, Seek, SeekFrom};
    use windows_sys::Win32::{
        Foundation::{
            GetLastError, SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        },
        System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
    };
    // A nested launch must not inherit the updater's caller pipes in addition
    // to its own redirected output, or that caller could wait on the live host.
    for std_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let handle = GetStdHandle(std_handle);
            if !handle.is_null()
                && handle != INVALID_HANDLE_VALUE
                && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) == 0
            {
                return Err(std::io::Error::from_raw_os_error(GetLastError() as i32).into());
            }
        }
    }
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let started = (|| -> Result<std::process::ExitStatus> {
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            let too_large =
                stdout.metadata()?.len() > 1024 * 1024 || stderr.metadata()?.len() > 1024 * 1024;
            if too_large || STOPPING.load(Ordering::SeqCst) || Instant::now() >= deadline {
                return Err(if too_large {
                    "Host start command returned too much output."
                } else {
                    "Host start command interrupted or timed out after 15 seconds."
                }
                .into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    if started.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let status = started?;
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.take(1024 * 1024 + 1).read_to_end(&mut out)?;
    stderr.take(1024 * 1024 + 1).read_to_end(&mut err)?;
    if out.len() > 1024 * 1024 || err.len() > 1024 * 1024 {
        return Err("Host start command returned too much output.".into());
    }
    Ok(process::Output {
        status,
        stdout: out,
        stderr: err,
    })
}

/// How long a replaced host has to run the new program and answer.
#[cfg(any(target_os = "macos", windows))]
const UPDATED_HOST_START: Duration = Duration::from_secs(60);
/// Then how long its agents get to come up before the update says they are
/// not ready yet.
#[cfg(any(target_os = "macos", windows))]
const UPDATED_AGENTS_START: Duration = Duration::from_secs(30);

/// Accepts an update once the host runs `exe` and answers; returns whether
/// any agent is ready too.
#[cfg(any(target_os = "macos", windows))]
async fn wait_updated_ready(paths: &Paths, exe: &Path) -> Result<bool> {
    let local = Local::connect()?;
    let host_runs = || {
        runs(
            running_supervisor(paths).and_then(|pid| u32::try_from(pid).ok()),
            exe,
        )
    };
    wait_updated_host(&local, host_runs, UPDATED_HOST_START, UPDATED_AGENTS_START).await
}

/// Waits up to `start` until the host's supervisor runs the new program
/// (`host_runs`) and the daemon answers this computer's own credential
/// (`host.status`): the replacement, not some other process on the port.
/// Agents that were down or are slow to start do not undo the update: it
/// then waits up to `agents` for any agent to be ready (`/readyz`) and says
/// whether one is, as long as the host keeps running the new program and,
/// with no agent ready, still answers at the end.
#[cfg(any(target_os = "macos", windows, test))]
async fn wait_updated_host(
    local: &Local,
    host_runs: impl Fn() -> std::result::Result<(), String>,
    start: Duration,
    agents: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + start;
    loop {
        let program = host_runs();
        if program.is_ok() && local.call("host.status", Value::Null).await.is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(match program {
                Err(why) => format!(
                    "the updated host did not start within {} seconds: {why}.",
                    start.as_secs()
                ),
                Ok(()) => format!(
                    "the updated host did not answer within {} seconds.",
                    start.as_secs()
                ),
            }
            .into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let url = format!("{}/readyz", local.base);
    // A host that dies meanwhile also has no agent ready; that undoes the
    // update rather than reading as agents still starting.
    let deadline = Instant::now() + agents;
    loop {
        if let Err(why) = host_runs() {
            return Err(format!("the updated host stopped right after starting: {why}.").into());
        }
        let ready = client
            .get(&url)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if ready {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            if local.call("host.status", Value::Null).await.is_err() {
                return Err("the updated host stopped answering right after starting.".into());
            }
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(any(target_os = "macos", windows))]
fn report_platform_update(output: &UpdateOutput, as_json: bool) {
    if as_json {
        print_json(output);
    } else if output.installed {
        println!(
            "Installed Cupola {}.{}",
            output.latest.as_deref().unwrap_or_default(),
            if output.agents_ready == Some(false) {
                " The host is running again, but no agent is ready yet; `cupola status` shows each one."
            } else if output.restarted {
                " The host is ready again."
            } else {
                " Start it with `cupola start`."
            }
        );
    } else if output.update_available {
        println!(
            "Cupola {} is available; no changes applied.",
            output.latest.as_deref().unwrap_or_default()
        );
    } else {
        println!("Cupola is up to date.");
    }
}

/// Where verified releases are unpacked, one folder per version.
#[cfg(all(unix, not(target_os = "macos")))]
fn versions_dir() -> Result<PathBuf> {
    match std::env::var_os("WONDER_INSTALL_DIR") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir).join("versions")),
        _ => Ok(installation_receipt()
            .and_then(|r| r["prefix"].as_str().map(PathBuf::from))
            .unwrap_or(home()?.join(".local/share/wonder"))
            .join("versions")),
    }
}

fn update_feed() -> Result<reqwest::Url> {
    let base = std::env::var("WONDER_UPDATE_URL")
        .ok()
        .filter(|base| !base.is_empty())
        .or_else(|| {
            installation_receipt()?
                .get("feed")?
                .as_str()
                .map(str::to_owned)
        })
        .ok_or_else(|| -> Box<dyn std::error::Error> {
            Box::new(Unavailable(
                "No Cupola release feed is published for this computer yet. Set \
                 WONDER_UPDATE_URL to a release folder, or rebuild from source as \
                 described in INSTALL.md."
                    .into(),
            ))
        })?;
    let url = reqwest::Url::parse(&format!("{}/", base.trim_end_matches('/')))
        .map_err(|_| "WONDER_UPDATE_URL is not a URL.")?;
    trusted_url(&url)?;
    Ok(url)
}

/// Releases come over HTTPS; plain HTTP only from this computer itself.
fn trusted_url(url: &reqwest::Url) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Release URLs must not contain credentials.".into());
    }
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if url.scheme() == "https" || (url.scheme() == "http" && loopback) {
        Ok(())
    } else {
        Err(format!("Refusing to download Cupola from {url}: releases must use HTTPS.").into())
    }
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// Orders `1.2.10` after `1.2.9`, and a release after its own pre-releases.
#[cfg(any(not(target_os = "macos"), test))]
fn version_key(version: &str) -> (Vec<u64>, bool, String) {
    let version = version.split('+').next().unwrap_or(version);
    let (core, pre) = version.split_once('-').unwrap_or((version, ""));
    let numbers = core.split('.').map(|n| n.parse().unwrap_or(0)).collect();
    (numbers, pre.is_empty(), pre.to_owned())
}

#[cfg(any(not(target_os = "macos"), test))]
fn newer(candidate: &str, current: &str) -> bool {
    version_key(candidate) > version_key(current)
}

async fn download_verified(
    client: &reqwest::Client,
    url: &reqwest::Url,
    sha256: &str,
    to: &Path,
) -> Result<()> {
    use sha2::Digest;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut response = client.get(url.clone()).send().await?.error_for_status()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(to)?;
    let mut hash = sha2::Sha256::new();
    while let Some(chunk) = response.chunk().await? {
        hash.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    if hex::encode(hash.finalize()) != sha256 {
        let _ = std::fs::remove_file(to);
        return Err(
            "The download did not match its published SHA-256, so nothing \
                    changed. Retry later; if it keeps happening, the release is damaged."
                .into(),
        );
    }
    Ok(())
}

/// Unpacks a verified archive and checks it is a runnable `wonder` of the
/// expected version. Returns the release folder, still under `staging`.
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
fn unpack_release(archive: &Path, staging: &Path, version: &str) -> Result<PathBuf> {
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging)?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(staging)
        .arg("--no-same-owner")
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        return Err("The release archive could not be unpacked.".into());
    }
    // Accept the files at the top or inside one folder.
    let mut release = staging.to_path_buf();
    if !release.join("cupola").is_file() {
        let entries: Vec<_> = std::fs::read_dir(staging)?.flatten().collect();
        if let [only] = entries.as_slice() {
            release = only.path();
        }
    }
    for name in ["cupola", "wonderd"] {
        if !release.join(name).is_file() {
            return Err(format!("The release has no `{name}`; nothing changed.").into());
        }
    }
    let output = Command::new(release.join("cupola"))
        .args(["version", "--json"])
        .stdin(Stdio::null())
        .output()?;
    let reported: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    if reported["version"].as_str() != Some(version) {
        return Err(format!(
            "The downloaded release does not run as Cupola {version} on this computer; \
             nothing changed."
        )
        .into());
    }
    Ok(release)
}

/// Moves a verified release into place at `release`. A folder of that
/// version that a running program (this command or the host) was started
/// from is used as it is, never replaced under it.
#[cfg(all(unix, not(target_os = "macos")))]
fn place_release(unpacked: &Path, release: &Path, in_use: &[PathBuf]) -> Result<()> {
    if release.exists() {
        if in_use.iter().any(|path| path.starts_with(release)) {
            let _ = std::fs::remove_dir_all(unpacked);
            return Ok(());
        }
        let name = release.file_name().ok_or("no release name")?;
        let aside = release.with_file_name(format!(
            ".replaced-{}-{}",
            std::process::id(),
            name.to_string_lossy()
        ));
        std::fs::rename(release, &aside)?;
        let placed = std::fs::rename(unpacked, release);
        if placed.is_err() {
            let _ = std::fs::rename(&aside, release);
        }
        let _ = std::fs::remove_dir_all(&aside);
        return Ok(placed?);
    }
    Ok(std::fs::rename(unpacked, release)?)
}

/// A source install copies `wonder` and `wonderd` into `bin`; replacing
/// them would delete the files a running host restarts its daemon from.
/// Moves such copies, whole, into their own release folder under `versions`
/// and links them back, so the switch has a release to keep and go back to.
/// Returns that folder.
#[cfg(all(unix, not(target_os = "macos")))]
fn adopt_copies(bin: &Path, versions: &Path) -> Result<Option<PathBuf>> {
    let copied = |name: &str| {
        std::fs::symlink_metadata(bin.join(name)).is_ok_and(|meta| meta.file_type().is_file())
    };
    if !copied("cupola") {
        return Ok(None);
    }
    let version = Command::new(bin.join("cupola"))
        .args(["version", "--json"])
        .stdin(Stdio::null())
        .output()
        .ok()
        .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
        .and_then(|value| value["version"].as_str().map(str::to_owned))
        .filter(|version| valid_version(version))
        .unwrap_or_else(|| "0".into());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let folder = versions.join(format!("{version}+copy.{stamp}"));
    std::fs::create_dir_all(&folder)?;
    let mut moved: Vec<&str> = Vec::new();
    for name in ["cupola", "wonderd"] {
        if !copied(name) {
            continue;
        }
        let result = std::fs::rename(bin.join(name), folder.join(name)).and_then(|_| {
            moved.push(name);
            let tmp = bin.join(format!(".{name}.update"));
            let _ = std::fs::remove_file(&tmp);
            std::os::unix::fs::symlink(folder.join(name), &tmp)?;
            std::fs::rename(&tmp, bin.join(name))
        });
        if let Err(error) = result {
            for name in moved {
                let _ = std::fs::rename(folder.join(name), bin.join(name));
            }
            let _ = std::fs::remove_dir_all(&folder);
            return Err(format!(
                "Could not move the copied {} into {} ({error}); nothing changed.",
                bin.join(name).display(),
                versions.display()
            )
            .into());
        }
    }
    Ok(Some(folder))
}

/// What selecting a release changed: where each `bin` link pointed and the
/// service file before, so a release that does not start can be undone.
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
struct Switch {
    bin: PathBuf,
    links: Vec<(&'static str, Option<PathBuf>)>,
    /// The service file and its text before ExecStart was pointed at the link.
    unit: Option<(PathBuf, String)>,
}

#[cfg(all(unix, any(not(target_os = "macos"), test)))]
impl Switch {
    /// Points `bin/<name>` at the release, each link replaced in one rename,
    /// and the service file at `unit` (when installed) at `bin/wonder`: a
    /// unit installed from a build folder pins a binary the links never
    /// reach. On failure everything is put back.
    fn apply(bin: &Path, release: &Path, unit: Option<&Path>) -> Result<Self> {
        std::fs::create_dir_all(bin)?;
        let mut switch = Switch {
            bin: bin.to_path_buf(),
            links: Vec::new(),
            unit: None,
        };
        let result = (|| -> Result<()> {
            for name in ["cupola", "wonderd"] {
                let previous = std::fs::read_link(bin.join(name)).ok();
                link(bin, name, &release.join(name))?;
                switch.links.push((name, previous));
            }
            if let Some(unit) = unit {
                let before = std::fs::read_to_string(unit)?;
                if let Some(text) = retarget_unit(&before, &bin.join("cupola"))? {
                    switch.unit = Some((unit.to_path_buf(), before));
                    replace_file(unit, &text)?;
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(switch),
            Err(error) => {
                let _ = switch.roll_back();
                Err(error)
            }
        }
    }

    /// Puts the links and service file back as they were, as far as it
    /// can; the first failure is returned.
    fn roll_back(&self) -> Result<()> {
        let mut result = Ok(());
        for (name, previous) in self.links.iter().rev() {
            let restored = match previous {
                Some(target) => link(&self.bin, name, target),
                None => match std::fs::remove_file(self.bin.join(name)) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
                    _ => Ok(()),
                },
            };
            result = result.and(restored);
        }
        if let Some((unit, text)) = &self.unit {
            result = result.and(replace_file(unit, text));
        }
        result
    }

    /// Puts everything back, then, when the service file starts the host
    /// through `bin/wonder`, points the links at the release the host ran
    /// (`running`): links can select a release installed with its restart
    /// declined, which never ran and is no way back.
    fn roll_back_to(&self, running: Option<&Path>, unit: Option<&Path>) -> Result<()> {
        self.roll_back()?;
        let (Some(running), Some(unit)) = (running, unit) else {
            return Ok(());
        };
        let wonder = self.bin.join("cupola");
        let through_links = retarget_unit(&std::fs::read_to_string(unit)?, &wonder)?.is_none();
        if !through_links
            || wonderd::canonical_path::canonicalize(&wonder)
                .ok()
                .as_deref()
                == Some(running)
        {
            return Ok(());
        }
        let release = running.parent().ok_or("the old program is unknown")?;
        if !["cupola", "wonderd"]
            .iter()
            .all(|name| release.join(name).is_file())
        {
            return Err(format!("{} is not a whole release", release.display()).into());
        }
        for name in ["cupola", "wonderd"] {
            link(&self.bin, name, &release.join(name))?;
        }
        Ok(())
    }

    /// The release folder `bin/wonder` selected before, the way back.
    fn previous_release(&self) -> Option<PathBuf> {
        let (_, target) = self.links.first()?;
        Some(target.as_deref()?.parent()?.to_path_buf())
    }
}

#[cfg(all(unix, any(not(target_os = "macos"), test)))]
fn link(bin: &Path, name: &str, target: &Path) -> Result<()> {
    let tmp = bin.join(format!(".{name}.update"));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, bin.join(name))?;
    Ok(())
}

#[cfg(all(unix, any(not(target_os = "macos"), test)))]
fn replace_file(path: &Path, text: &str) -> Result<()> {
    // The service file holds the selected provider credentials: never wider
    // than owner-only.
    write_private(path, text.as_bytes())
}

/// The program `pid` runs, read from the kernel so a link switched since
/// does not mislead.
#[cfg(all(unix, not(target_os = "macos")))]
fn program_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "macos")]
fn program_of(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer length is passed in bytes.
    let length =
        unsafe { libc::proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(std::ffi::OsString::from_vec(buffer).into())
}

#[cfg(windows)]
fn program_of(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is closed below; the size is in UTF-16 units.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut buffer = vec![0u16; 32 * 1024];
        let mut length = buffer.len() as u32;
        let named = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            buffer.as_mut_ptr(),
            &mut length,
        ) != 0;
        CloseHandle(process);
        named.then(|| std::ffi::OsString::from_wide(&buffer[..length as usize]).into())
    }
}

/// Whether the host's supervisor (`pid`) runs `exe`, in the person's words
/// when it does not.
fn runs(pid: Option<u32>, exe: &Path) -> std::result::Result<(), String> {
    let same = |running: &Path| {
        running == exe
            || wonderd::canonical_path::canonicalize(running).is_ok_and(|running| {
                wonderd::canonical_path::canonicalize(exe).is_ok_and(|exe| running == exe)
            })
    };
    match pid.and_then(program_of) {
        Some(running) if same(&running) => Ok(()),
        Some(running) => Err(format!(
            "it runs {} instead of {}",
            running.display(),
            exe.display()
        )),
        None => Err("it is not running".into()),
    }
}

/// Matches the daemon's refusal when replies started after the owner was
/// asked (`update_admission::WORK_STARTED`).
const WORK_STARTED: &str = "Agents started working after you were asked";

#[derive(Debug)]
struct WorkStarted;

impl std::fmt::Display for WorkStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Agents started working after you were asked.")
    }
}

impl std::error::Error for WorkStarted {}

/// How to restart a host that still runs another release than the one this
/// command belongs to (a declined or postponed restart after `wonder update`).
#[cfg(all(unix, not(target_os = "macos")))]
fn stale_host_restart(paths: &Paths) -> Option<&'static str> {
    let Ok(Some(host)) = host_release(paths) else {
        return None;
    };
    let mine = executable_path().ok()?.parent()?.to_path_buf();
    if wonderd::canonical_path::canonicalize(&host).ok()?
        == wonderd::canonical_path::canonicalize(mine).ok()?
    {
        return None;
    }
    Some(if service_running() {
        "cupola service restart"
    } else {
        "cupola stop` and `cupola start"
    })
}

/// The release directory the running host was started from, read from the
/// kernel so a link switched since does not mislead. `Err` when a host runs
/// but its release cannot be told; then nothing may be pruned.
#[cfg(all(unix, not(target_os = "macos")))]
fn host_release(paths: &Paths) -> std::result::Result<Option<PathBuf>, ()> {
    match running_supervisor(paths) {
        None => Ok(None),
        Some(pid) => u32::try_from(pid)
            .ok()
            .and_then(program_of)
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .map(Some)
            .ok_or(()),
    }
}

#[cfg(all(unix, any(not(target_os = "macos"), test)))]
fn prune(versions: &Path, mut keep: Vec<PathBuf>, host: std::result::Result<Option<PathBuf>, ()>) {
    // A host that runs from somewhere unknown (`Err`) keeps everything.
    if let Ok(host) = host {
        keep.extend(host);
        prune_versions(versions, &keep);
    }
}

/// What a release that did not start leaves to keep: `keep` without it,
/// plus the release the host ran before the switch (`ran`). That one stays
/// even when going back failed and no host runs now: the restored links
/// select it and the suggested restart starts it. `Err` (the host's release
/// could not be told) keeps everything.
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
fn keep_after_rollback(
    keep: Vec<PathBuf>,
    release: &Path,
    ran: std::result::Result<Option<PathBuf>, ()>,
) -> std::result::Result<Vec<PathBuf>, ()> {
    Ok(keep
        .into_iter()
        .filter(|path| path != release)
        .chain(ran?)
        .collect())
}

/// Held by one `wonder update` from download through pruning, so a second
/// one cannot switch the links or prune the release the first selected.
/// The kernel drops it when the updater exits, however it exits.
struct UpdateLock(#[allow(dead_code)] std::fs::File);

const UPDATE_RUNNING: &str =
    "Another `cupola update` is running. Let it finish, then run `cupola update` again.";

#[cfg(unix)]
fn lock_updates(versions: &Path) -> Result<UpdateLock> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(versions)?;
    // std opens with O_CLOEXEC, so a host this command restarts does not
    // inherit the lock.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(versions.join(".update.lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(UPDATE_RUNNING.into());
        }
        return Err(error.into());
    }
    Ok(UpdateLock(file))
}

#[cfg(windows)]
fn lock_updates(versions: &Path) -> Result<UpdateLock> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::create_dir_all(versions)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .share_mode(0)
        .open(versions.join(".update.lock"))
        .map_err(|error| -> Box<dyn std::error::Error> {
            if matches!(error.raw_os_error(), Some(32 | 33)) {
                UPDATE_RUNNING.into()
            } else {
                error.into()
            }
        })?;
    Ok(UpdateLock(file))
}

/// Task Scheduler has one task per user. A fixture/custom install must never
/// retarget a task whose executable or data directory belongs to another host.
#[cfg(windows)]
fn windows_owns_service(root: &Path, paths: &Paths) -> Result<bool> {
    let Some(task) = os::service_action()? else {
        return Ok(false);
    };
    let exe = PathBuf::from(task.command);
    let data = paths.data.to_string_lossy();
    let arguments = format!(
        "start --data-dir \"{}\"",
        data.trim_end_matches(['\\', '/'])
    );
    Ok(
        wonderd::canonical_path::canonicalize(exe).is_ok_and(|exe| exe.starts_with(root))
            && task.arguments == arguments,
    )
}

#[cfg(windows)]
fn windows_select_release(root: &Path, release: &Path) -> Result<()> {
    let name = release
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("Invalid release directory.")?;
    // The executing CMD file must remain immutable. CMD resumes reading it
    // after wonder.exe exits; rewriting it mid-command can execute a path suffix.
    let temporary = root.join(".current-version.new");
    std::fs::write(&temporary, format!("{name}\r\n"))?;
    std::fs::rename(temporary, root.join("current-version"))?;
    Ok(())
}

/// Starts the updated host. With the login task installed it runs the task, so
/// the host lands in the signed-in user's desktop session (computer view needs
/// one) wherever the update was typed, an SSH shell included.
#[cfg(windows)]
fn windows_start_host(service: bool, exe: &Path) -> Result<()> {
    if service {
        os::service_run()
    } else {
        start_updated(exe)
    }
}

#[cfg(windows)]
fn windows_retarget_service(exe: &Path) -> Result<()> {
    let result = process::output(Command::new(exe).args(["service", "install", "--json"]))?;
    if !result.status.success() {
        return Err("Could not retarget the login task; previous version will be restored.".into());
    }
    Ok(())
}

#[cfg(windows)]
async fn update_windows(
    check: bool,
    channel: Channel,
    version: Option<String>,
    yes: bool,
    as_json: bool,
) -> Result<()> {
    let feed = platform_release(channel, version.as_deref()).await?;
    let current = cli::RELEASE_VERSION;
    let available = if version.is_some() {
        feed.version != current
    } else {
        newer(&feed.version, current)
    };
    let mut output = UpdateOutput {
        channel,
        current: current.into(),
        latest: Some(feed.version.clone()),
        update_available: available,
        installed: false,
        restarted: false,
        agents_ready: None,
    };
    if !available || check {
        report_platform_update(&output, as_json);
        return Ok(());
    }
    let receipt = installation_receipt().ok_or_else(|| {
        Unavailable("Install with install.ps1 before self-updating this Windows build.".into())
    })?;
    let root = wonderd::canonical_path::canonicalize(Path::new(
        receipt["prefix"]
            .as_str()
            .ok_or("Invalid installation receipt.")?,
    ))?;
    if !root.join("current-version").is_file() {
        return Err(Unavailable(
            "Reinstall with the current install.ps1 to enable atomic Windows self-updates.".into(),
        )
        .into());
    }
    let versions = root.join("versions");
    let _lock = lock_updates(&versions)?;
    let stage = tempfile::Builder::new()
        .prefix(".update-")
        .tempdir_in(&versions)?;
    let archive = stage.path().join("download.zip");
    feed.download(&archive).await?;
    let unpacked = stage.path().join("payload");
    unpack_platform_archive(&archive, &unpacked)?;
    for name in ["cupola.exe", "wonderd.exe", "uninstall.ps1"] {
        if !unpacked.join(name).is_file() {
            return Err(format!("Release lacks {name}; nothing changed.").into());
        }
    }
    verify_cli_version(&unpacked.join("cupola.exe"), &feed.version)?;
    let paths = Paths::resolve()?;
    let service = windows_owns_service(&root, &paths)?;
    let plan = update_plan(&paths, yes, as_json).await?;
    if matches!(plan, UpdatePlan::Deferred) {
        report_platform_update(&output, as_json);
        return Ok(());
    }
    let running = matches!(plan, UpdatePlan::Restart { .. });
    let previous = executable_path()?
        .parent()
        .ok_or("Missing previous release.")?
        .to_owned();
    let release = versions.join(format!(
        "{}-{}",
        feed.version,
        uuid::Uuid::new_v4().simple()
    ));
    // No loaded .exe is replaced. Retain the previous directory for rollback.
    std::fs::rename(unpacked, &release)?;
    let exe = release.join("cupola.exe");
    let switched = (|| -> Result<()> {
        if running {
            stop_host(&paths)?;
        }
        windows_select_release(&root, &release)?;
        if service {
            windows_retarget_service(&exe)?;
        }
        Ok(())
    })();
    let applied = match switched {
        Ok(()) if running => match windows_start_host(service, &exe) {
            Ok(()) => wait_updated_ready(&paths, &exe).await.map(Some),
            Err(error) => Err(error),
        },
        other => other.map(|()| None),
    };
    let agents_ready = match applied {
        Ok(ready) => ready,
        Err(error) => {
            let _ = stop_host(&paths);
            windows_select_release(&root, &previous)?;
            if service {
                windows_retarget_service(&previous.join("cupola.exe"))?;
            }
            if running {
                let _ = windows_start_host(service, &previous.join("cupola.exe"));
            }
            if let UpdatePlan::Restart { local, request } = plan {
                let _ = local.call("update.cancel", request).await;
            }
            // The release that did not start is not needed to go back.
            let _ = std::fs::remove_dir_all(&release);
            return Err(format!("Update failed; previous release restored: {error}").into());
        }
    };
    output.installed = true;
    output.restarted = running;
    output.agents_ready = agents_ready;
    // Keep the new release and this command's own (the way back); a loaded
    // program that cannot be removed is simply left for the next update.
    prune_versions(&versions, &[release, previous, stage.path().to_owned()]);
    report_platform_update(&output, as_json);
    Ok(())
}

/// Keeps the new release, the ones to go back to (this command's and the
/// previous link's) and the one the host still runs; removes older ones and
/// what interrupted updates left.
#[cfg(any(all(unix, any(not(target_os = "macos"), test)), windows))]
fn prune_versions(versions: &Path, keep: &[PathBuf]) {
    let Ok(entries) = std::fs::read_dir(versions) else {
        return;
    };
    // `keep` may come from `current_exe()` (`C:\…`) while `versions` came from
    // a canonicalized root (`\\?\C:\…`); compare one spelling.
    let keep: Vec<PathBuf> = keep
        .iter()
        .map(|k| wonderd::canonical_path::simplified(k.clone()))
        .collect();
    for entry in entries.flatten() {
        let path = entry.path();
        let same_spelling = wonderd::canonical_path::simplified(path.clone());
        let name = entry.file_name().to_string_lossy().into_owned();
        // The caller holds the update lock, so these are left by an update
        // that was interrupted, not one in progress (`keep` names this
        // command's own staging folder).
        if [".download-", ".staging-", ".replaced-", ".update-"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
            && !keep.iter().any(|k| k.starts_with(&same_spelling))
        {
            let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        } else if path.is_dir()
            && !name.starts_with('.')
            && !keep.iter().any(|k| k.starts_with(&same_spelling))
        {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// `previous` comes from `current_exe()` (`C:\…`) while `versions` comes from
/// a canonicalized install root: the folder the update ran from must be
/// kept whichever way each is spelled.
#[cfg(test)]
#[test]
fn headless_update_keeps_the_previous_release_in_either_path_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let root = wonderd::canonical_path::canonicalize(dir.path()).unwrap();
    let make = |versions: &Path, name: &str| {
        let path = versions.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("cupola.exe"), b"x").unwrap();
        path
    };
    // On Windows `std` gives `\\?\C:\…`; elsewhere it is the same path.
    let verbatim = std::fs::canonicalize(&root).unwrap(); // canonical-path: verbatim on purpose
    for (versions_root, spelling) in [(&root, &verbatim), (&verbatim, &root), (&root, &root)] {
        let versions = versions_root.join("versions");
        let (old, previous, new) = (
            make(&versions, "1.0.0-aa"),
            make(&versions, "1.0.1-bb"),
            make(&versions, "1.0.2-cc"),
        );
        let stage = make(&versions, ".update-live");
        let respell = |path: &Path| spelling.join(path.strip_prefix(versions_root).unwrap());
        prune_versions(
            &versions,
            &[respell(&new), respell(&previous), respell(&stage)],
        );
        let case = format!(
            "versions {} kept as {}",
            versions.display(),
            spelling.display()
        );
        assert!(!old.exists(), "{case}: old release left");
        assert!(new.is_dir(), "{case}: new release removed");
        assert!(previous.is_dir(), "{case}: previous release removed");
        assert!(stage.is_dir(), "{case}: staging folder removed");
        std::fs::remove_dir_all(&versions).unwrap();
    }
}

/// Pauses the host's work durably for a restart and returns the request to
/// cancel with. `consented` is how many running replies the owner agreed to
/// pause (`None` for `--yes`); the daemon refuses with [`WorkStarted`] if
/// more run now, before pausing anything.
async fn prepare_for_update(local: &Local, consented: Option<u64>) -> Result<Value> {
    let request = uuid::Uuid::new_v4().to_string();
    let mut body = json!({ "requestId": request });
    if let Some(consented) = consented {
        body["consentedRunning"] = consented.into();
    }
    local
        .send("update.prepare", body.clone(), Duration::from_secs(60))
        .await
        .map_err(|error| -> Box<dyn std::error::Error> {
            match error.downcast_ref::<ApiError>() {
                Some(api) if api.status == Some(409) && api.message.starts_with(WORK_STARTED) => {
                    Box::new(WorkStarted)
                }
                _ => error,
            }
        })?;
    Ok(body)
}

/// How long a restarted host has to run its new program and answer.
#[cfg(all(unix, not(target_os = "macos")))]
const HOST_START: Duration = Duration::from_secs(60);

/// Restarts the host: through its service, or by stopping it and running
/// `program start`.
#[cfg(all(unix, not(target_os = "macos")))]
fn relaunch(paths: &Paths, service: bool, program: &Path) -> Result<()> {
    if service {
        // A service systemd gave up on restarting still restarts.
        let _ = systemctl(&["reset-failed", UNIT_NAME]);
        return systemctl_ok(&["restart", UNIT_NAME]);
    }
    stop_host(paths)?;
    let output = Command::new(program)
        .arg("start")
        .stdin(Stdio::null())
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_owned()
            .into())
    }
}

/// An error as a sentence that ends with a full stop.
#[cfg(all(unix, not(target_os = "macos")))]
fn sentence(error: &dyn std::fmt::Display) -> String {
    let text = error.to_string();
    if text.ends_with('.') {
        text
    } else {
        format!("{text}.")
    }
}

/// Waits until the host's supervisor runs `exe` and its daemon answers.
#[cfg(all(unix, not(target_os = "macos")))]
async fn wait_for_host(paths: &Paths, local: &Local, service: bool, exe: &Path) -> Result<()> {
    let deadline = Instant::now() + HOST_START;
    let mut started = None;
    loop {
        let pid = if service {
            service_pid()
        } else {
            running_supervisor(paths).and_then(|pid| u32::try_from(pid).ok())
        };
        let program = runs(pid, exe);
        if program.is_ok() && local.call("host.status", Value::Null).await.is_ok() {
            return Ok(());
        }
        // A host that ran `exe` and is gone again has failed; waiting for
        // the deadline would only keep the paused work waiting.
        match (started, pid) {
            (None, Some(pid)) if program.is_ok() => started = Some(pid),
            (Some(first), now) if now != Some(first) => {
                return Err("it stopped right after starting.".into())
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(match program {
                Err(why) => format!("after {} seconds {why}.", HOST_START.as_secs()),
                Ok(()) => format!("it did not answer within {} seconds.", HOST_START.as_secs()),
            }
            .into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn update(
    check: bool,
    channel: Channel,
    version: Option<String>,
    yes: bool,
    as_json: bool,
) -> Result<()> {
    ensure_self_update_installation()?;
    let feed = update_feed()?;
    let manifest_url = match &version {
        Some(version) if !valid_version(version) => {
            return Err(format!("`{version}` is not a Cupola version.").into())
        }
        Some(version) => feed.join(&format!("versions/{version}.json"))?,
        None => feed.join(&format!("{}.json", channel.as_str()))?,
    };
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 5 && trusted_url(attempt.url()).is_ok() {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()?;
    let manifest: Value = client
        .get(manifest_url.clone())
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| match error.status() {
            Some(reqwest::StatusCode::NOT_FOUND) => {
                format!("No such Cupola release at {manifest_url}.")
            }
            _ => format!("The release feed could not be reached ({manifest_url})."),
        })?
        .json()
        .await
        .map_err(|_| "The release feed answered with something that is not a release.")?;
    let latest = manifest["version"]
        .as_str()
        .filter(|v| valid_version(v))
        .ok_or("The release feed has no valid version.")?
        .to_owned();
    let current = cli::RELEASE_VERSION;
    let update_available = if version.is_some() {
        latest != current
    } else {
        newer(&latest, current)
    };
    let mut output = UpdateOutput {
        channel,
        current: current.to_owned(),
        latest: Some(latest.clone()),
        update_available,
        installed: false,
        restarted: false,
        agents_ready: None,
    };
    let report = |output: &UpdateOutput, lines: &[String]| {
        if as_json {
            print_json(output);
        } else {
            for line in lines {
                println!("{line}");
            }
        }
    };
    if !update_available {
        if !check && version.is_none() {
            remember_channel(channel);
        }
        let mut lines = vec![format!("Cupola {current} is up to date.")];
        if let Some(restart) = stale_host_restart(&Paths::resolve()?) {
            lines.push(format!(
                "The host still runs the release from before; run `{restart}` when work finishes."
            ));
        }
        report(&output, &lines);
        return Ok(());
    }
    if check {
        let command = if channel == followed_channel(None) {
            "cupola update".to_owned()
        } else {
            format!("cupola update --channel {}", channel.as_str())
        };
        report(
            &output,
            &[format!(
                "Cupola {latest} is available (this is {current}). Install it with `{command}`."
            )],
        );
        return Ok(());
    }
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let artifact = &manifest["artifacts"][&target];
    let sha256 = artifact["sha256"]
        .as_str()
        .filter(|h| h.len() == 64 && h.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')))
        .ok_or_else(|| format!("Cupola {latest} has no verified download for {target}."))?;
    let archive_url = manifest_url.join(
        artifact["url"]
            .as_str()
            .ok_or_else(|| format!("Cupola {latest} has no download for {target}."))?,
    )?;
    trusted_url(&archive_url)?;

    let versions = versions_dir()?;
    let _lock = lock_updates(&versions)?;
    let pid = std::process::id();
    let archive = versions.join(format!(".download-{pid}"));
    let staging = versions.join(format!(".staging-{pid}"));
    if !as_json {
        println!("Downloading Cupola {latest}...");
    }
    download_verified(&client, &archive_url, sha256, &archive).await?;
    let unpacked = unpack_release(&archive, &staging, &latest);
    let _ = std::fs::remove_file(&archive);
    let unpacked = unpacked.inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&staging);
    })?;
    let bin = bin_dir()?;
    let adopted = adopt_copies(&bin, &versions).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&staging);
    })?;
    let paths = Paths::resolve()?;
    // Read before anything is replaced: the host's release stays whole while
    // it runs, through a declined or failed restart, and is the way back.
    let host = host_release(&paths);
    let host_program = running_supervisor(&paths)
        .and_then(|pid| u32::try_from(pid).ok())
        .and_then(program_of);
    let running_from = wonderd::canonical_path::canonicalize(std::env::current_exe()?)?;
    let release = versions.join(&latest);
    let in_use: Vec<PathBuf> = [Some(running_from.clone()), host.clone().ok().flatten()]
        .into_iter()
        .flatten()
        .collect();
    let placed = place_release(&unpacked, &release, &in_use);
    let _ = std::fs::remove_dir_all(&staging);
    placed?;
    let unit = service_installed().then(unit_path).transpose()?;
    let switch = Switch::apply(&bin, &release, unit.as_deref()).map_err(|error| {
        format!("Could not switch to Cupola {latest}; nothing changed: {error}")
    })?;
    if switch.unit.is_some() {
        if let Err(error) = systemctl_ok(&["daemon-reload"]) {
            let _ = switch.roll_back();
            let _ = systemctl(&["daemon-reload"]);
            return Err(format!(
                "Could not point the service at Cupola {latest}, so nothing changed: {error}"
            )
            .into());
        }
    }
    output.installed = true;
    if version.is_none() {
        remember_channel(channel);
    }
    let mut keep = vec![release.clone(), running_from];
    keep.extend(switch.previous_release());
    keep.extend(adopted);
    let mut lines = vec![format!("Installed Cupola {latest} (was {current}).")];
    if switch.unit.is_some() {
        lines.push(format!(
            "The service ran a fixed copy of cupola; it now runs {}.",
            bin.join("cupola").display()
        ));
    }
    let later = if service_installed() {
        "`cupola service restart`"
    } else {
        "`cupola stop` and `cupola start`"
    };
    if running_supervisor(&paths).is_some() {
        let local = Local::connect()?;
        let interactive = !as_json && unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        loop {
            // An older host without this route counts as possibly busy.
            let work = local.call("update.work", Value::Null).await.ok();
            let running = work
                .as_ref()
                .and_then(|w| w["running"].as_u64())
                .unwrap_or(1);
            let blocked = work
                .as_ref()
                .is_some_and(|w| w["blocksUpdate"].as_bool() == Some(true));
            let consented = if blocked {
                lines.push(
                    "The host keeps running the old version until computer sharing ends or \
                     a message finishes sending."
                        .into(),
                );
                None
            } else if yes {
                Some(None)
            } else if running == 0 {
                Some(Some(0))
            } else if interactive {
                let question = match work {
                    Some(_) => format!(
                        "Agents are working in {running} conversation{}. Restart now? Their \
                         work pauses and continues after the restart. [y/N] ",
                        if running == 1 { "" } else { "s" }
                    ),
                    None => "Agents may be working. Restart now? [y/N] ".into(),
                };
                print!("{question}");
                std::io::stdout().flush()?;
                // Read on its own thread, so the daemon connection stays up.
                pairing_answer().await?.then_some(Some(running))
            } else {
                lines.push("Agents are working, so the host was not restarted.".into());
                None
            };
            let Some(consented) = consented else {
                lines.push(format!("Restart it with {later} when work finishes."));
                break;
            };
            let body = match prepare_for_update(&local, consented).await {
                Ok(body) => body,
                // Work started after the answer: ask again with the new count.
                Err(error) if error.downcast_ref::<WorkStarted>().is_some() => continue,
                Err(error) => {
                    prune(&versions, keep, host);
                    return Err(format!(
                        "Installed Cupola {latest}, but the host keeps running its old version: \
                         {error} Restart it with {later} when work finishes."
                    )
                    .into());
                }
            };
            let service = service_running();
            let exe = wonderd::canonical_path::canonicalize(release.join("cupola"))?;
            let started = match relaunch(&paths, service, &bin.join("cupola")) {
                Ok(()) => wait_for_host(&paths, &local, service, &exe).await,
                Err(error) => Err(error),
            };
            let Err(error) = started else {
                output.restarted = true;
                lines.push("Restarted the host on the new version.".into());
                break;
            };
            // The new release did not start: select the old one again and
            // restart the host on it. Its paused work resumes there.
            let back = switch
                .roll_back_to(host_program.as_deref(), unit.as_deref())
                .and_then(|()| {
                    if service_installed() {
                        systemctl_ok(&["daemon-reload"])?;
                    }
                    let program = host_program
                        .clone()
                        .or_else(|| wonderd::canonical_path::canonicalize(bin.join("cupola")).ok())
                        .ok_or("the old program is unknown")?;
                    relaunch(&paths, service, &program)?;
                    Ok(program)
                });
            let back = match back {
                Ok(program) => wait_for_host(&paths, &local, service, &program).await,
                Err(error) => Err(error),
            };
            let _ = local.call("update.cancel", body).await;
            if let Ok(keep) = keep_after_rollback(keep, &release, host) {
                prune(&versions, keep, host_release(&paths));
            }
            return Err(match back {
                Ok(()) => format!(
                    "Cupola {latest} did not start: {} The host went back to the version \
                     it ran before and keeps working; see why with `cupola logs`.",
                    sentence(&error)
                ),
                Err(back) => format!(
                    "Cupola {latest} did not start: {} Going back to the version it ran \
                     before failed too: {} Run {later}, and see why with `cupola logs`.",
                    sentence(&error),
                    sentence(&back)
                ),
            }
            .into());
        }
    }
    prune(&versions, keep, host);
    report(&output, &lines);
    Ok(())
}

#[cfg(test)]
mod distribution_contract_tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn selected_computer_backend_is_explicit_and_missing_native_never_falls_back() {
        assert_eq!(
            parse_computer_backend(None, false).unwrap(),
            ComputerBackend::Native
        );
        assert_eq!(
            parse_computer_backend(Some("python"), false).unwrap(),
            ComputerBackend::Python
        );
        assert_eq!(
            parse_computer_backend(Some("native"), false).unwrap(),
            ComputerBackend::Native
        );
        for name in ["", "NATIVE", "other", "native\n"] {
            assert!(parse_computer_backend(Some(name), false).is_err());
        }
        assert_eq!(
            parse_computer_backend(None, true).unwrap(),
            ComputerBackend::Python
        );
        assert_eq!(
            parse_computer_backend(Some("native"), true).unwrap(),
            ComputerBackend::Native
        );
        assert_eq!(
            parse_computer_backend(Some("python"), true).unwrap(),
            ComputerBackend::Python
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(format!(
            "wonder-computer-helper{}",
            std::env::consts::EXE_SUFFIX
        ));
        // A source installation without the archive manifest has native
        // sharing only when its helper is installed beside the binaries.
        assert!(packaged_native_unavailable(directory.path()));
        std::fs::write(&path, b"fixture").unwrap();
        assert!(!packaged_native_unavailable(directory.path()));
        std::fs::remove_file(&path).unwrap();
        let manifest = directory.path().join("computer-backends.json");
        std::fs::write(&manifest, br#"{"native":{"included":false}}"#).unwrap();
        assert!(packaged_native_unavailable(directory.path()));
        let message = native_computer_helper(directory.path())
            .unwrap_err()
            .to_string();
        assert!(message.contains("not included for this platform"));
        assert!(message.contains("WONDER_COMPUTER_BACKEND=python"));
        assert!(!message.contains("Reinstall"));
        for contents in [r#"{"native":{"included":true}}"#, "invalid", "{}"] {
            std::fs::write(&manifest, contents).unwrap();
            assert!(!packaged_native_unavailable(directory.path()));
            assert!(native_computer_helper(directory.path())
                .unwrap_err()
                .to_string()
                .contains("Reinstall"));
        }
        std::fs::write(&path, b"fixture").unwrap();
        assert_eq!(native_computer_helper(directory.path()).unwrap(), path);
    }

    // Contract: updates leave package-manager-owned files alone, including a
    // cask's renamed app outside its prefix. A standalone copy still updates.
    #[cfg(unix)]
    #[test]
    fn homebrew_ownership_follows_formula_and_moved_cask_without_claiming_other_copies() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().join("custom brew prefix");
        let exe = prefix.join("Cellar/cupola/1.2.3/libexec/cupola");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"formula").unwrap();
        let bin = prefix.join("bin/cupola");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        symlink(&exe, &bin).unwrap();
        assert_eq!(
            homebrew_update_command(&bin, &[]),
            Some("brew upgrade cupola")
        );
        assert_eq!(
            homebrew_update_command(&exe, &[]),
            Some("brew upgrade cupola")
        );

        let app = directory.path().join("Applications/Cupola CLI.app");
        let cli = app.join("Contents/MacOS/cupola");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&cli, b"cask").unwrap();
        let staged = prefix.join("Caskroom/cupola/1.2.3/Cupola.app");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        symlink(&app, &staged).unwrap();
        std::fs::remove_file(&bin).unwrap();
        symlink(&cli, &bin).unwrap();
        for invocation in [&bin, &cli, &staged.join("Contents/MacOS/cupola")] {
            assert_eq!(
                homebrew_update_command(invocation, std::slice::from_ref(&prefix)),
                Some("brew upgrade --cask cupola")
            );
        }
        let standalone = directory.path().join("standalone/cupola");
        std::fs::create_dir_all(standalone.parent().unwrap()).unwrap();
        std::fs::write(&standalone, b"standalone").unwrap();
        assert!(homebrew_update_command(&standalone, std::slice::from_ref(&prefix)).is_none());
        let other = prefix.join("Cellar/wonder-other/1/bin/cupola");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, b"unrelated package").unwrap();
        assert!(homebrew_update_command(&other, &[prefix]).is_none());
    }

    #[test]
    fn receipt_follows_the_executable_and_rejects_a_foreign_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("install");
        let exe = root.join("versions/1/cupola");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"fixture").unwrap();
        let receipt = json!({"owner":"wonder-cli-v1", "prefix":root, "feed":"https://private.example/releases/"});
        let mut bytes = vec![0xef, 0xbb, 0xbf]; // Windows PowerShell's UTF-8 BOM
        bytes.extend(serde_json::to_vec(&receipt).unwrap());
        std::fs::write(root.join("install.json"), bytes).unwrap();
        assert_eq!(
            receipt_for_executable(&exe).unwrap()["feed"],
            receipt["feed"]
        );
        let foreign = json!({"owner":"wonder-cli-v1", "prefix":directory.path()});
        std::fs::write(
            root.join("install.json"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();
        assert!(receipt_for_executable(&exe).is_none());
    }

    #[test]
    fn an_app_outside_the_install_folder_finds_the_receipt_that_names_it() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("install");
        let app = directory.path().join("Applications/Cupola.app");
        let other = directory.path().join("Applications/Other.app");
        let exe = app.join("Contents/MacOS/cupola");
        let stranger = other.join("Contents/MacOS/cupola");
        for path in [&exe, &stranger] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"fixture").unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        let receipt = json!({"owner":"wonder-cli-v1", "prefix":root, "app":app,
                             "feed":"https://private.example/releases/"});
        std::fs::write(root.join("install.json"), receipt.to_string()).unwrap();
        let found = receipt_of_app_in(&exe, [root.clone()].into_iter());
        let foreign = receipt_of_app_in(&stranger, [root].into_iter());
        assert_eq!(found.unwrap()["feed"], receipt["feed"]);
        assert!(foreign.is_none());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn an_update_channel_is_remembered_in_the_private_receipt() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("install");
        let exe = root.join("versions/1/cupola");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"fixture").unwrap();
        let receipt = json!({"owner": "wonder-cli-v1", "prefix": root, "channel": "stable",
                             "feed": "https://private.example/releases/"});
        write_private(&root.join("install.json"), receipt.to_string().as_bytes()).unwrap();
        assert_eq!(
            receipt_channel(receipt_for_executable(&exe).as_ref()),
            Channel::Stable
        );
        remember_channel_in(receipt_for_executable(&exe).unwrap(), Channel::Nightly);
        let saved = receipt_for_executable(&exe).unwrap();
        assert_eq!(receipt_channel(Some(&saved)), Channel::Nightly);
        assert_eq!(saved["feed"], receipt["feed"]);
        let mode = std::fs::metadata(root.join("install.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn archive_cannot_escape_before_a_downloaded_program_runs() {
        use base64::Engine;
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("bad.zip");
        // A real stored ZIP entry named ../escape, not a mocked extractor.
        let bytes = base64::engine::general_purpose::STANDARD.decode("UEsDBBQAAAAAAAAxUV2DFtyMAQAAAAEAAAAJAAAALi4vZXNjYXBleFBLAQIUAxQAAAAAAAAxUV2DFtyMAQAAAAEAAAAJAAAAAAAAAAAAAACAAQAAAAAuLi9lc2NhcGVQSwUGAAAAAAEAAQA3AAAAKAAAAAAA").unwrap();
        std::fs::write(&archive, bytes).unwrap();
        let error =
            unpack_platform_archive(&archive, &directory.path().join("payload")).unwrap_err();
        assert!(error.to_string().contains("Unsafe"), "{error}");
        assert!(!directory.path().join("escape").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_lock_is_released_when_the_updater_exits() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_updates(dir.path()).unwrap();
        assert!(lock_updates(dir.path()).is_err());
        drop(first);
        assert!(lock_updates(dir.path()).is_ok());
    }

    // Contract: exiting the short start command must leave its intentional
    // background host alive. The setup Job Object previously killed it, so a
    // real Windows update waited 60 seconds and also killed its rollback host.
    #[cfg(windows)]
    #[test]
    fn windows_updated_host_outlives_its_start_command() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("start-fixture.ps1");
        let pid_file = directory.path().join("pid");
        let token = uuid::Uuid::new_v4().simple().to_string();
        std::fs::write(
            &script,
            format!(
                "$p = Start-Process (Join-Path $PSHOME 'powershell.exe') -ArgumentList @('-NoProfile','-Command','Start-Sleep -Seconds 30 # {token}') -WindowStyle Hidden -PassThru\n[IO.File]::WriteAllText($args[0], [string]$p.Id)\n"
            ),
        )
        .unwrap();
        let output = start_command_output(
            Command::new("powershell.exe")
                .args(["-NoProfile", "-File"])
                .arg(&script)
                .arg(&pid_file),
        )
        .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Inspect and stop only the PID whose private fixture token still
        // matches. Never stop an unrelated process even if a PID was reused.
        let inspected = process::output(Command::new("powershell.exe").args([
            "-NoProfile",
            "-Command",
            &format!(
                "$p=Get-CimInstance Win32_Process -Filter 'ProcessId={pid}'; if ($p -and $p.CommandLine.Contains('{token}')) {{ Stop-Process -Id {pid}; 'alive' }} else {{ 'missing' }}"
            ),
        ]))
        .unwrap();
        assert!(inspected.status.success(), "{:?}", inspected.stderr);
        assert_eq!(String::from_utf8_lossy(&inspected.stdout).trim(), "alive");
    }

    #[cfg(windows)]
    #[test]
    fn windows_selection_replaces_one_marker_without_rewriting_the_running_launcher() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let launcher = root.join("bin/cupola.cmd");
        std::fs::write(&launcher, b"immutable launch fixture").unwrap();
        std::fs::write(root.join("current-version"), b"1.0.1\r\n").unwrap();
        windows_select_release(root, &root.join("versions/1.0.2")).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("current-version")).unwrap(),
            "1.0.2\r\n"
        );
        assert_eq!(
            std::fs::read(&launcher).unwrap(),
            b"immutable launch fixture"
        );
        windows_select_release(root, &root.join("versions/1.0.1")).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("current-version")).unwrap(),
            "1.0.1\r\n"
        );
    }
}

// ---------------------------------------------------------------- pairing

/// Asks the daemon for an offer. 503 means the connection address is still
/// coming up, or there is no Tailscale yet; a phone on the same Wi-Fi can
/// still pair then.
async fn pairing_offer(local: &Local) -> Result<Value> {
    let home = || async {
        let mut offer = local
            .call("pairing.offer", json!({ "route": "home" }))
            .await?;
        // Read by `pair_offer` only, to say where the phone must be.
        offer[HOME_OFFER] = json!(true);
        Ok::<_, Box<dyn std::error::Error>>(offer)
    };
    let home_on = match local.pair_via {
        // A home network link says for itself why it cannot be made.
        PairVia::Home => return home().await,
        PairVia::Main => None,
        PairVia::Any { home } => Some(home),
    };
    match local.call("pairing.offer", Value::Null).await {
        Err(error) if api_status(error.as_ref()) == Some(503) => match home_on {
            None => Err(
                "This computer's connection address isn't ready yet. Check `cupola status`.".into(),
            ),
            Some(false) => Err(
                "Tailscale is not ready yet, and same Wi-Fi connections are turned off. Turn \
                 them on with `cupola lan on` and restart the host, or check `cupola status`."
                    .into(),
            ),
            Some(true) => home().await.map_err(|error| {
                format!("Tailscale is not ready yet, and same Wi-Fi pairing failed: {error}").into()
            }),
        },
        offer => offer,
    }
}

/// Marks an offer whose link works only on this computer's home network.
const HOME_OFFER: &str = "cupolaHomeNetwork";

async fn cancel_offer(local: &Local, id: &str) -> Result<()> {
    match local.call("pairing.cancel", json!({ "offerId": id })).await {
        Ok(_) => Ok(()),
        Err(error) if not_found(error.as_ref()) => Ok(()),
        Err(error) => Err(error),
    }
}

async fn cancel_pair(id: &str, as_json: bool) -> Result<()> {
    cancel_pair_with_local(&Local::connect()?, &Paths::resolve()?, id).await?;
    if as_json {
        print_event(&PairEvent::Cancelled {
            offer_id: id.into(),
        });
    } else {
        println!("Pairing offer cancelled.");
    }
    Ok(())
}

async fn cancel_pair_with_local(local: &Local, paths: &Paths, id: &str) -> Result<()> {
    cancel_offer(local, id).await?;
    // The offer can belong to a different CLI process. Remove only the QR
    // registered for this exact offer after the daemon accepts cancellation.
    let record = pair_qr_record(paths, id);
    let bytes = match std::fs::read(&record) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let qr: Value = serde_json::from_slice(&bytes)?;
    if qr["offerId"].as_str() != Some(id) {
        return Err("pairing QR record does not match the cancelled offer".into());
    }
    remove_pair_file(Path::new(
        qr["qrPath"]
            .as_str()
            .ok_or("pairing QR record has no path")?,
    ))?;
    remove_pair_file(&record)
}

fn pair_qr_record(paths: &Paths, id: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    paths.state.join(format!(
        "pair-qr-{}.json",
        hex::encode(Sha256::digest(id.as_bytes()))
    ))
}

fn remove_pair_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// `pair --approve|--deny`: the non-interactive answer to the verification
/// prompt of a `pair` that is still waiting (possibly in another process).
/// Approval requires the verification text the person reads from their phone.
async fn decide_pair(
    device_id: &str,
    approve: bool,
    phone_code: Option<&str>,
    as_json: bool,
) -> Result<()> {
    let event = decide_with_local(&Local::connect()?, device_id, approve, phone_code).await?;
    match (&event, as_json) {
        (_, true) => print_event(&event),
        (PairEvent::Connected { label, .. }, false) => println!("Connected “{label}”."),
        (PairEvent::Rejected { label, .. }, false) => println!("Rejected “{label}”."),
        _ => {}
    }
    Ok(())
}

async fn decide_with_local(
    local: &Local,
    device_id: &str,
    approve: bool,
    phone_code: Option<&str>,
) -> Result<PairEvent> {
    let phone_code = if approve {
        Some(phone_code.filter(|code| !code.is_empty()).ok_or(
            "approval requires --code <TEXT>: ask the person to read the verification text from their phone",
        )?)
    } else {
        None
    };
    let pending = local.call("pairing.pending", Value::Null).await?;
    let device = pending
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["deviceId"].as_str() == Some(device_id))
        .ok_or_else(|| {
            format!("no phone is waiting as {device_id}; run `cupola pair` again if it timed out")
        })?;
    let label = device["label"].as_str().unwrap_or("A phone").to_owned();
    let action = if approve { "confirm" } else { "reject" };
    local
        .call(
            &format!("pairing.{action}"),
            if let Some(code) = phone_code {
                json!({"deviceId": device_id, "code": code})
            } else {
                json!({"deviceId": device_id})
            },
        )
        .await?;
    let device_id = device_id.to_owned();
    Ok(if approve {
        PairEvent::Connected { device_id, label }
    } else {
        PairEvent::Rejected { device_id, label }
    })
}

/// Writes the pairing link as a QR image: SVG for a `.svg` path, else PNG.
fn write_qr_file(path: &Path, url: &str) -> Result<()> {
    use qrcode::{Color, EcLevel, QrCode};
    let code = QrCode::with_error_correction_level(url.as_bytes(), EcLevel::M)?;
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
    {
        let svg = code
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(256, 256)
            .quiet_zone(true)
            .build();
        return write_new_qr(path, svg.as_bytes());
    }
    const SCALE: u32 = 8;
    const QUIET: u32 = 4;
    let width = code.width() as u32;
    let side = (width + 2 * QUIET) * SCALE;
    let colors = code.to_colors();
    let image = image::GrayImage::from_fn(side, side, |x, y| {
        let (mx, my) = (x / SCALE, y / SCALE);
        let dark = mx >= QUIET
            && my >= QUIET
            && mx < QUIET + width
            && my < QUIET + width
            && colors[((my - QUIET) * width + (mx - QUIET)) as usize] == Color::Dark;
        image::Luma([if dark { 0 } else { 255 }])
    });
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    write_new_qr(path, &png)
}

fn write_new_qr(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    // create-new is the ownership boundary: no existence-check/rename race,
    // and no predictable .tmp sibling that could belong to the person.
    let mut file = os::create_private(path).map_err(|error| {
        format!(
            "Cannot create QR {}; choose a new private path: {error}",
            path.display()
        )
    })?;
    let result = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result.map_err(Into::into)
}

/// Deletes the QR on claim/exit; its private record also lets a separate
/// `pair --cancel` process remove it without stopping this waiting process.
struct QrFile(Option<PathBuf>, Option<PathBuf>);

impl QrFile {
    fn create(paths: &Paths, id: &str, path: &Path, url: &str) -> Result<Self> {
        let path = std::path::absolute(path)?;
        write_qr_file(&path, url)?;
        // Register cleanup only after create-new succeeds; otherwise a failed
        // create could delete the existing file it refused to overwrite.
        let mut file = Self(Some(path.clone()), None);
        let record = pair_qr_record(paths, id);
        file.1 = Some(record.clone());
        write_private(
            &record,
            &serde_json::to_vec(&json!({"offerId": id, "qrPath": path}))?,
        )?;
        Ok(file)
    }

    fn clear(&mut self) -> Result<()> {
        for path in [&mut self.0, &mut self.1] {
            if let Some(value) = path.as_ref() {
                remove_pair_file(value)?;
                *path = None;
            }
        }
        Ok(())
    }
}

impl Drop for QrFile {
    fn drop(&mut self) {
        let _ = self.clear();
    }
}

fn pairing_verification(device: &Value) -> PairEvent {
    PairEvent::Verification {
        device_id: device["deviceId"].as_str().unwrap_or_default().into(),
        label: device["label"].as_str().unwrap_or("A phone").into(),
        host_installation_id: device["hostInstallationId"]
            .as_str()
            .unwrap_or_default()
            .into(),
    }
}

async fn pair(
    assume_yes: bool,
    timeout: Option<u64>,
    as_json: bool,
    watch_stdin: bool,
    via: PairVia,
) -> Result<()> {
    if watch_stdin {
        // SSH relays EOF even when its remote command survives a disconnect.
        // The agent holds this pipe open for the lifetime of its own offer.
        std::thread::spawn(|| {
            let mut buffer = [0u8; 128];
            while std::io::stdin().read(&mut buffer).is_ok_and(|n| n > 0) {}
            STOPPING.store(true, Ordering::SeqCst);
        });
    }
    os::install_pair_stop_handlers();
    let mut local = Local::connect()?;
    let interactive = !as_json
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && !assume_yes
        && !watch_stdin;
    local.pair_interactive = interactive;
    local.pair_via = via;
    // Automation has no terminal answer to read. --watch-stdin owns its pipe
    // solely for cancellation; it must never race an approval reader.
    let mut console =
        interactive.then(|| pairing_console(std::io::BufReader::new(std::io::stdin())));
    let mut answer = || match console.as_mut() {
        Some(console) => console(),
        None => Box::pin(std::future::pending()) as PairingCodeAnswer,
    };
    pair_until(
        &local,
        assume_yes,
        timeout.map(|t| Instant::now() + Duration::from_secs(t)),
        as_json,
        &mut std::io::stdout(),
        &mut answer,
        &|| STOPPING.load(Ordering::SeqCst),
    )
    .await
}

/// The whole pair command: origin wait, offer and the phone's approval all
/// end at one `deadline` (`--timeout`).
async fn pair_until(
    local: &Local,
    assume_yes: bool,
    deadline: Option<Instant>,
    as_json: bool,
    output: &mut impl Write,
    answer: &mut dyn FnMut() -> PairingCodeAnswer,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
    // A home network link does not go through the main route.
    let main_route =
        (local.pair_via != PairVia::Home).then(|| local.call("host.status", Value::Null));
    if let Some(Ok(host)) = match main_route {
        Some(status) => Some(by(deadline, status).await),
        None => None,
    } {
        if let Some(origin) = host["publicOrigin"].as_str() {
            let limit = left().map_or(ORIGIN_WAIT, |left| ORIGIN_WAIT.min(left));
            if !wait_for_origin(origin, as_json, limit).await && left().is_some_and(|l| l.is_zero())
            {
                return Err(format!(
                    "{origin} did not answer before the timeout; run `cupola pair` again"
                )
                .into());
            }
        }
    }
    pair_with_local(
        local, assume_yes, deadline, as_json, output, answer, cancelled,
    )
    .await
}

/// Runs `work`, giving up at `deadline` if there is one.
async fn by<T>(
    deadline: Option<Instant>,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.into(), work)
            .await
            .unwrap_or_else(|_| Err("pairing timed out; run `cupola pair` again".into())),
        None => work.await,
    }
}

/// A phone reaches the host only through its origin. Right after the first
/// `wonder service install`, Tailscale is still issuing the HTTPS certificate
/// (measured 34 s on a fresh Ubuntu VM), and the phone's first request would
/// hang on it, so wait before showing a code. Any HTTP answer counts.
/// Returns whether the origin answered within `limit`.
async fn wait_for_origin(origin: &str, as_json: bool, limit: Duration) -> bool {
    let Ok(client) = reqwest::Client::builder().build() else {
        return false;
    };
    let url = format!("{origin}/healthz");
    let deadline = Instant::now() + limit;
    let mut said = false;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let request = client.get(&url).timeout(left.min(Duration::from_secs(5)));
        if !left.is_zero() && request.send().await.is_ok() {
            return true;
        }
        if STOPPING.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return false;
        }
        if !said && !as_json {
            eprintln!(
                "Waiting for {origin} to answer. The first time, Tailscale takes up to a \
                 minute to issue its HTTPS certificate…"
            );
            said = true;
        }
        tokio::time::sleep(Duration::from_secs(1).min(left)).await;
    }
}

const ORIGIN_WAIT: Duration = Duration::from_secs(120);

/// Reads the typed answer on its own thread and awaits it, so the daemon
/// connection keeps answering pings however long the person takes.
fn pairing_answer() -> PairingAnswer {
    let (send, receive) = tokio::sync::oneshot::channel();
    // A detached stdin reader cannot hold up offer cancellation. This process
    // ends after pairing; no later command can reuse its blocked stdin reader.
    std::thread::spawn(move || {
        let mut answer = String::new();
        let result = std::io::stdin().read_line(&mut answer).map(|_| answer);
        let _ = send.send(result);
    });
    Box::pin(wait_for_pairing_answer(receive, || {
        STOPPING.load(Ordering::SeqCst)
    }))
}

async fn wait_for_pairing_answer(
    mut receive: tokio::sync::oneshot::Receiver<std::io::Result<String>>,
    cancelled: impl Fn() -> bool,
) -> Result<bool> {
    loop {
        if cancelled() {
            return Err("pairing cancelled".into());
        }
        if let Ok(answer) = tokio::time::timeout(Duration::from_millis(50), &mut receive).await {
            return Ok(matches!(answer??.trim(), "y" | "Y" | "yes" | "Yes"));
        }
    }
}

/// One reader for the entire offer. A cancelled prompt drops only its
/// receiver future: the next prompt can still receive the person's next line.
/// EOF is absence of a decision, never denial; --approve/--deny remain usable.
fn pairing_console(
    reader: impl std::io::BufRead + Send + 'static,
) -> impl FnMut() -> PairingCodeAnswer {
    let (send, receive) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut reader = reader;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if send.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = send.send(Err(error));
                    break;
                }
            }
        }
    });
    let receive = std::sync::Arc::new(tokio::sync::Mutex::new(receive));
    move || {
        let receive = receive.clone();
        Box::pin(async move {
            let answer = receive.lock().await.recv().await;
            match answer {
                Some(line) => {
                    let line = line?.trim().to_owned();
                    Ok(if matches!(line.as_str(), "" | "n" | "N" | "no" | "No") {
                        None
                    } else {
                        Some(line)
                    })
                }
                None => std::future::pending().await,
            }
        })
    }
}

/// A pending yes/no answer to the pairing prompt.
type PairingAnswer = futures_util::future::LocalBoxFuture<'static, Result<bool>>;
/// No line at EOF is no decision. An explicit blank/no rejects; otherwise the
/// terminal supplies phone text for the daemon to check, never a blind yes.
type PairingCodeAnswer = futures_util::future::LocalBoxFuture<'static, Result<Option<String>>>;

async fn pair_with_local(
    local: &Local,
    assume_yes: bool,
    deadline: Option<Instant>,
    as_json: bool,
    output: &mut impl Write,
    answer: &mut dyn FnMut() -> PairingCodeAnswer,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let offer = by(deadline, pairing_offer(local)).await?;
    let id = offer["offerId"].as_str().ok_or("offer has no ID")?;
    let result = pair_offer(
        local,
        &offer,
        assume_yes,
        deadline,
        as_json,
        output,
        PairingInput { answer, cancelled },
    )
    .await;
    // Every exit after creation closes this exact offer, including malformed
    // responses, a failed phone, timeout and SIGINT/SIGTERM. EOF alone waits
    // for the person's explicit decision instead of ending the offer.
    let cleanup = cancel_offer(local, id).await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (result, Err(cleanup)) => Err(format!(
            "{}; could not cancel pairing offer {id}: {cleanup}",
            result
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Phone approved".into())
        )
        .into()),
    }
}

struct PairingInput<'a> {
    answer: &'a mut dyn FnMut() -> PairingCodeAnswer,
    cancelled: &'a dyn Fn() -> bool,
}

async fn pair_offer(
    local: &Local,
    offer: &Value,
    assume_yes: bool,
    timeout: Option<Instant>,
    as_json: bool,
    output: &mut impl Write,
    input: PairingInput<'_>,
) -> Result<()> {
    let PairingInput { answer, cancelled } = input;
    let url = offer["url"].as_str().ok_or("offer has no URL")?;
    let code = offer["humanCode"].as_str().unwrap_or("");
    let expires_at = offer["expiresAtMs"].as_u64().unwrap_or(0);
    let origin = url.split("/pair").next().unwrap_or(url);
    let mut qr_file = match PAIR_QR.get() {
        Some(path) => QrFile::create(
            &Paths::resolve()?,
            offer["offerId"].as_str().ok_or("offer has no ID")?,
            path,
            url,
        )?,
        None => QrFile(None, None),
    };
    let qr_path = qr_file.0.as_ref().map(|p| {
        std::path::absolute(p)
            .unwrap_or_else(|_| p.clone())
            .display()
            .to_string()
    });
    if as_json {
        write_event(
            output,
            &PairEvent::Offer {
                offer_id: offer["offerId"].as_str().unwrap_or_default().to_owned(),
                url: url.to_owned(),
                origin: origin.to_owned(),
                code: code.to_ascii_uppercase(),
                expires_at_ms: expires_at,
                qr_path,
                host_key_pin: offer["hostKeyPin"].as_str().map(str::to_owned),
            },
        )?;
    } else {
        let qr = qrcode::QrCode::new(url.as_bytes())?
            .render::<qrcode::render::unicode::Dense1x2>()
            .dark_color(qrcode::render::unicode::Dense1x2::Light)
            .light_color(qrcode::render::unicode::Dense1x2::Dark)
            .quiet_zone(true)
            .build();
        let qr_image = qr_path
            .as_ref()
            .map(|p| format!("QR image: {p}\n"))
            .unwrap_or_default();
        if offer[HOME_OFFER] == json!(true) {
            // The app trusts a home network address only with the key the
            // QR code and link carry, so a typed code cannot work here.
            writeln!(output, "{qr}\nScan in Cupola > Add computer, or paste this pairing link there: {url}\n{qr_image}Your iPhone must be on the same Wi-Fi as this computer.\nWaiting for the phone… (Ctrl-C to cancel)")?;
        } else {
            writeln!(output, "{qr}\nScan in Cupola > Add computer, or enter:\n\n    Address: {origin}\n    Code:    {}\n\nOr paste this pairing link in Add computer: {url}\n{qr_image}Waiting for the phone… (Ctrl-C to cancel)", code.to_ascii_uppercase())?;
        }
        output.flush()?;
    }
    let deadline = timeout.unwrap_or_else(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Instant::now() + Duration::from_secs(expires_at.saturating_sub(now) / 1000 + 60)
    });
    let mut asked = std::collections::HashSet::new();
    while Instant::now() < deadline {
        if cancelled() {
            return Err("pairing cancelled".into());
        }
        let pending = by(Some(deadline), local.call("pairing.pending", Value::Null)).await?;
        'devices: for device in pending.as_array().into_iter().flatten() {
            if cancelled() {
                return Err("pairing cancelled".into());
            }
            if device["offerId"] != offer["offerId"] {
                continue;
            }
            let id = device["deviceId"].as_str().unwrap_or_default().to_owned();
            if id.is_empty() || !asked.insert(id.clone()) {
                continue;
            }
            // Claim consumes the single-use link. The image is no longer
            // useful while the person reads the phone's verification text.
            qr_file.clear()?;
            let label = device["label"].as_str().unwrap_or("A phone");
            let verification = pairing_verification(device);
            let interactive = local.pair_interactive && !as_json;
            if as_json {
                write_event(output, &verification)?;
            } else if interactive {
                writeln!(
                    output,
                    "“{label}” is waiting. Read the verification text on your phone to connect it."
                )?;
            } else {
                writeln!(output, "“{label}” is waiting. Ask the person to read the verification text from their phone, then use `cupola pair --approve {id} --code <TEXT>` or --deny.")?;
            }
            output.flush()?;
            // A mistyped text keeps this phone, its expiry and the daemon's
            // attempt limit: the terminal asks again (only interactively).
            let mut prompt = "Verification text from your phone (blank to reject): ";
            let phone_code = loop {
                let phone_code = if cfg!(feature = "dev-pairing") && assume_yes {
                    Some(String::new())
                } else {
                    if interactive {
                        write!(output, "{prompt}")?;
                        output.flush()?;
                    }
                    // `pair --approve <deviceId> --code <phone text>` or --deny
                    // (another process) answers for the person; the phone then
                    // leaves the pending list.
                    let typed = if interactive {
                        answer()
                    } else {
                        Box::pin(std::future::pending()) as PairingCodeAnswer
                    };
                    let external = async {
                        loop {
                            if cancelled() {
                                return Err("pairing cancelled".into());
                            }
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            let pending = local.call("pairing.pending", Value::Null).await?;
                            if !pending
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(|d| d["deviceId"].as_str() == Some(id.as_str()))
                            {
                                return Ok::<_, Box<dyn std::error::Error>>(());
                            }
                        }
                    };
                    tokio::select! {
                        typed = by(Some(deadline), typed) => typed?,
                        decided = by(Some(deadline), async { external.await.map_err(|e| e.to_string().into()) }) => {
                            decided?;
                            let devices = local.call("devices.list", Value::Null).await?;
                            let connected = devices.as_array().into_iter().flatten().any(|d| {
                                d["id"].as_str() == Some(id.as_str()) && d["revokedAt"].is_null()
                            });
                            if as_json {
                                write_event(output, &if connected {
                                    PairEvent::Connected { device_id: id, label: label.into() }
                                } else {
                                    PairEvent::Rejected { device_id: id, label: label.into() }
                                })?;
                            } else {
                                writeln!(output, "{} “{label}”.", if connected { "Connected" } else { "Rejected" })?;
                            }
                            if connected { return Ok(()); }
                            continue 'devices;
                        }
                    }
                };
                if cancelled() {
                    return Err("pairing cancelled".into());
                }
                let approve = phone_code.is_some();
                let action = if approve { "confirm" } else { "reject" };
                let decided = local
                .call(&format!("pairing.{action}"), if approve {
                    let params = json!({ "deviceId": id, "code": phone_code.as_deref().unwrap_or_default() });
                    #[cfg(feature = "dev-pairing")]
                    let params = { let mut p = params; p["devApprove"] = Value::Bool(assume_yes); p };
                    params
                } else { json!({ "deviceId": id }) })
                .await;
                match decided {
                    Err(error) if interactive && approve && code_mismatch(error.as_ref()) => {
                        writeln!(output, "That text does not match “{label}”.")?;
                        prompt =
                            "Read the verification text from your phone again (blank to reject): ";
                    }
                    decided => {
                        decided?;
                        break phone_code;
                    }
                }
            };
            let approve = phone_code.is_some();
            if interactive && approve {
                writeln!(
                    output,
                    "Verification text: {}\nMatched the text on “{label}”.",
                    phone_code.as_deref().unwrap_or_default()
                )?;
            }
            if as_json {
                write_event(
                    output,
                    &if approve {
                        PairEvent::Connected {
                            device_id: id,
                            label: label.into(),
                        }
                    } else {
                        PairEvent::Rejected {
                            device_id: id,
                            label: label.into(),
                        }
                    },
                )?;
            } else {
                writeln!(
                    output,
                    "{} “{label}”.",
                    if approve { "Connected" } else { "Rejected" }
                )?;
            }
            if approve {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err("pairing timed out; run `cupola pair` again".into())
}

fn write_event(output: &mut impl Write, event: &PairEvent) -> Result<()> {
    writeln!(output, "{}", serde_json::to_string(event)?)?;
    output.flush()?; // The automation must receive an offer before the process exits.
    Ok(())
}

/// Pairing events are one compact JSON object per line.
fn print_event(event: &PairEvent) {
    let _ = write_event(&mut std::io::stdout(), event);
}

#[cfg(test)]
mod qr_file_tests {
    use super::*;

    #[test]
    fn pair_qr_writes_png_or_svg_and_is_deleted_with_the_offer() {
        let dir = tempfile::tempdir().unwrap();
        let url = "https://wonder.example.ts.net/pair#fixture";
        let png = dir.path().join("pair.png");
        write_qr_file(&png, url).unwrap();
        let image = image::open(&png).unwrap().to_luma8();
        assert!(image.width() == image.height() && image.width() >= 200);
        assert_eq!(image.get_pixel(0, 0).0[0], 255, "quiet zone is light");
        let svg = dir.path().join("pair.SVG");
        write_qr_file(&svg, url).unwrap();
        assert!(std::fs::read_to_string(&svg).unwrap().contains("<svg"));
        drop(QrFile(Some(png.clone()), None));
        assert!(!png.exists());
        drop(QrFile(None, None));
    }

    #[test]
    fn pair_qr_preserves_destination_and_unrelated_temporary_files() {
        // Contract: QR creation never replaces user files, even at the last
        // write boundary. The existing QR test owns rendering and cleanup.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            data: dir.path().into(),
            state: dir.path().into(),
        };
        let url = "https://wonder.example.ts.net/pair#fixture";
        for extension in ["png", "svg"] {
            let qr = dir.path().join(format!("report.{extension}"));
            let sibling = qr.with_extension("tmp");
            std::fs::write(&sibling, b"unrelated report").unwrap();
            let file = QrFile::create(&paths, "offer", &qr, url).unwrap();
            assert_eq!(std::fs::read(&sibling).unwrap(), b"unrelated report");
            // Exercise the writer directly: create-new must protect the
            // destination without relying on an earlier existence check.
            let original = std::fs::read(&qr).unwrap();
            assert!(write_qr_file(&qr, url).is_err());
            assert_eq!(std::fs::read(&qr).unwrap(), original);
            drop(file);
            assert!(!qr.exists());
            assert_eq!(std::fs::read(&sibling).unwrap(), b"unrelated report");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, net::UnixStream};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn computer_helper_launcher_forwards_every_argument_verbatim() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("it's $HOME `x`");
        std::fs::create_dir_all(&root).unwrap();
        // A stand-in interpreter that prints each argument it receives.
        let python = root.join("py thon");
        std::fs::write(&python, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        os::make_executable(&python).unwrap();
        let script = root.join("helper's.py");
        let launcher = root.join("computer-helper");
        let body = computer_helper_script(&python.to_string_lossy(), &script);
        std::fs::write(&launcher, body).unwrap();
        os::make_executable(&launcher).unwrap();
        let output = Command::new(&launcher)
            .args(["--discover-sources", "a b", "$(touch pwned)", "'\""])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "{}\n--discover-sources\na b\n$(touch pwned)\n'\"\n",
                script.display()
            )
        );
        assert!(!root.join("pwned").exists());
    }

    #[test]
    fn headless_service_unit_quotes_paths_and_appends_to_the_host_log() {
        let unit = unit_file(
            Path::new("/home/a b/.local/bin/cupola$x%"),
            Path::new("/home/a b/.wonder/logs/wonder.log"),
            &[("PATH".into(), "/usr/bin:/home/a \"b\"/bin".into())],
        )
        .unwrap();
        assert!(
            unit.contains("ExecStart=\"/home/a b/.local/bin/cupola$$x%%\" start --foreground\n")
        );
        assert!(unit.contains("Environment=\"PATH=/usr/bin:/home/a \\\"b\\\"/bin\"\n"));
        assert!(unit.contains("StandardOutput=append:/home/a b/.wonder/logs/wonder.log\n"));
        assert!(unit.contains("KillMode=mixed\n"));
        assert!(unit.contains("WantedBy=default.target\n"));
        assert!(unit_file(Path::new("/x\ny"), Path::new("/l"), &[]).is_err());
    }

    #[test]
    fn a_moved_data_directory_moves_the_service_log() {
        let env = [("WONDER_DATA_DIR".to_owned(), "/home/a/.wonder".to_owned())];
        let exe = Path::new("/home/a/.local/bin/cupola");
        let (legacy, target) = (Path::new("/home/a/.wonder"), Path::new("/home/a/.cupola"));
        let old = unit_file(exe, &legacy.join("logs/wonder.log"), &env).unwrap();
        let moved = relocate_unit(&old, legacy, target).unwrap();
        // The service install of the new release writes this same unit.
        assert_eq!(
            moved,
            unit_file(exe, &target.join("logs/wonder.log"), &env).unwrap()
        );
        assert_eq!(relocate_unit(&moved, legacy, target), None);
    }

    #[test]
    fn headless_update_points_a_pinned_service_at_the_switched_links() {
        let env = [("WONDER_DATA_DIR".to_owned(), "/home/a/.wonder".to_owned())];
        let log = Path::new("/home/a/.wonder/logs/wonder.log");
        let link = Path::new("/home/a/.local/bin/cupola");
        let pinned = unit_file(Path::new("/home/a/src/target/release/cupola"), log, &env).unwrap();
        let fresh = unit_file(link, log, &env).unwrap();
        // The retargeted unit is the one `service install` writes from the link.
        assert_eq!(retarget_unit(&pinned, link).unwrap(), Some(fresh.clone()));
        assert_eq!(retarget_unit(&fresh, link).unwrap(), None);
        assert!(retarget_unit("[Service]\nType=simple\n", link).is_err());
    }

    #[test]
    fn headless_update_orders_versions_and_downloads_only_over_https() {
        assert!(newer("1.2.10", "1.2.9"));
        assert!(newer("1.3.0", "1.3.0-beta.2"));
        assert!(!newer("1.3.0-beta.2", "1.3.0"));
        assert!(!newer("1.2.0", "1.2.0"));
        assert!(valid_version("1.2.0-nightly.20261017"));
        assert!(!valid_version("../1"));
        assert!(!valid_version("1/2"));
        let url = |text| reqwest::Url::parse(text).unwrap();
        assert!(trusted_url(&url("https://example.com/r/")).is_ok());
        assert!(trusted_url(&url("http://127.0.0.1:8000/r/")).is_ok());
        assert!(trusted_url(&url("http://example.com/r/")).is_err());
        assert!(trusted_url(&url("file:///tmp/r/")).is_err());
    }

    #[tokio::test]
    async fn headless_update_checks_sha256_before_anything_is_switched() {
        use sha2::Digest;
        let dir = tempfile::tempdir().unwrap();
        // A release whose `wonder` reports a version, packed as the feed ships it.
        let release = dir.path().join("pack/cupola-9.9.9");
        std::fs::create_dir_all(&release).unwrap();
        let script = "#!/bin/sh\necho '{\"version\": \"9.9.9\"}'\n";
        for name in ["cupola", "wonderd"] {
            std::fs::write(release.join(name), script).unwrap();
            std::fs::set_permissions(release.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let archive = dir.path().join("release.tar.gz");
        assert!(Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.path().join("pack"))
            .arg("cupola-9.9.9")
            .status()
            .unwrap()
            .success());
        let bytes = std::fs::read(&archive).unwrap();
        let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
        let app = axum::Router::new().route(
            "/release.tar.gz",
            axum::routing::get(move || {
                let bytes = bytes.clone();
                async move { bytes }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = reqwest::Url::parse(&format!(
            "http://{}/release.tar.gz",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let download = dir.path().join("download");

        let wrong = "0".repeat(64);
        let error = download_verified(&client, &url, &wrong, &download)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("did not match its published SHA-256"));
        assert!(!download.exists(), "a mismatched download is removed");

        download_verified(&client, &url, &sha256, &download)
            .await
            .unwrap();
        let staging = dir.path().join("staging");
        assert!(unpack_release(&download, &staging, "9.9.8")
            .unwrap_err()
            .to_string()
            .contains("does not run as Cupola 9.9.8"));
        let unpacked = unpack_release(&download, &staging, "9.9.9").unwrap();
        let bin = dir.path().join("bin");
        Switch::apply(&bin, &unpacked, None).unwrap();
        Switch::apply(&bin, &unpacked, None).unwrap();
        for name in ["cupola", "wonderd"] {
            assert_eq!(
                std::fs::read_link(bin.join(name)).unwrap(),
                unpacked.join(name)
            );
        }
    }

    /// A release folder under `versions` whose `wonder` and `wonderd` are
    /// real programs (copies of `sleep`), so a "host" can run from it.
    fn fake_release(versions: &Path, version: &str) -> PathBuf {
        let path = versions.join(version);
        std::fs::create_dir_all(&path).unwrap();
        for name in ["cupola", "wonderd"] {
            std::fs::copy("/bin/sleep", path.join(name)).unwrap();
        }
        path
    }

    /// Runs `program` as a stand-in host; killed when dropped.
    #[cfg(all(unix, not(target_os = "macos")))]
    struct FakeHost(std::process::Child);

    #[cfg(all(unix, not(target_os = "macos")))]
    impl FakeHost {
        fn run(program: &Path) -> Self {
            let child = Command::new(program).arg("60").spawn().unwrap();
            // Until exec finishes, /proc shows the test binary.
            let deadline = Instant::now() + Duration::from_secs(60);
            while program_of(child.id()).as_deref() != Some(program) {
                assert!(
                    Instant::now() < deadline,
                    "{} did not start",
                    program.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Self(child)
        }

        fn release(&self) -> PathBuf {
            program_of(self.0.id())
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf()
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    impl Drop for FakeHost {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn headless_update_keeps_the_release_the_host_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let versions = dir.path();
        let (a, b, c) = (
            fake_release(versions, "1.0.0"),
            fake_release(versions, "1.0.1"),
            fake_release(versions, "1.0.2"),
        );
        let host = FakeHost::run(&a.join("cupola"));
        // A→B with the restart declined, then B→C: the host still runs A.
        prune(
            versions,
            vec![c.clone(), b.join("cupola")],
            Ok(Some(host.release())),
        );
        assert!(a.is_dir() && b.is_dir() && c.is_dir());
        // A host whose release cannot be told keeps every release.
        let d = fake_release(versions, "0.9.0");
        prune(versions, vec![c.clone()], Err(()));
        assert!(a.is_dir() && b.is_dir() && d.is_dir());

        // Reinstalling the version the host runs leaves its files alone:
        // the host's program is never deleted under it.
        let staged = fake_release(versions, ".staging-1");
        std::fs::write(staged.join("marker"), "new").unwrap();
        place_release(&staged, &a, &[host.release()]).unwrap();
        assert_eq!(program_of(host.0.id()), Some(a.join("cupola")));
        assert!(!a.join("marker").exists() && !staged.exists());
        // A folder nothing runs from is replaced.
        let staged = fake_release(versions, ".staging-2");
        std::fs::write(staged.join("marker"), "new").unwrap();
        place_release(&staged, &d, &[host.release()]).unwrap();
        assert!(d.join("marker").is_file() && !staged.exists());

        // An interrupted update's leftovers go; the lock stays.
        std::fs::create_dir_all(versions.join(".staging-7")).unwrap();
        std::fs::write(versions.join(".download-7"), "").unwrap();
        std::fs::write(versions.join(".update.lock"), "").unwrap();
        // After the host restarted onto C, older releases go.
        drop(host);
        let host = FakeHost::run(&c.join("cupola"));
        prune(
            versions,
            vec![c.clone(), b.clone()],
            Ok(Some(host.release())),
        );
        assert!(!a.exists() && !d.exists() && b.is_dir() && c.is_dir());
        assert!(!versions.join(".staging-7").exists() && !versions.join(".download-7").exists());
        assert!(versions.join(".update.lock").is_file());
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn headless_update_moves_a_copied_install_aside_whole_before_switching() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, versions) = (dir.path().join("bin"), dir.path().join("versions"));
        std::fs::create_dir_all(&bin).unwrap();
        // INSTALL.md's source install copies both programs into bin.
        std::fs::write(
            bin.join("cupola"),
            "#!/bin/sh\necho '{\"version\": \"1.2.3\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(bin.join("cupola"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::copy("/bin/sleep", bin.join("wonderd")).unwrap();
        let daemon = FakeHost::run(&bin.join("wonderd"));

        let adopted = adopt_copies(&bin, &versions).unwrap().unwrap();
        assert!(adopted
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("1.2.3+copy."));
        // The running daemon's program moved with it instead of being deleted.
        assert_eq!(program_of(daemon.0.id()), Some(adopted.join("wonderd")));
        for name in ["cupola", "wonderd"] {
            assert_eq!(
                std::fs::read_link(bin.join(name)).unwrap(),
                adopted.join(name)
            );
        }
        // Links already: nothing more to adopt; the switch can go back to it.
        assert_eq!(adopt_copies(&bin, &versions).unwrap(), None);
        let new = fake_release(&versions, "1.3.0");
        let switch = Switch::apply(&bin, &new, None).unwrap();
        assert_eq!(switch.previous_release(), Some(adopted.clone()));
        prune(&versions, vec![new.clone()], Ok(Some(daemon.release())));
        assert!(adopted.join("wonderd").is_file());
    }

    #[test]
    fn headless_update_puts_links_and_service_back_when_a_switch_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, versions) = (dir.path().join("bin"), dir.path().join("versions"));
        let (old, new) = (
            fake_release(&versions, "1.0.0"),
            fake_release(&versions, "1.1.0"),
        );
        let env = [("WONDER_DATA_DIR".to_owned(), "/d".to_owned())];
        let log = Path::new("/d/logs/wonder.log");
        let unit = dir.path().join("wonder.service");
        let pinned = unit_file(Path::new("/src/target/release/cupola"), log, &env).unwrap();
        std::fs::write(&unit, &pinned).unwrap();
        Switch::apply(&bin, &old, None).unwrap();
        let links =
            |bin: &Path| ["cupola", "wonderd"].map(|name| std::fs::read_link(bin.join(name)).ok());
        let before = links(&bin);

        // The service file cannot be retargeted: both links go back.
        std::fs::write(&unit, "[Service]\nType=simple\n").unwrap();
        assert!(Switch::apply(&bin, &new, Some(&unit)).is_err());
        assert_eq!(links(&bin), before);
        // `wonderd` cannot be replaced: `wonder` goes back.
        std::fs::write(&unit, &pinned).unwrap();
        let other = dir.path().join("other");
        Switch::apply(&other, &old, None).unwrap();
        std::fs::remove_file(other.join("wonderd")).unwrap();
        std::fs::create_dir_all(other.join("wonderd/busy")).unwrap();
        assert!(Switch::apply(&other, &new, Some(&unit)).is_err());
        assert_eq!(
            std::fs::read_link(other.join("cupola")).unwrap(),
            old.join("cupola")
        );
        assert_eq!(std::fs::read_to_string(&unit).unwrap(), pinned);

        // A switch that worked but whose release did not start is undone:
        // links and the pinned service file exactly as they were.
        let switch = Switch::apply(&bin, &new, Some(&unit)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&unit).unwrap(),
            unit_file(&bin.join("cupola"), log, &env).unwrap()
        );
        assert_eq!(switch.previous_release(), Some(old.clone()));
        // The pinned program starts the host again; the links stay as they were.
        switch
            .roll_back_to(Some(&new.join("cupola")), Some(&unit))
            .unwrap();
        assert_eq!(links(&bin), before);
        assert_eq!(std::fs::read_to_string(&unit).unwrap(), pinned);
        // Links that did not exist before are removed again.
        let fresh = dir.path().join("fresh");
        Switch::apply(&fresh, &new, None)
            .unwrap()
            .roll_back()
            .unwrap();
        assert_eq!(links(&fresh), [None, None]);
    }

    #[test]
    fn headless_update_keeps_the_service_file_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (bin, versions) = (dir.path().join("bin"), dir.path().join("versions"));
        let new = fake_release(&versions, "1.1.0");
        let env = [("ANTHROPIC_API_KEY".to_owned(), "sk-secret".to_owned())];
        let log = Path::new("/d/logs/wonder.log");
        let unit = dir.path().join("wonder.service");
        let pinned = unit_file(Path::new("/src/target/release/cupola"), log, &env).unwrap();
        assert!(pinned.contains("sk-secret"));
        write_private(&unit, pinned.as_bytes()).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        // A temp file left wide open by an older build must not carry its
        // mode into the unit.
        let tmp = unit.with_extension("tmp");
        std::fs::write(&tmp, "").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();

        let switch = Switch::apply(&bin, &new, Some(&unit)).unwrap();
        assert_ne!(std::fs::read_to_string(&unit).unwrap(), pinned);
        assert_eq!(
            mode(&unit) & 0o077,
            0,
            "retargeted unit is {:o}",
            mode(&unit)
        );
        switch.roll_back().unwrap();
        assert_eq!(std::fs::read_to_string(&unit).unwrap(), pinned);
        assert_eq!(mode(&unit) & 0o077, 0, "restored unit is {:o}", mode(&unit));
    }

    #[test]
    fn headless_update_keeps_the_way_back_when_going_back_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, versions) = (dir.path().join("bin"), dir.path().join("versions"));
        let (a, b, c) = (
            fake_release(&versions, "1.0.0"),
            fake_release(&versions, "1.0.1"),
            fake_release(&versions, "1.0.2"),
        );
        let unit = dir.path().join("wonder.service");
        let log = Path::new("/d/logs/wonder.log");
        std::fs::write(&unit, unit_file(&bin.join("cupola"), log, &[]).unwrap()).unwrap();
        // The host ran A; B was installed with the restart declined and this
        // command runs from it.
        Switch::apply(&bin, &a, Some(&unit)).unwrap();
        Switch::apply(&bin, &b, Some(&unit)).unwrap();
        // C does not start; the links go back to A, but A does not start
        // either, so no host runs while pruning.
        let switch = Switch::apply(&bin, &c, Some(&unit)).unwrap();
        let keep = [
            Some(c.clone()),
            Some(b.join("cupola")),
            switch.previous_release(),
        ]
        .into_iter()
        .flatten()
        .collect();
        switch
            .roll_back_to(Some(&a.join("cupola")), Some(&unit))
            .unwrap();
        let keep = keep_after_rollback(keep, &c, Ok(Some(a.clone()))).unwrap();
        prune(&versions, keep, Ok(None));
        // `wonder service restart` still finds A through the links.
        assert_eq!(
            wonderd::canonical_path::canonicalize(bin.join("cupola")).unwrap(),
            wonderd::canonical_path::canonicalize(a.join("cupola")).unwrap()
        );
        assert!(a.join("wonderd").is_file() && b.join("cupola").is_file());
        assert!(!c.exists());
        // A host whose release could not be told prunes nothing.
        assert_eq!(keep_after_rollback(Vec::new(), &c, Err(())), Err(()));
    }

    /// Starts the program the service file's ExecStart names, the way
    /// systemd does: through the links it names, as they are now.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn start_service(unit: &Path) -> FakeHost {
        let text = std::fs::read_to_string(unit).unwrap();
        let exec = text
            .lines()
            .find_map(|line| line.strip_prefix("ExecStart=\""))
            .and_then(|rest| rest.split('"').next())
            .unwrap();
        FakeHost::run(&wonderd::canonical_path::canonicalize(exec).unwrap())
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn headless_update_restarts_a_failed_service_on_the_release_the_host_ran() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, versions) = (dir.path().join("bin"), dir.path().join("versions"));
        let (a, b, c) = (
            fake_release(&versions, "1.0.0"),
            fake_release(&versions, "1.0.1"),
            fake_release(&versions, "1.0.2"),
        );
        let unit = dir.path().join("wonder.service");
        let log = Path::new("/d/logs/wonder.log");
        std::fs::write(&unit, unit_file(&bin.join("cupola"), log, &[]).unwrap()).unwrap();
        // The service runs A; B was installed with the restart declined.
        Switch::apply(&bin, &a, Some(&unit)).unwrap();
        let host = start_service(&unit);
        Switch::apply(&bin, &b, Some(&unit)).unwrap();
        let running = program_of(host.0.id()).unwrap();
        // C does not start: going back restarts the service on A, not B.
        let switch = Switch::apply(&bin, &c, Some(&unit)).unwrap();
        drop(host);
        switch.roll_back_to(Some(&running), Some(&unit)).unwrap();
        let restarted = start_service(&unit);
        assert_eq!(runs(Some(restarted.0.id()), &running), Ok(()));
        assert_eq!(
            std::fs::read_link(bin.join("wonderd")).unwrap(),
            a.join("wonderd")
        );
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn headless_update_counts_a_restart_only_when_the_host_runs_the_new_program() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (
            fake_release(dir.path(), "1.0.0"),
            fake_release(dir.path(), "1.1.0"),
        );
        let wanted = new.join("cupola");
        // The service restarted the old (pinned) program.
        let stale = FakeHost::run(&old.join("cupola"));
        let error = runs(Some(stale.0.id()), &wanted).unwrap_err();
        assert!(
            error.contains(&old.join("cupola").display().to_string()),
            "{error}"
        );
        // systemd between attempts reports no main process.
        assert_eq!(runs(None, &wanted).unwrap_err(), "it is not running");
        let fresh = FakeHost::run(&wanted);
        assert_eq!(runs(Some(fresh.0.id()), &wanted), Ok(()));
    }

    const UPDATE_LOCK_PROBE: &str = "WONDER_TEST_UPDATE_LOCK_PROBE";

    /// Runs only as the second process of the test below: exits 75 when an
    /// update holds the lock, 0 when it could take it.
    #[test]
    fn headless_update_lock_probe() {
        let Some(versions) = std::env::var_os(UPDATE_LOCK_PROBE) else {
            return;
        };
        match lock_updates(Path::new(&versions)) {
            Ok(_) => std::process::exit(0),
            Err(error) if error.to_string() == UPDATE_RUNNING => std::process::exit(75),
            Err(error) => panic!("{error}"),
        }
    }

    #[test]
    fn headless_update_runs_one_updater_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let versions = dir.path().join("versions");
        // Updater A holds the lock while it waits for the restart answer.
        let first = lock_updates(&versions).unwrap();
        // Updater B (another process, or this one) stops before downloading.
        let error = lock_updates(&versions).err().unwrap();
        assert_eq!(error.to_string(), UPDATE_RUNNING);
        // Another process (this test binary, run as the probe below) too.
        let other_process = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::headless_update_lock_probe"])
            .env(UPDATE_LOCK_PROBE, &versions)
            .output()
            .unwrap();
        assert_eq!(
            other_process.status.code(),
            Some(75),
            "{}",
            String::from_utf8_lossy(&other_process.stdout)
        );
        // Neither A's pruning nor B's touches the lock file.
        prune_versions(&versions, &[]);
        assert!(versions.join(".update.lock").is_file());
        // A exits (or is killed): the next update proceeds.
        // (Another test's fork holds the fd until its exec closes it.)
        drop(first);
        let deadline = Instant::now() + Duration::from_secs(60);
        while let Err(error) = lock_updates(&versions) {
            assert_eq!(error.to_string(), UPDATE_RUNNING);
            assert!(Instant::now() < deadline, "the lock outlived its updater");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The Windows updater keeps the new release and the one it ran from,
    /// and clears older releases and what interrupted updates left; the
    /// update lock and this command's own staging folder stay.
    #[test]
    fn headless_update_prunes_old_releases_and_interrupted_staging() {
        let dir = tempfile::tempdir().unwrap();
        let versions = dir.path().join("versions");
        let make = |name: &str| {
            let path = versions.join(name);
            std::fs::create_dir_all(path.join("nested")).unwrap();
            std::fs::write(path.join("cupola.exe"), b"x").unwrap();
            path
        };
        let (old, previous, new) = (make("1.0.0-aa"), make("1.0.1-bb"), make("1.0.2-cc"));
        let (interrupted, stage) = (make(".update-dead"), make(".update-live"));
        std::fs::write(versions.join(".update.lock"), b"").unwrap();
        prune_versions(&versions, &[new.clone(), previous.clone(), stage.clone()]);
        assert!(!old.exists() && !interrupted.exists());
        assert!(new.is_dir() && previous.is_dir() && stage.is_dir());
        assert!(versions.join(".update.lock").is_file());
    }

    #[tokio::test]
    async fn headless_update_sends_the_owners_consent_and_asks_again_when_work_started() {
        let (local, seen) = stub_daemon(|_, path| match path {
            "/api/v1/host/update/prepare" => (
                409,
                "Agents started working after you were asked (1 running).",
            ),
            _ => (404, ""),
        })
        .await;
        let error = prepare_for_update(&local, Some(0)).await.unwrap_err();
        assert!(error.downcast_ref::<WorkStarted>().is_some(), "{error}");
        let body: Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        assert_eq!(body["consentedRunning"], 0);
        assert!(uuid::Uuid::parse_str(body["requestId"].as_str().unwrap()).is_ok());

        // Any other refusal is passed on as the daemon's own words.
        let (local, _) =
            stub_daemon(|_, _| (409, r#"{"detail":"Another update is being prepared."}"#)).await;
        let error = prepare_for_update(&local, None).await.unwrap_err();
        assert!(error.downcast_ref::<WorkStarted>().is_none());
        assert_eq!(error.to_string(), "Another update is being prepared.");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn platform_update_never_pauses_sharing_or_unconsented_work() {
        let (local, seen) = stub_daemon(|_, path| match path {
            "/api/v1/host/update/work" => (200, r#"{"running":2,"blocksUpdate":false}"#),
            _ => (500, "{}"),
        })
        .await;
        assert!(matches!(
            update_running_plan(local, false, true).await.unwrap(),
            UpdatePlan::Deferred
        ));
        assert_eq!(seen.lock().unwrap().len(), 1);

        let (local, seen) = stub_daemon(|_, path| match path {
            "/api/v1/host/update/work" => (200, r#"{"running":0,"blocksUpdate":true}"#),
            _ => (500, "{}"),
        })
        .await;
        assert!(matches!(
            update_running_plan(local, true, true).await.unwrap(),
            UpdatePlan::Deferred
        ));
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "--yes must not pause a viewer or in-flight send"
        );

        let (local, seen) = stub_daemon(|_, path| match path {
            "/api/v1/host/update/work" => (200, r#"{"running":0,"blocksUpdate":false}"#),
            "/api/v1/host/update/prepare" => (200, "{}"),
            _ => (500, "{}"),
        })
        .await;
        assert!(matches!(
            update_running_plan(local, false, true).await.unwrap(),
            UpdatePlan::Restart { .. }
        ));
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            let body: Value = serde_json::from_str(&seen[1].body).unwrap();
            assert_eq!(body["consentedRunning"], 0);
        }

        let (local, seen) = stub_daemon(|_, path| match path {
            "/api/v1/host/update/work" => (200, r#"{"running":0,"blocksUpdate":false}"#),
            "/api/v1/host/update/prepare" => (
                409,
                r#"{"detail":"Agents started working after you were asked."}"#,
            ),
            _ => (500, "{}"),
        })
        .await;
        assert!(matches!(
            update_running_plan(local, false, true).await.unwrap(),
            UpdatePlan::Deferred
        ));
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "noninteractive updates must not spin on a consent race"
        );
    }

    // Contract: `logs --follow` counts raw bytes, so a byte that is not
    // text never makes it reprint the log, and a character split between two
    // writes prints once, whole.
    #[test]
    fn following_the_log_survives_bytes_that_are_not_text() {
        use std::io::Write as _;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wonder.log");
        std::fs::write(&path, b"one \xff\xfe\ntwo\nthree\n").unwrap();
        let (lines, mut offset) = log_tail(&path, 2).unwrap();
        assert_eq!(lines, ["two", "three"]);
        assert_eq!(offset, std::fs::metadata(&path).unwrap().len());
        let mut carry = Utf8Carry::default();
        let mut output = Vec::new();
        let append = |bytes: &[u8]| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(bytes).unwrap();
        };
        follow_log(&path, &mut offset, &mut carry, &mut output).unwrap();
        assert!(output.is_empty());
        append(b"bad \xff then \xc3");
        follow_log(&path, &mut offset, &mut carry, &mut output).unwrap();
        append(b"\xa9\n");
        follow_log(&path, &mut offset, &mut carry, &mut output).unwrap();
        follow_log(&path, &mut offset, &mut carry, &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "bad \u{fffd} then \u{e9}\n"
        );
    }

    // The tail is read from the end, bounded, and drops the line it cut.
    #[test]
    fn the_log_tail_reads_only_the_end_of_a_large_log() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wonder.log");
        let line = format!("{}\n", "x".repeat(1023));
        let mut log = line.repeat((LOG_TAIL_BYTES as usize / 1024) + 64);
        log.push_str("last\n");
        std::fs::write(&path, &log).unwrap();
        let (lines, offset) = log_tail(&path, 3).unwrap();
        assert_eq!(lines.last().map(String::as_str), Some("last"));
        assert_eq!(lines.len(), 3);
        assert_eq!(offset, log.len() as u64);
        let (lines, _) = log_tail(&path, usize::MAX).unwrap();
        assert!(lines.len() < LOG_TAIL_BYTES as usize / 1024 + 1);
        assert!(lines.iter().all(|l| l == "last" || l.len() == 1023));
    }

    #[test]
    fn headless_logs_show_the_latest_lines_oldest_first() {
        assert_eq!(last_lines("a\nb\nc\n", 2), ["b", "c"]);
        assert_eq!(last_lines("a\nb", 10), ["a", "b"]);
        assert!(last_lines("", 5).is_empty());
        assert!(last_lines("a\n", 0).is_empty());
    }

    /// A Mac install has Node only inside Cupola.app, never on PATH: Claude
    /// Code's sign-in and its hint use that Node, as `cupola doctor` does.
    #[test]
    fn headless_claude_sign_in_runs_the_node_bundled_in_cupola_app() {
        if std::env::var_os("WONDER_NODE_BIN").is_some()
            || std::env::var_os("WONDER_CLAUDE_BRIDGE").is_some()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let contents = dir.path().join("Cupola.app/Contents");
        let exe = contents.join("MacOS/cupola");
        let node = contents.join("Resources/node/bin/node");
        let runtime = contents.join("Resources/claude-runtime");
        for path in [
            &exe,
            &node,
            &runtime.join("main.mjs"),
            &runtime.join("manage.mjs"),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let command = own_sign_in_command_for(Provider::Claude, Some(&exe)).unwrap();
        let real = |path: &Path| wonderd::canonical_path::canonicalize(path).unwrap();
        assert_eq!(real(Path::new(command.get_program())), real(&node));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args.len(), 2, "{args:?}");
        assert_eq!(real(Path::new(args[0])), real(&runtime.join("manage.mjs")));
        assert_eq!(args[1], "login");
    }

    #[test]
    fn headless_own_sign_in_runs_the_providers_command_attached_to_this_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("codex");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho \"$@\" > \"$0.args\"\necho 'Enter code ABCD-1234'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let mut command = Command::new(&fake);
        command.args(["login", "--device-auth"]);
        run_in_this_terminal(&mut command, "Codex", true).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("codex.args")).unwrap(),
            "login --device-auth\n"
        );
        let mut failing = Command::new("/usr/bin/false");
        let error = run_in_this_terminal(&mut failing, "Codex", false).unwrap_err();
        assert!(error.to_string().contains("did not finish"), "{error}");
    }

    /// One request the stub daemon received.
    #[derive(Debug, Clone, PartialEq)]
    struct Seen {
        method: String,
        path: String,
        capability: Option<String>,
        body: String,
    }

    /// A loopback HTTP server standing in for `wonderd`: it records each
    /// request and answers `reply(method, path)` as (status, body).
    async fn stub_daemon(
        reply: impl Fn(&str, &str) -> (u16, &'static str) + Clone + Send + Sync + 'static,
    ) -> (Local, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        stub_daemon_after(Duration::ZERO, reply).await
    }

    /// [`stub_daemon`] answering each request only after `delay`.
    async fn stub_daemon_after(
        delay: Duration,
        reply: impl Fn(&str, &str) -> (u16, &'static str) + Clone + Send + Sync + 'static,
    ) -> (Local, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        stub_daemon_steps(vec![delay], reply).await
    }

    /// [`stub_daemon`] whose handler works through `steps` one after another
    /// before answering, as the daemon chains its own bounded steps.
    async fn stub_daemon_steps(
        steps: Vec<Duration>,
        reply: impl Fn(&str, &str) -> (u16, &'static str) + Clone + Send + Sync + 'static,
    ) -> (Local, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        use axum::{body::Bytes, http::Request, response::IntoResponse};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let app = axum::Router::new().fallback(move |request: Request<axum::body::Body>| {
            let log = log.clone();
            let steps = steps.clone();
            let reply = reply.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body: Bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap();
                let method = parts.method.to_string();
                let path = parts.uri.path().to_owned();
                log.lock().unwrap().push(Seen {
                    method: method.clone(),
                    path: path.clone(),
                    capability: parts
                        .headers
                        .get("x-wonder-loopback-capability")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                    body: String::from_utf8_lossy(&body).into_owned(),
                });
                for step in steps {
                    tokio::time::sleep(step).await;
                }
                let (status, body) = reply(&method, &path);
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    [("content-type", "application/json")],
                    body,
                )
                    .into_response()
            }
        });
        let http = app.clone();
        let app = app.route(
            "/api/v1/rpc",
            axum::routing::get(
                move |ws: axum::extract::WebSocketUpgrade, headers: axum::http::HeaderMap| {
                    let http = http.clone();
                    let capability = headers
                        .get("x-wonder-loopback-capability")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    async move { ws.on_upgrade(move |socket| stub_rpc(socket, http, capability)) }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (Local::at(base, "cap-1".into()).unwrap(), seen)
    }

    /// The stub daemon's `/api/v1/rpc`: each method becomes its route call
    /// from the real method table (`wonderd::rpc::route`), answered by the
    /// stub's HTTP handler, so these tests see the routes the CLI reaches.
    async fn stub_rpc(
        socket: axum::extract::ws::WebSocket,
        http: axum::Router,
        capability: String,
    ) {
        use axum::extract::ws::Message;
        use futures_util::{SinkExt, StreamExt};
        use tower::ServiceExt;
        use wonderd::rpc::{route, Caller, Request, RpcError};
        let (mut sink, mut stream) = socket.split();
        let (out, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(reply) = outgoing.recv().await {
                if sink
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        while let Some(Ok(Message::Text(text))) = stream.next().await {
            let request: Request = serde_json::from_str(text.as_str()).unwrap();
            let id = json!(request.id);
            if request.method == "initialize" {
                let result = json!({
                    "protocolVersion": 1,
                    "serverInfo": {"name": "stub", "version": "0", "apiVersion": "v1"},
                    "authority": "owner", "hostEpoch": "e", "hostInstallationId": "i",
                    "methods": [], "notifications": [],
                    "capabilities": {"compactView": false, "maxInFlight": 32, "maxRequestBytes": 1024},
                });
                out.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                    .unwrap();
                continue;
            }
            let call = route(&request.method, request.params, &Caller::Owner).unwrap();
            let (http, out, capability) = (http.clone(), out.clone(), capability.clone());
            tokio::spawn(async move {
                let mut builder = axum::http::Request::builder()
                    .method(call.method)
                    .uri(call.uri)
                    .header("x-wonder-loopback-capability", capability);
                let body = match call.body {
                    Some(body) => {
                        builder = builder.header("content-type", "application/json");
                        axum::body::Body::from(body.to_string())
                    }
                    None => axum::body::Body::empty(),
                };
                let response = http.oneshot(builder.body(body).unwrap()).await.unwrap();
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap();
                let reply = if status.is_success() {
                    let result: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
                    json!({"jsonrpc": "2.0", "id": id, "result": result})
                } else {
                    let error = RpcError::from_http(status.as_u16(), &bytes);
                    json!({"jsonrpc": "2.0", "id": id, "error": error})
                };
                let _ = out.send(reply);
            });
        }
    }

    /// A daemon socket that, like `wonderd`, pings and drops a peer that
    /// stays silent past `idle`; it answers every call but `initialize`
    /// with `{}`.
    fn pinging_daemon(idle: Duration) -> (Local, std::sync::Arc<AtomicUsize>) {
        use axum::extract::ws::Message;
        let connections = std::sync::Arc::new(AtomicUsize::new(0));
        let count = connections.clone();
        let app = axum::Router::new().route(
            "/api/v1/rpc",
            axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
                count.fetch_add(1, Ordering::SeqCst);
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        let mut ping = tokio::time::interval(idle / 4);
                        let mut heard = tokio::time::Instant::now();
                        loop {
                            tokio::select! {
                                frame = socket.recv() => {
                                    heard = tokio::time::Instant::now();
                                    let Some(Ok(frame)) = frame else { break };
                                    let Message::Text(text) = frame else { continue };
                                    let request: Value = serde_json::from_str(&text).unwrap();
                                    let result = if request["method"] == "initialize" {
                                        json!({
                                            "protocolVersion": 1,
                                            "serverInfo": {"name": "stub", "version": "0", "apiVersion": "v1"},
                                            "authority": "owner", "hostEpoch": "e", "hostInstallationId": "i",
                                            "methods": [], "notifications": [],
                                            "capabilities": {"compactView": false, "maxInFlight": 32, "maxRequestBytes": 1024},
                                        })
                                    } else {
                                        json!({})
                                    };
                                    let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
                                    let _ = socket.send(Message::Text(reply.to_string().into())).await;
                                }
                                _ = ping.tick() => {
                                    let _ = socket.send(Message::Ping(Default::default())).await;
                                }
                                _ = tokio::time::sleep_until(heard + idle) => break,
                            }
                        }
                    })
                }
            }),
        );
        // Its own thread and runtime, as the daemon is its own process: a
        // CLI that blocks its runtime must not stall the daemon too.
        let (addr, bound) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    addr.send(listener.local_addr().unwrap()).unwrap();
                    axum::serve(listener, app).await.unwrap()
                })
        });
        let base = format!("http://{}", bound.recv().unwrap());
        (Local::at(base, "cap-1".into()).unwrap(), connections)
    }

    #[tokio::test]
    async fn a_slow_answer_at_the_pairing_prompt_keeps_the_daemon_connection() {
        let idle = Duration::from_millis(800);
        let (local, connections) = pinging_daemon(idle);
        local.call("pairing.pending", Value::Null).await.unwrap();
        // The person answers long after the daemon's idle limit.
        let (send, receive) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            std::thread::sleep(idle * 3);
            let _ = send.send(Ok("y\n".into()));
        });
        let approve = wait_for_pairing_answer(receive, || false).await.unwrap();
        assert!(approve);
        local
            .call(
                "pairing.confirm",
                json!({"deviceId": "d1", "code": "B36D1B62BB5F"}),
            )
            .await
            .unwrap();
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }

    /// A daemon that reads frames as the host does and answers each after
    /// `delay`, as a route call would.
    async fn slow_call_daemon(delay: Duration) -> Local {
        use axum::extract::ws::Message;
        let app = axum::Router::new().route(
            "/api/v1/rpc",
            axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| async move {
                // The host's transport limit: a larger frame ends the session.
                let ws = ws.max_message_size(wonderd::rpc::MAX_REQUEST_BYTES + 1024);
                ws.on_upgrade(move |socket| async move {
                    use futures_util::{SinkExt, StreamExt};
                    let (sink, mut stream) = socket.split();
                    let sink = std::sync::Arc::new(tokio::sync::Mutex::new(sink));
                    while let Some(Ok(Message::Text(text))) = stream.next().await {
                        let (id, answer) = match wonderd::rpc::read_frame(text.as_str()) {
                            wonderd::rpc::Incoming::Call(id, _) => {
                                (Some(id), json!({"result": {}}))
                            }
                            wonderd::rpc::Incoming::Invalid(id, error) => {
                                (id, json!({"error": error}))
                            }
                            wonderd::rpc::Incoming::Notification => continue,
                        };
                        let sink = sink.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let mut reply = json!({"jsonrpc": "2.0", "id": id});
                            reply
                                .as_object_mut()
                                .unwrap()
                                .extend(answer.as_object().unwrap().clone());
                            let _ = sink
                                .lock()
                                .await
                                .send(Message::Text(reply.to_string().into()))
                                .await;
                        });
                    }
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Local::at(base, "cap-1".into()).unwrap()
    }

    #[tokio::test]
    async fn wonder_rpc_waits_for_every_answer_after_unreadable_lines() {
        let local = slow_call_daemon(Duration::from_millis(300)).await;
        let piped = |input: &'static str| {
            let local = &local;
            async move {
                let mut output = Vec::new();
                tokio::time::timeout(
                    Duration::from_secs(60),
                    relay(local, input.as_bytes(), &mut output),
                )
                .await
                .expect("the relay never finished")
                .unwrap();
                String::from_utf8(output)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .collect::<Vec<_>>()
            }
        };
        // Answers to unreadable lines must not retire the call's answer.
        let answers = piped(
            "not json\n[]\n{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"host.status\"}\n\
             {\"jsonrpc\":\"2.0\",\"method\":\"ignored\"}\n",
        )
        .await;
        assert_eq!(answers.len(), 3, "{answers:?}");
        assert!(answers
            .iter()
            .any(|a| a["id"].is_null() && a["error"]["code"] == wonderd::rpc::codes::PARSE_ERROR));
        assert!(answers
            .iter()
            .any(|a| a["id"] == 7 && a["result"] == json!({})));
        // Unreadable input alone still gets its answer before the exit.
        let answers = piped("not json\n").await;
        assert_eq!(answers.len(), 1, "{answers:?}");
        assert!(answers[0]["id"].is_null());
    }

    // Contract: one oversized line gets its own "too large" answer and the
    // session goes on to answer the requests after it.
    #[tokio::test]
    async fn wonder_rpc_answers_an_oversized_line_and_keeps_the_session() {
        let local = slow_call_daemon(Duration::from_millis(10)).await;
        let big = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"host.name\",\"params\":{{\"pad\":\"{}\"}}}}",
            "x".repeat(300 * 1024)
        );
        let input = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"host.name\"}}\n{big}\r\n\
             {{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"host.name\"}}\n"
        );
        let mut output = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(60),
            relay(&local, input.as_bytes(), &mut output),
        )
        .await
        .expect("the relay never finished")
        .unwrap();
        let answers: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(answers.len(), 3, "{answers:?}");
        assert!(answers.iter().any(
            |a| a["id"].is_null() && a["error"]["code"] == wonderd::rpc::codes::INVALID_REQUEST
        ));
        for id in [1, 3] {
            assert!(answers
                .iter()
                .any(|a| a["id"] == id && a["result"] == json!({})));
        }
    }

    #[tokio::test]
    async fn bounded_lines_keep_a_line_at_the_limit_and_drop_one_over_it() {
        let input = b"abc\r\nabcd\nab\nabcdefgh\nz".as_slice();
        let mut lines = BoundedLines::new(input, 3);
        let mut read = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            read.push(line);
        }
        let text = |s: &str| InputLine::Text(s.into());
        assert_eq!(
            read,
            [
                text("abc"),
                InputLine::TooLarge,
                text("ab"),
                InputLine::TooLarge,
                text("z")
            ]
        );
    }

    #[tokio::test]
    async fn headless_devices_revoke_calls_the_daemons_revoke_route() {
        let (local, seen) = stub_daemon(|method, path| match (method, path) {
            ("POST", "/api/v1/devices/d%201/revoke") => (200, r#"{"revoked":true}"#),
            _ => (404, ""),
        })
        .await;
        let output = change_device(&local, DevicesCommand::Revoke { id: "d 1".into() })
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&output).unwrap(),
            json!({"deviceId": "d 1", "state": "revoked"})
        );
        assert_eq!(
            *seen.lock().unwrap(),
            [Seen {
                method: "POST".into(),
                path: "/api/v1/devices/d%201/revoke".into(),
                capability: Some("cap-1".into()),
                // The route takes optional signed fields; the local owner sends none.
                body: "null".into(),
            }]
        );

        // An unknown ID gets the recovery step, not the daemon's 404.
        let error = change_device(&local, DevicesCommand::Revoke { id: "zz".into() })
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "No phone with ID zz. Run `cupola devices` to see the IDs."
        );
    }

    // Contract: revoke or rename of a phone `cupola devices` lists as
    // disconnected says so and points to forget; it changes nothing.
    #[tokio::test]
    async fn revoking_or_renaming_a_disconnected_phone_says_it_is_already_disconnected() {
        let (local, seen) = stub_daemon(|method, path| match (method, path) {
            ("GET", "/api/v1/devices") => (
                200,
                r#"[{"id":"old","label":"iPhone","role":"owner","lastSeenAt":null,"revokedAt":"2026-10-01T00:00:00Z"}]"#,
            ),
            _ => (404, ""),
        })
        .await;
        for command in [
            DevicesCommand::Revoke { id: "old".into() },
            DevicesCommand::Rename {
                id: "old".into(),
                name: vec!["New".into()],
            },
        ] {
            let error = change_device(&local, command).await.unwrap_err();
            assert_eq!(
                error.to_string(),
                "old is already disconnected. `cupola devices forget old` removes it from the list."
            );
        }
        let error = change_device(&local, DevicesCommand::Revoke { id: "zz".into() })
            .await
            .unwrap_err();
        assert!(
            error.to_string().starts_with("No phone with ID zz."),
            "{error}"
        );
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path.ends_with("/forget") || r.method == "DELETE"));
    }

    #[tokio::test]
    async fn headless_devices_rename_patches_the_device_and_prints_its_new_name() {
        let (local, seen) = stub_daemon(|method, path| match (method, path) {
            ("PATCH", "/api/v1/devices/d1") => (
                200,
                r#"{"id":"d1","label":"Work iPhone","role":"owner","lastSeenAt":null,"revokedAt":null}"#,
            ),
            _ => (404, ""),
        })
        .await;
        let output = change_device(
            &local,
            DevicesCommand::Rename {
                id: "d1".into(),
                name: vec!["Work".into(), "iPhone".into()],
            },
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(&output).unwrap(),
            json!({"deviceId": "d1", "state": "renamed", "label": "Work iPhone"})
        );
        assert_eq!(
            device_action_text(&output),
            "Renamed d1 to \"Work iPhone\"."
        );
        assert_eq!(
            *seen.lock().unwrap(),
            [Seen {
                method: "PATCH".into(),
                path: "/api/v1/devices/d1".into(),
                capability: Some("cap-1".into()),
                body: r#"{"label":"Work iPhone"}"#.into(),
            }]
        );
        let error = change_device(
            &local,
            DevicesCommand::Rename {
                id: "zz".into(),
                name: vec!["Old".into()],
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "No phone with ID zz. Run `cupola devices` to see the IDs."
        );
    }

    #[tokio::test]
    async fn headless_pair_while_the_origin_starts_points_to_status() {
        // The daemon's exact answer before Tailscale Serve is ready (and, for
        // the same Wi-Fi link, its reason too).
        let cases = [
            (
                PairVia::Main,
                "This computer's connection address isn't ready yet. Check `cupola status`.",
                1,
            ),
            (
                PairVia::Any { home: false },
                "Tailscale is not ready yet, and same Wi-Fi connections are turned off. Turn \
                 them on with `cupola lan on` and restart the host, or check `cupola status`.",
                1,
            ),
            (
                PairVia::Any { home: true },
                "Tailscale is not ready yet, and same Wi-Fi pairing failed: \
                 This computer's connection address isn't ready yet.",
                2,
            ),
        ];
        for (via, message, requests) in cases {
            let (mut local, seen) =
                stub_daemon(|_, _| (503, "This computer's connection address isn't ready yet."))
                    .await;
            local.pair_via = via;
            let error = pairing_offer(&local).await.unwrap_err();
            assert_eq!(error.to_string(), message, "{via:?}");
            assert_eq!(seen.lock().unwrap().len(), requests, "{via:?}");
        }

        // Other refusals keep the daemon's own words.
        let (local, _) = stub_daemon(|_, _| (409, r#"{"detail":"Pairing is paused."}"#)).await;
        let error = pairing_offer(&local).await.unwrap_err();
        assert_eq!(error.to_string(), "Pairing is paused.");
    }

    /// Without Tailscale, `cupola pair` falls back to a same Wi-Fi link and
    /// says where the phone must be, without waiting for a public address;
    /// `--route tailscale` never falls back.
    #[tokio::test]
    async fn headless_pair_without_tailscale_pairs_on_the_same_wifi() {
        // Before Tailscale is ready the host reports no public address and
        // refuses a main-route offer.
        let daemon = || async {
            let offers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            stub_daemon(move |method, path| match (method, path) {
                (_, "/api/v1/host/status") => (200, r#"{"publicOrigin":null}"#),
                ("POST", "/api/v1/pairing/offers")
                    if offers.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 =>
                {
                    (503, "This computer's connection address isn't ready yet.")
                }
                _ => pairing_reply(method, path),
            })
            .await
        };
        let offers = |seen: &std::sync::Mutex<Vec<Seen>>| -> Vec<Value> {
            seen.lock()
                .unwrap()
                .iter()
                .filter(|r| r.path == "/api/v1/pairing/offers")
                .map(|r| serde_json::from_str::<Value>(&r.body).unwrap_or(Value::Null))
                .collect()
        };
        let pair = |local: Local| async move {
            let mut output = Vec::new();
            let started = Instant::now();
            let result = pair_until(
                &local,
                true,
                Some(started + Duration::from_secs(60)),
                false,
                &mut output,
                &mut || Box::pin(std::future::pending()),
                // Cancelled once the link is shown; this test reads only that.
                &|| true,
            )
            .await;
            (
                result,
                String::from_utf8(output).unwrap(),
                started.elapsed(),
            )
        };

        let (mut local, seen) = daemon().await;
        local.pair_via = PairVia::Any { home: true };
        let (result, text, took) = pair(local).await;
        result.unwrap_err();
        assert!(
            text.contains("Your iPhone must be on the same Wi-Fi as this computer."),
            "{text}"
        );
        // The app accepts a home network address only with the key the QR
        // code and link carry, so a home offer offers no code to type.
        assert!(
            text.contains("paste this pairing link there: https://"),
            "{text}"
        );
        assert!(!text.contains("Code:"), "{text}");
        assert!(!text.contains("Address:"), "{text}");
        assert!(took < Duration::from_secs(5), "waited {took:?}");
        assert_eq!(offers(&seen), [Value::Null, json!({"route": "home"})]);

        // `--route tailscale` asks for the main link only and says it is not ready.
        let (mut local, seen) = daemon().await;
        local.pair_via = PairVia::Main;
        let (result, text, _) = pair(local).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "This computer's connection address isn't ready yet. Check `cupola status`."
        );
        assert!(!text.contains("Wi-Fi"), "{text}");
        assert_eq!(offers(&seen), [Value::Null]);

        // With Tailscale ready, the link is the main one and says nothing of Wi-Fi.
        let (mut local, _) = stub_daemon(|method, path| match path {
            "/api/v1/host/status" => (200, "{}"),
            _ => pairing_reply(method, path),
        })
        .await;
        local.pair_via = PairVia::Any { home: true };
        let (result, text, _) = pair(local).await;
        result.unwrap_err();
        assert!(text.contains("Scan in Cupola"), "{text}");
        // The code and address use the app's own field name.
        assert!(text.contains("    Address: https://"), "{text}");
        assert!(text.contains("    Code:    "), "{text}");
        assert!(!text.contains("Origin:"), "{text}");
        assert!(!text.contains("Wi-Fi"), "{text}");
    }

    /// `pair --route home` asks the daemon for a home network link, and the
    /// daemon's own reason reaches the owner when it cannot make one.
    #[tokio::test]
    async fn headless_pair_over_the_home_network_asks_for_a_home_link() {
        let (mut local, seen) = stub_daemon(|_, _| {
            (
                503,
                r#"{"detail":"Home network connections are not running on this computer."}"#,
            )
        })
        .await;
        local.pair_via = PairVia::Home;
        let error = pairing_offer(&local).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Home network connections are not running on this computer."
        );
        let seen = seen.lock().unwrap();
        assert_eq!(
            (seen[0].method.as_str(), seen[0].path.as_str()),
            ("POST", "/api/v1/pairing/offers")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&seen[0].body).unwrap(),
            json!({"route": "home"})
        );
    }

    fn pairing_reply(method: &str, path: &str) -> (u16, &'static str) {
        match (method, path) {
            ("POST", "/api/v1/pairing/offers") => (
                200,
                r#"{"offerId":"o1","url":"https://wonder.example.ts.net/pair#fixture","humanCode":"abcd","expiresAtMs":9000000000000,"hostKeyPin":"fixture-pin"}"#,
            ),
            ("GET", "/api/v1/pairing/pending") => (
                200,
                r#"[{"deviceId":"other","offerId":"another-offer"},
                {"deviceId":"device-01","label":"Fixture phone","offerId":"o1","hostInstallationId":"install-1","expiresAtMs":61000,"display":null}]"#,
            ),
            (
                "POST",
                "/api/v1/pairing/pending/device-01/confirm"
                | "/api/v1/pairing/pending/device-01/reject"
                | "/api/v1/pairing/offers/o1/cancel",
            ) => (200, "{}"),
            _ => (500, "unexpected request"),
        }
    }

    async fn interactive_pairing_daemon() -> (Local, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        let (mut local, seen) = stub_daemon(pairing_reply).await;
        local.pair_interactive = true;
        (local, seen)
    }

    #[tokio::test]
    async fn pairing_prints_the_phone_transcript_only_for_its_interactive_terminal() {
        let (local, seen) = interactive_pairing_daemon().await;
        #[derive(Clone, Default)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut output = Capture::default();
        let before_input = output.clone();
        let mut asked = 0;
        pair_with_local(
            &local,
            false,
            Some(Instant::now() + Duration::from_secs(60)),
            false,
            &mut output,
            &mut || {
                assert!(!String::from_utf8_lossy(&before_input.0.lock().unwrap())
                    .contains("B36D1B62BB5F"));
                asked += 1;
                Box::pin(async { Ok(Some("B36D1B62BB5F".into())) })
            },
            &|| false,
        )
        .await
        .unwrap();
        assert_eq!(asked, 1);
        let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        assert!(
            text.find("Verification text from your phone").unwrap()
                < text.find("Verification text: B36D1B62BB5F").unwrap()
        );
        let seen = seen.lock().unwrap();
        assert!(seen
            .iter()
            .all(|r| r.capability.as_deref() == Some("cap-1")));
        assert_eq!(
            seen.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            [
                "/api/v1/pairing/offers",
                "/api/v1/pairing/pending",
                "/api/v1/pairing/pending/device-01/confirm",
                "/api/v1/pairing/offers/o1/cancel"
            ]
        );
        let body: Value = serde_json::from_str(&seen[2].body).unwrap();
        assert_eq!(body["code"], "B36D1B62BB5F");
    }

    /// [`interactive_pairing_daemon`] whose first confirmation answers `refusal`.
    async fn pairing_daemon_refusing_once(
        refusal: &'static str,
    ) -> (Local, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        let refused = std::sync::Arc::new(AtomicUsize::new(0));
        let (mut local, seen) = stub_daemon(move |method, path| match (method, path) {
            ("POST", "/api/v1/pairing/pending/device-01/confirm")
                if refused.fetch_add(1, Ordering::SeqCst) == 0 =>
            {
                (403, refusal)
            }
            _ => pairing_reply(method, path),
        })
        .await;
        local.pair_interactive = true;
        (local, seen)
    }

    // Contract: a mistyped text asks again for the same phone instead of
    // ending the offer (which would drop the waiting phone).
    #[tokio::test]
    async fn pairing_asks_again_after_a_mistyped_text_and_connects_the_same_phone() {
        let (local, seen) = pairing_daemon_refusing_once(
            r#"{"error":"verification_mismatch","detail":"verification text does not match"}"#,
        )
        .await;
        let mut output = Vec::new();
        let mut typed = vec!["000000000000", "b36d-1b62-bb5f"].into_iter();
        pair_with_local(
            &local,
            false,
            Some(Instant::now() + Duration::from_secs(60)),
            false,
            &mut output,
            &mut || {
                let line = typed.next().expect("asked more than twice").to_owned();
                Box::pin(async move { Ok(Some(line)) })
            },
            &|| false,
        )
        .await
        .unwrap();
        assert_eq!(typed.next(), None);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("That text does not match “Fixture phone”."));
        assert!(text.contains("Read the verification text from your phone again"));
        assert!(text.contains("Connected “Fixture phone”."));
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            [
                "/api/v1/pairing/offers",
                "/api/v1/pairing/pending",
                "/api/v1/pairing/pending/device-01/confirm",
                "/api/v1/pairing/pending/device-01/confirm",
                "/api/v1/pairing/offers/o1/cancel"
            ]
        );
    }

    // Any other refusal, a plain 403 included, still ends pairing.
    #[tokio::test]
    async fn pairing_ends_on_a_refusal_other_than_a_mistyped_text() {
        let (local, seen) = pairing_daemon_refusing_once(r#"{"detail":"not allowed"}"#).await;
        let error = pair_with_local(
            &local,
            false,
            Some(Instant::now() + Duration::from_secs(60)),
            false,
            &mut Vec::new(),
            &mut || Box::pin(async { Ok(Some("B36D1B62BB5F".into())) }),
            &|| false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("not allowed"), "{error}");
        let confirms = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path.ends_with("/confirm"))
            .count();
        assert_eq!(confirms, 1);
    }

    #[tokio::test]
    async fn agent_pairing_hides_the_text_and_never_reads_a_typed_yes() {
        // Even a terminal-style session in JSON mode must not consume input
        // or print a comparison. Noninteractive text output is equally private.
        for json in [false, true] {
            let (local, seen) = if json {
                interactive_pairing_daemon().await
            } else {
                stub_daemon(pairing_reply).await
            };
            let mut output = Vec::new();
            let error = pair_with_local(
                &local,
                false,
                Some(Instant::now() + Duration::from_millis(40)),
                json,
                &mut output,
                &mut || panic!("an agent cannot answer y/N"),
                &|| false,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("timed out"));
            let text = String::from_utf8(output).unwrap();
            assert!(!text.contains("B36D1B62BB5F"));
            assert!(!text.contains("verificationText"));
            assert!(!text.contains("nonce"));
            assert!(!text.contains("[y/N]"));
            if json {
                let events: Vec<Value> = text
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(events[1]["event"], "verification");
                assert_eq!(events[1]["hostInstallationId"], "install-1");
            }
            assert!(!seen
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.path.ends_with("/confirm") || r.path.ends_with("/reject")));
        }
    }

    // Contract: EOF in an agent's background shell is no decision. Only the
    // person's explicit --approve/--deny may answer a claimed offer.
    #[tokio::test]
    async fn pairing_eof_waits_for_external_approval_without_rejecting_the_phone() {
        let state = std::sync::Arc::new(AtomicUsize::new(0));
        let host_state = state.clone();
        let (local, seen) = stub_daemon(move |method, path| match (method, path) {
            ("GET", "/api/v1/pairing/pending") if host_state.load(Ordering::SeqCst) != 0 => {
                (200, "[]")
            }
            ("POST", "/api/v1/pairing/pending/device-01/confirm") => {
                host_state.store(1, Ordering::SeqCst);
                (200, "{}")
            }
            ("POST", "/api/v1/pairing/pending/device-01/reject") => {
                host_state.store(2, Ordering::SeqCst);
                (200, "{}")
            }
            ("GET", "/api/v1/devices") => (200, r#"[{"id":"device-01","revokedAt":null}]"#),
            _ => pairing_reply(method, path),
        })
        .await;
        let mut output = Vec::new();
        let mut eof = pairing_console(std::io::Cursor::new(Vec::<u8>::new()));
        let (paired, approved) = tokio::join!(
            pair_with_local(
                &local,
                false,
                Some(Instant::now() + Duration::from_secs(60)),
                true,
                &mut output,
                &mut eof,
                &|| false
            ),
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                decide_with_local(&local, "device-01", true, Some("B36D1B62BB5F")).await
            }
        );
        paired.unwrap();
        approved.unwrap();
        let events: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events
                .iter()
                .map(|e| e["event"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["offer", "verification", "connected"]
        );
        assert_eq!(events[0]["hostKeyPin"], "fixture-pin");
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path.ends_with("/reject")));
    }

    #[tokio::test]
    async fn pairing_external_decision_does_not_steal_the_next_typed_answer() {
        let (read, mut write) = UnixStream::pair().unwrap();
        let mut console = pairing_console(std::io::BufReader::new(read));
        let mut first_prompt = console();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut first_prompt)
                .await
                .is_err()
        );
        // An external denial wins the select and drops this prompt. Its reader
        // must not swallow the yes intended for the next phone.
        drop(first_prompt);
        write.write_all(b"B36D1B62BB5F\n").unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(60), console())
                .await
                .unwrap()
                .unwrap(),
            Some("B36D1B62BB5F".into())
        );
        write.write_all(b"\n").unwrap();
        assert!(
            console().await.unwrap().is_none(),
            "an explicit empty line still means no"
        );
        drop(write);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), console())
                .await
                .is_err(),
            "EOF must wait for an external decision"
        );
    }

    #[tokio::test]
    async fn pair_approve_and_deny_answer_only_the_waiting_phone() {
        let (local, seen) = stub_daemon(pairing_reply).await;
        let connected = decide_with_local(&local, "device-01", true, Some("B36D1B62BB5F"))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(connected).unwrap(),
            json!({"event":"connected","deviceId":"device-01","label":"Fixture phone"})
        );
        let rejected = decide_with_local(&local, "device-01", false, None)
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(rejected).unwrap()["event"], "rejected");
        let error = decide_with_local(&local, "nobody", true, Some("B36D1B62BB5F"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no phone is waiting as nobody"));
        let paths: Vec<_> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.path.clone())
            .collect();
        assert_eq!(
            paths,
            [
                "/api/v1/pairing/pending",
                "/api/v1/pairing/pending/device-01/confirm",
                "/api/v1/pairing/pending",
                "/api/v1/pairing/pending/device-01/reject",
                "/api/v1/pairing/pending",
            ]
        );
    }

    // The CLI refuses an absent answer before making any request. Matching
    // and wrong answers are checked by the actual daemon's enrollment tests.
    #[tokio::test]
    async fn pair_approval_refuses_missing_phone_text() {
        for code in [None, Some("")] {
            let (local, seen) = stub_daemon(pairing_reply).await;
            let error = decide_with_local(&local, "device-01", true, code)
                .await
                .unwrap_err();
            assert!(error.to_string().starts_with("approval requires --code"));
            assert!(seen.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn pair_cancel_removes_only_its_registered_qr_and_preserves_files_on_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            data: directory.path().into(),
            state: directory.path().into(),
        };
        let url = "https://wonder.example.ts.net/pair#fixture";
        let a = directory.path().join("a.png");
        let b = directory.path().join("b.svg");
        let first = QrFile::create(&paths, "o1", &a, url).unwrap();
        let mut second = QrFile::create(&paths, "o2", &b, url).unwrap();
        assert_eq!(
            std::fs::metadata(pair_qr_record(&paths, "o1"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let (refused, _) = stub_daemon(|_, _| (500, "cancel refused")).await;
        assert!(cancel_pair_with_local(&refused, &paths, "o1")
            .await
            .is_err());
        assert!(a.exists() && b.exists());
        let (local, _) = stub_daemon(pairing_reply).await;
        cancel_pair_with_local(&local, &paths, "o1").await.unwrap();
        assert!(!a.exists() && !pair_qr_record(&paths, "o1").exists());
        assert!(b.exists() && pair_qr_record(&paths, "o2").exists());
        // A repeat cancel and the still-running first process's destructor
        // tolerate the already-removed image/record. Claim clears the other.
        cancel_pair_with_local(&local, &paths, "o1").await.unwrap();
        drop(first);
        second.clear().unwrap();
        assert!(!b.exists() && !pair_qr_record(&paths, "o2").exists());
        std::fs::write(&a, b"the person's existing file").unwrap();
        assert!(QrFile::create(&paths, "o1", &a, url).is_err());
        assert_eq!(std::fs::read(&a).unwrap(), b"the person's existing file");
    }

    #[tokio::test]
    async fn origin_wait_stops_at_its_limit_and_returns_once_the_origin_answers() {
        // Like Tailscale before its certificate: connections open, nothing answers.
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let silent_origin = format!("http://{}", silent.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = silent.accept().await {
                held.push(stream);
            }
        });
        let refused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused_origin = format!("http://{}", refused.local_addr().unwrap());
        drop(refused);
        for origin in [&silent_origin, &refused_origin] {
            let started = Instant::now();
            assert!(!wait_for_origin(origin, true, Duration::from_secs(2)).await);
            let took = started.elapsed();
            assert!(
                took >= Duration::from_secs(2) && took < Duration::from_secs(4),
                "{origin}: {took:?}"
            );
        }
        let answering = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let answering_origin = format!("http://{}", answering.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = answering.accept().await {
                let mut buffer = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await;
                let _ = tokio::io::AsyncWriteExt::write_all(
                    &mut stream,
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n",
                )
                .await;
            }
        });
        assert!(wait_for_origin(&answering_origin, true, Duration::from_secs(60)).await);
    }

    #[cfg(feature = "dev-pairing")]
    #[tokio::test]
    async fn pairing_yes_skips_only_the_prompt_and_timeout_and_errors_cancel_the_offer() {
        let (local, seen) = stub_daemon(pairing_reply).await;
        let mut output = Vec::new();
        pair_with_local(
            &local,
            true,
            Some(Instant::now() + Duration::from_secs(60)),
            true,
            &mut output,
            &mut || panic!("--yes must not ask"),
            &|| false,
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("B36D1B62BB5F"));
        assert!(!output.contains("verificationText"));
        let events: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.last().unwrap()["event"], "connected");
        let confirmation: Value = serde_json::from_str(
            &seen
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.path.ends_with("/confirm"))
                .unwrap()
                .body,
        )
        .unwrap();
        assert_eq!(confirmation["devApprove"], true);
        let (local, seen) = stub_daemon(|method, path| {
            if path == "/api/v1/pairing/pending" {
                (500, "pending failed")
            } else {
                pairing_reply(method, path)
            }
        })
        .await;
        let error = pair_with_local(
            &local,
            true,
            Some(Instant::now() + Duration::from_secs(60)),
            true,
            &mut Vec::new(),
            &mut || Box::pin(async { Ok(Some("B36D1B62BB5F".into())) }),
            &|| false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("pending failed"), "{error}");
        assert_eq!(
            seen.lock().unwrap().last().unwrap().path,
            "/api/v1/pairing/offers/o1/cancel"
        );
    }

    #[tokio::test]
    async fn pairing_timeout_covers_the_whole_command() {
        // A slow host status and an origin that never answers share one
        // limit: 3 s, not 1.5 s plus 3 s.
        let (local, _) = stub_daemon_after(Duration::from_millis(1500), |method, path| {
            if path == "/api/v1/host/status" {
                (200, r#"{"publicOrigin":"http://192.0.2.1"}"#)
            } else {
                pairing_reply(method, path)
            }
        })
        .await;
        let started = Instant::now();
        let error = pair_until(
            &local,
            true,
            Some(started + Duration::from_secs(3)),
            true,
            &mut Vec::new(),
            &mut || panic!("--yes must not ask"),
            &|| false,
        )
        .await
        .unwrap_err();
        let took = started.elapsed();
        assert!(
            error
                .to_string()
                .contains("did not answer before the timeout"),
            "{error}"
        );
        assert!(
            took >= Duration::from_secs(3) && took < Duration::from_secs(4),
            "{took:?}"
        );

        // A prompt nobody answers ends at the same limit and closes the offer.
        let (local, seen) = stub_daemon(|method, path| {
            if path == "/api/v1/host/status" {
                (200, "{}")
            } else {
                pairing_reply(method, path)
            }
        })
        .await;
        let started = Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            pair_until(
                &local,
                false,
                Some(started + Duration::from_secs(2)),
                true,
                &mut Vec::new(),
                &mut || Box::pin(std::future::pending()),
                &|| false,
            ),
        )
        .await
        .expect("the prompt outlived --timeout")
        .unwrap_err();
        let took = started.elapsed();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            took >= Duration::from_secs(2) && took < Duration::from_secs(3),
            "{took:?}"
        );
        let seen = seen.lock().unwrap();
        assert!(!seen.iter().any(|r| r.path.ends_with("/confirm")));
        assert_eq!(
            seen.last().unwrap().path,
            "/api/v1/pairing/offers/o1/cancel"
        );
    }

    #[tokio::test]
    async fn pairing_cancellation_before_confirmation_never_approves() {
        // Cancellation can arrive while pending is in flight, while emitting
        // verification for --yes, or while the human prompt is open.
        for stage in ["pending", "verification", "answer"] {
            let (local, seen) = interactive_pairing_daemon().await;
            let stopped = std::sync::Arc::new(AtomicBool::new(false));
            struct Output {
                bytes: Vec<u8>,
                stopped: std::sync::Arc<AtomicBool>,
                interrupt: bool,
            }
            impl Write for Output {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    self.bytes.extend_from_slice(bytes);
                    if self.interrupt
                        && String::from_utf8_lossy(&self.bytes)
                            .contains("Verification text from your phone")
                    {
                        self.stopped.store(true, Ordering::SeqCst);
                    }
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let mut output = Output {
                bytes: vec![],
                stopped: stopped.clone(),
                interrupt: stage == "verification",
            };
            let checks = std::cell::Cell::new(0);
            let error = pair_with_local(
                &local,
                false,
                Some(Instant::now() + Duration::from_secs(60)),
                false,
                &mut output,
                &mut || {
                    stopped.store(true, Ordering::SeqCst);
                    Box::pin(async { Ok(Some("B36D1B62BB5F".into())) })
                },
                &|| {
                    checks.set(checks.get() + 1);
                    stopped.load(Ordering::SeqCst) || (stage == "pending" && checks.get() >= 2)
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.to_string(), "pairing cancelled", "{stage}");
            let seen = seen.lock().unwrap();
            assert!(
                !seen.iter().any(|r| r.path.ends_with("/confirm")),
                "{stage}"
            );
            assert_eq!(
                seen.last().unwrap().path,
                "/api/v1/pairing/offers/o1/cancel"
            );
        }
    }

    #[tokio::test]
    async fn pairing_cancellation_encodes_the_id_and_is_idempotent() {
        let (local, seen) = stub_daemon(|_, _| (404, "gone")).await;
        cancel_offer(&local, "offer / one").await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].path,
            "/api/v1/pairing/offers/offer%20%2F%20one/cancel"
        );
        assert_eq!(seen[0].capability.as_deref(), Some("cap-1"));
    }

    #[tokio::test]
    async fn pairing_declining_never_confirms_and_still_cancels() {
        let (local, seen) = interactive_pairing_daemon().await;
        let mut output = Vec::new();
        let error = pair_with_local(
            &local,
            false,
            Some(Instant::now() + Duration::from_secs(1)),
            false,
            &mut output,
            &mut || Box::pin(async { Ok(None) }),
            &|| false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let seen = seen.lock().unwrap();
        assert!(!seen.iter().any(|r| r.path.ends_with("/confirm")));
        assert_eq!(
            seen.iter().filter(|r| r.path.ends_with("/reject")).count(),
            1
        );
        assert_eq!(
            seen.last().unwrap().path,
            "/api/v1/pairing/offers/o1/cancel"
        );
    }

    #[tokio::test]
    async fn pairing_interrupt_does_not_wait_for_an_open_stdin() {
        let (_send, receive) = tokio::sync::oneshot::channel();
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let interrupt = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let error = wait_for_pairing_answer(receive, || cancelled.load(Ordering::SeqCst))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "pairing cancelled");
        assert!(started.elapsed() < Duration::from_secs(60));
        interrupt.join().unwrap();
    }

    /// A Mac or Windows update keeps a replacement host that runs the new
    /// program and answers, even while no agent is ready; it is undone only
    /// when the host is not the new program or does not answer.
    #[tokio::test]
    async fn update_is_undone_when_the_new_host_dies_while_agents_start() {
        let short = Duration::from_millis(300);
        let answering = || async {
            stub_daemon(|_, path| match path {
                "/api/v1/host/status" => (200, "{}"),
                _ => (503, r#"{"detail":"No agent runtime is ready"}"#),
            })
            .await
            .0
        };

        // The supervisor is gone right after the host first answered.
        let local = answering().await;
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let gone_after_first = || match checks.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(()),
            _ => Err("it is not running".to_owned()),
        };
        let error = wait_updated_host(&local, gone_after_first, short, short)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("stopped"), "{error}");

        // The supervisor stays, but its daemon stops answering.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let (local, _) = stub_daemon(move |_, path| match path {
            "/api/v1/host/status" if seen.fetch_add(1, Ordering::SeqCst) == 0 => (200, "{}"),
            _ => (503, r#"{"detail":"stopped"}"#),
        })
        .await;
        let ours = || Ok(());
        let error = wait_updated_host(&local, ours, short, short)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("stopped"), "{error}");
        assert!(calls.load(Ordering::SeqCst) > 1);
    }

    #[tokio::test]
    async fn update_accepts_the_new_host_without_waiting_on_agents() {
        let this = std::env::current_exe().unwrap();
        let ours = || runs(Some(std::process::id()), &this);
        let short = Duration::from_millis(300);

        // Agents down or still starting: kept, and said separately.
        let (local, seen) = stub_daemon(|_, path| match path {
            "/api/v1/host/status" => (200, "{}"),
            _ => (503, r#"{"detail":"No agent runtime is ready"}"#),
        })
        .await;
        assert!(!wait_updated_host(&local, ours, short, short).await.unwrap());
        assert!(seen.lock().unwrap().iter().any(|r| r.path == "/readyz"));

        let (local, _) = stub_daemon(|_, path| match path {
            "/api/v1/host/status" | "/readyz" => (200, "{}"),
            _ => (404, "{}"),
        })
        .await;
        assert!(wait_updated_host(&local, ours, short, short).await.unwrap());

        // Something else answering for the host is not the update.
        let other = Path::new("/bin/sh");
        let error = wait_updated_host(
            &local,
            || runs(Some(std::process::id()), other),
            short,
            short,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("instead of /bin/sh"), "{error}");

        // The new program running without answering is not ready either.
        let (local, _) = stub_daemon(|_, _| (503, r#"{"detail":"starting"}"#)).await;
        let error = wait_updated_host(&local, ours, short, short)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not answer"), "{error}");
    }

    // Run the real supervisor in a separate test process so executable overrides,
    // signal handlers and STOPPING never leak into parallel tests.
    #[test]
    fn headless_supervisor_fixture() {
        if std::env::var_os("WONDER_SERVE_FIXTURE").is_none() {
            return;
        }
        let paths = Paths::resolve().unwrap();
        // `restarts` holds "limit first-delay-ms" for a test of crash
        // recovery; without it the fixture never replaces a crashed daemon.
        let restarts = std::fs::read_to_string(paths.data.join("restarts"))
            .map(|spec| {
                let mut parts = spec.split_whitespace().map(|part| part.parse().unwrap());
                let (limit, first) = (parts.next().unwrap(), parts.next().unwrap());
                CrashRestarts {
                    limit: limit as usize,
                    window: Duration::from_secs(600),
                    first_delay: Duration::from_millis(first),
                    max_delay: Duration::from_millis(first * 2),
                }
            })
            .unwrap_or(CrashRestarts::NEVER);
        // `stop-grace` holds the daemon's stop deadline in milliseconds.
        let stop_grace = std::fs::read_to_string(paths.data.join("stop-grace"))
            .map(|ms| Duration::from_millis(ms.trim().parse().unwrap()))
            .unwrap_or(DAEMON_STOP_GRACE);
        supervise_with(&paths, &load_config(&paths).unwrap(), restarts, stop_grace).unwrap();
    }

    /// The daemon's home network listener, run by the daemon fixture when
    /// a test asks for it: this computer's pinned key on the port the host
    /// passed down.
    #[test]
    fn headless_lan_listener_fixture() {
        let Some(port) = std::env::var_os("WONDER_LAN_FIXTURE_PORT") else {
            return;
        };
        let data = PathBuf::from(std::env::var_os("WONDER_DATA_DIR").unwrap());
        let port: u16 = port.to_str().unwrap().parse().unwrap();
        pinned_listener(
            &data,
            std::net::TcpListener::bind(("127.0.0.1", port)).unwrap(),
        );
        write_private(&data.join("lan-serving"), b"").unwrap();
        // The host stops the daemon's process group; this is a backstop.
        std::thread::sleep(Duration::from_secs(30));
    }

    struct ServeFixture {
        directory: tempfile::TempDir,
        host: OwnedChild,
    }

    impl Drop for ServeFixture {
        fn drop(&mut self) {
            if let Ok(mut stream) = UnixStream::connect(self.root().join("headless/control.sock")) {
                let _ = stream.write_all(b"stop\n");
                let deadline = Instant::now() + Duration::from_secs(60);
                while !self.host.exited().unwrap_or(true) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    impl ServeFixture {
        fn start() -> Self {
            Self::start_with_daemon(false)
        }

        fn start_with_daemon(ignore_term: bool) -> Self {
            Self::start_with(ignore_term, Value::Null)
        }

        fn start_with(ignore_term: bool, home_network: Value) -> Self {
            let fixture = Self::launch(ignore_term, home_network, &[]);
            fixture.wait("initial ready", || fixture.state()["state"] == "ready");
            fixture.wait("daemon fixture started", || {
                !fixture.read("descendant.pid").is_empty()
            });
            fixture
        }

        /// As `start_with_daemon`, with each (name, contents) of `files` in
        /// place first: `stop-grace` (ms) bounds the daemon's stop, and
        /// `drain` has the daemon take a second to stop while a helper in
        /// its process group, as Codex is, notes any SIGTERM it receives.
        fn start_prepared(ignore_term: bool, files: &[(&str, &str)]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            for (name, contents) in files {
                std::fs::write(directory.path().join(name), contents).unwrap();
            }
            let fixture = Self::launch_in(directory, ignore_term, Value::Null, &[]);
            fixture.wait("initial ready", || fixture.state()["state"] == "ready");
            fixture.wait("daemon fixture started", || {
                !fixture.read("descendant.pid").is_empty()
            });
            fixture
        }

        /// Starts the host with `files` already present, without waiting.
        fn launch(ignore_term: bool, home_network: Value, files: &[&str]) -> Self {
            Self::launch_in(
                tempfile::tempdir().unwrap(),
                ignore_term,
                home_network,
                files,
            )
        }

        /// As `start`, replacing up to `limit` crashed daemons after
        /// `first_delay_ms`, then twice that. Each daemon also starts a
        /// provider bridge stand-in in a process group of its own.
        fn start_restarting(limit: usize, first_delay_ms: u64) -> Self {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("bridge"), "").unwrap();
            std::fs::write(
                directory.path().join("restarts"),
                format!("{limit} {first_delay_ms}"),
            )
            .unwrap();
            let fixture = Self::launch_in(directory, false, Value::Null, &[]);
            fixture.wait("initial ready", || fixture.state()["state"] == "ready");
            fixture.wait("daemon fixture started", || fixture.daemon_started());
            fixture
        }

        fn daemon_started(&self) -> bool {
            !self.read("descendant.pid").is_empty() && !self.read("bridge.pid").is_empty()
        }

        /// Kills the running daemon fixture, as a crash would, and returns
        /// its pid, its descendant's and its bridge's.
        fn crash_daemon(&self) -> (String, String, String) {
            let pids = (
                self.read("daemon.pid"),
                self.read("descendant.pid"),
                self.read("bridge.pid"),
            );
            for file in ["descendant.pid", "bridge.pid"] {
                std::fs::remove_file(self.root().join(file)).unwrap();
            }
            unsafe { libc::kill(pids.0.trim().parse().unwrap(), libc::SIGKILL) };
            pids
        }

        /// As `launch`, with a daemon that serves the home network port.
        fn launch_with_lan(home_network: Value, files: &[&str]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let server = directory.path().join("lan-server");
            std::fs::write(
                &server,
                format!(
                    "#!/bin/sh\nWONDER_LAN_FIXTURE_PORT=\"$1\" exec '{}' --exact tests::headless_lan_listener_fixture --nocapture\n",
                    std::env::current_exe().unwrap().display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self::launch_in(directory, false, home_network, files)
        }

        fn launch_in(
            directory: tempfile::TempDir,
            ignore_term: bool,
            home_network: Value,
            files: &[&str],
        ) -> Self {
            let root = directory.path();
            let paths = Paths {
                data: root.into(),
                state: root.join("headless"),
            };
            std::fs::create_dir_all(&paths.state).unwrap();
            write_private(
                &paths.config(),
                json!({"tunnel": "tailscale", "httpsPort": 8444,
                    "listenAddr": "127.0.0.1:3777", "homeNetwork": home_network})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
            write_private(&paths.capability(), b"fixture-only").unwrap();
            let identity = json!({"BackendState": "Running",
                "Self": {"ID": "owned-node", "DNSName": "owned.example.ts.net."}});
            write_private(&root.join("identity.json"), identity.to_string().as_bytes()).unwrap();
            let owned = json!({"TCP": {"8444": {"HTTPS": true}},
                "Web": {"owned.example.ts.net:8444": {"Handlers": {
                    "/": {"Proxy": "http://127.0.0.1:3777"}}}}});
            for file in ["serve.json", "owned.json"] {
                write_private(&root.join(file), owned.to_string().as_bytes()).unwrap();
            }
            let tailscale = root.join("tailscale");
            std::fs::write(
                &tailscale,
                r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
printf '%s\n' "$*" >> calls
case "$*" in
  'status --json')
    if [ -e hang ]; then echo $$ > probe.pid; exec sleep 60; fi
    if [ -e unavailable ]; then exit 1; fi
    # The Mac app's command prints nothing while Tailscale is stopped.
    if [ -e stopped ]; then exit 0; fi
    cat identity.json ;;
  'serve status --json')
    if [ -e inspect-gate ] || { [ -e verify-gate ] && [ -e mutations ]; }; then
      touch verifying
      while [ -e inspect-gate ] || [ -e verify-gate ]; do sleep 0.02; done
    fi
    cat serve.json ;;
  'serve --bg --yes --https=8444 http://127.0.0.1:3777')
    printf '%s\n' "$*" >> mutations
    if [ ! -e no-apply ]; then cp owned.json serve.json; fi ;;
  *) exit 99 ;;
esac
"#,
            )
            .unwrap();
            let daemon = root.join("daemon");
            std::fs::write(
                &daemon,
                format!("#!/bin/sh\n{}\ncd \"$WONDER_DATA_DIR\" || exit 1\necho \"${{WONDER_LAN_PORT:-}}\" > lan-port\nif [ -x lan-server ] && [ -n \"${{WONDER_LAN_PORT:-}}\" ]; then ./lan-server \"$WONDER_LAN_PORT\" & fi\necho daemon fixture output\necho daemon fixture warning >&2\necho $$ > daemon.pid\nif [ -e drain ]; then trap 'sleep 1; echo drained > drained; exit 0' TERM; sh -c 'trap \"echo term > helper-term; exit 0\" TERM; while :; do sleep 0.05; done' & echo $! > helper.pid; fi\nif [ -e bridge ]; then python3 -c 'import os; os.setpgid(0, 0); open(\"bridge.pid\", \"w\").write(str(os.getpid())); os.execvp(\"sleep\", [\"sleep\", \"60\"])' & fi\nsleep 60 &\necho $! > descendant.pid\nwait\n",
                    if ignore_term { "trap '' TERM" } else { "" }),
            )
            .unwrap();
            for script in [&tailscale, &daemon] {
                std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            for file in files {
                write_private(&root.join(file), b"").unwrap();
            }
            let log = std::fs::File::create(root.join("host.log")).unwrap();
            let host = OwnedChild::spawn(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "tests::headless_supervisor_fixture",
                        "--nocapture",
                    ])
                    .env("WONDER_SERVE_FIXTURE", "1")
                    // Exported in the host's shell: only `wonder lan on` may
                    // turn the home network listener on.
                    .env("WONDER_LAN_PORT", "9")
                    .env("WONDER_DATA_DIR", root)
                    .env("TAILSCALE_BIN", tailscale)
                    .env_remove("TAILSCALE_SOCKET")
                    .env("WONDER_DAEMON_BIN", daemon)
                    .stdout(log.try_clone().unwrap())
                    .stderr(log),
            )
            .unwrap();
            Self { directory, host }
        }

        fn root(&self) -> &Path {
            self.directory.path()
        }

        fn set(&self, file: &str, value: &str) {
            write_private(&self.root().join(file), value.as_bytes()).unwrap();
        }

        fn read(&self, file: &str) -> String {
            std::fs::read_to_string(self.root().join(file)).unwrap_or_default()
        }

        fn state(&self) -> Value {
            self.read("headless/origin-status.jsonl")
                .lines()
                .last()
                .and_then(|line| serde_json::from_str(line).ok())
                .unwrap_or(Value::Null)
        }

        fn wait(&self, label: &str, predicate: impl Fn() -> bool) {
            self.wait_for(label, Duration::from_secs(60), predicate);
        }

        fn wait_for(&self, label: &str, timeout: Duration, predicate: impl Fn() -> bool) {
            let start = Instant::now();
            while !predicate() {
                assert!(
                    start.elapsed() < timeout,
                    "{label} timed out; origin={} calls={} log={}",
                    self.read("headless/origin-status.jsonl"),
                    self.read("calls"),
                    self.read("host.log")
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn write_pid(&self, file: &str, pid: &str) {
            std::fs::write(self.root().join(file), pid).unwrap();
        }

        fn assert_process_gone(&self, file: &str) {
            let pid = self.read(file);
            assert!(!pid.trim().is_empty(), "missing fixture pid: {file}");
            self.wait("fixture process cleanup", || process_gone(&pid));
        }

        fn stop(&self) {
            self.stop_with_budget(Duration::from_secs(2));
        }

        fn stop_with_budget(&self, budget: Duration) {
            let start = Instant::now();
            UnixStream::connect(self.root().join("headless/control.sock"))
                .unwrap()
                .write_all(b"stop\n")
                .unwrap();
            self.wait_for("stop", budget, || self.host.exited().unwrap());
            assert!(start.elapsed() < budget);
            assert_eq!(self.state()["state"], "stopped");
            println!("fixture stop: {:?}", start.elapsed());
        }
    }

    /// A listener on 127.0.0.1 that completes TLS handshakes with the host key
    /// in `key_dir`, as the daemon's home network listener does.
    fn pinned_listener(key_dir: &Path, listener: std::net::TcpListener) {
        let key = wonderd::lan::load_or_create_host_key(key_dir).unwrap();
        let config = wonderd::lan::tls_config(&key).unwrap();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let Ok(mut tls) = rustls::ServerConnection::new(config.clone()) else {
                    continue;
                };
                while tls.is_handshaking() {
                    if tls.complete_io(&mut stream).is_err() {
                        break;
                    }
                }
            }
        });
    }

    fn free_listener() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    /// Each way a listener can fail a phone gets its own result; only another
    /// key means another program holds the port.
    #[test]
    fn home_listener_probe_names_each_failure() {
        let mine = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let spki = wonderd::lan::load_or_create_host_key(mine.path())
            .unwrap()
            .public_key_spki();
        let at = |port| std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let once = |port| home_listener_attempt(&spki, at(port), Duration::from_millis(300));

        let (listener, port) = free_listener();
        pinned_listener(mine.path(), listener);
        assert_eq!(
            home_listener_answers(&spki, at(port)),
            Ok(()),
            "the host's listener"
        );

        let (listener, port) = free_listener();
        pinned_listener(other.path(), listener);
        assert_eq!(once(port), Err(ProbeFailure::WrongKey), "another key");
        let start = Instant::now();
        assert_eq!(
            home_listener_answers(&spki, at(port)),
            Err(ProbeFailure::WrongKey)
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "another key is final"
        );

        // A listener that accepts but never speaks TLS.
        let (silent, port) = free_listener();
        assert_eq!(once(port), Err(ProbeFailure::Timeout), "a silent listener");
        drop(silent);

        // A program that answers in plain text and closes.
        let (plain, port) = free_listener();
        std::thread::spawn(move || {
            for mut stream in plain.incoming().flatten() {
                let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            }
        });
        assert_eq!(once(port), Err(ProbeFailure::TlsError), "plain text");

        let (closed, port) = free_listener();
        drop(closed);
        assert_eq!(once(port), Err(ProbeFailure::Refused), "nothing listening");
    }

    /// A listener that comes up within about two seconds still counts.
    #[test]
    fn home_listener_probe_retries_a_listener_that_is_starting() {
        let mine = tempfile::tempdir().unwrap();
        let spki = wonderd::lan::load_or_create_host_key(mine.path())
            .unwrap()
            .public_key_spki();
        let (closed, port) = free_listener();
        drop(closed);
        let key_dir = mine.path().to_owned();
        let late = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
            pinned_listener(&key_dir, listener);
        });
        let start = Instant::now();
        let result =
            home_listener_answers(&spki, std::net::SocketAddr::from(([127, 0, 0, 1], port)));
        late.join().unwrap();
        assert_eq!(result, Ok(()));
        assert!(start.elapsed() >= Duration::from_millis(500));

        // Still refused after the retries: about two seconds, then Refused.
        let (closed, port) = free_listener();
        drop(closed);
        let start = Instant::now();
        assert_eq!(
            home_listener_answers(&spki, std::net::SocketAddr::from(([127, 0, 0, 1], port))),
            Err(ProbeFailure::Refused)
        );
        let elapsed = start.elapsed();
        assert!(
            (Duration::from_millis(1400)..Duration::from_millis(2600)).contains(&elapsed),
            "{elapsed:?}"
        );
    }

    /// A certificate that carries the host key's public bytes in an extension
    /// but whose own key is another: a phone pinning the host key rejects it.
    #[test]
    fn home_listener_ignores_the_host_key_embedded_in_another_certificate() {
        let mine = tempfile::tempdir().unwrap();
        let spki = wonderd::lan::load_or_create_host_key(mine.path())
            .unwrap()
            .public_key_spki();
        let other = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = rcgen::CertificateParams::default();
        params
            .custom_extensions
            .push(rcgen::CustomExtension::from_oid_content(
                &[1, 3, 6, 1, 4, 1, 99999, 1],
                spki.clone(),
            ));
        let certificate = params.self_signed(&other).unwrap();
        assert!(
            certificate
                .der()
                .windows(spki.len())
                .any(|window| window == spki),
            "the decoy carries the host key's bytes"
        );
        let config = std::sync::Arc::new(
            rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(other.serialize_der().into()),
            )
            .unwrap(),
        );
        let (listener, port) = free_listener();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut tls = rustls::ServerConnection::new(config.clone()).unwrap();
                while tls.is_handshaking() {
                    if tls.complete_io(&mut stream).is_err() {
                        break;
                    }
                }
            }
        });
        assert_eq!(
            home_listener_answers(&spki, std::net::SocketAddr::from(([127, 0, 0, 1], port))),
            Err(ProbeFailure::WrongKey)
        );
    }

    /// A listener that trickles bytes never holds the probe past its deadline.
    #[test]
    fn home_listener_probe_ends_at_one_deadline() {
        let (listener, port) = free_listener();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    // A TLS record header announcing a long handshake message,
                    // then one byte every 100 ms.
                    let _ = stream.write_all(&[0x16, 0x03, 0x03, 0x40, 0x00]);
                    while stream.write_all(&[0]).is_ok() {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                });
            }
        });
        let start = Instant::now();
        assert_eq!(
            home_listener_attempt(
                &[0; 91],
                std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                HOME_PROBE_ATTEMPT
            ),
            Err(ProbeFailure::Timeout)
        );
        let elapsed = start.elapsed();
        println!("trickling listener probe: {elapsed:?}");
        assert!(elapsed < Duration::from_millis(1200), "{elapsed:?}");
    }

    /// `cupola routes` asks every home address at once: the slowest answer,
    /// not the sum, sets the time, and each address keeps its own result.
    #[test]
    fn home_addresses_are_probed_at_once_and_keep_their_order() {
        let addresses: Vec<std::net::IpAddr> =
            ["192.168.1.2", "10.0.0.2", "172.16.0.2", "192.168.1.3"]
                .iter()
                .map(|ip| ip.parse().unwrap())
                .collect();
        let (active, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let answers = |ip: std::net::IpAddr, _port: u16| {
            most.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            active.fetch_sub(1, Ordering::SeqCst);
            match ip.to_string().as_str() {
                "192.168.1.2" => Ok(()),
                "10.0.0.2" => Err(ProbeFailure::Refused),
                "172.16.0.2" => Err(ProbeFailure::WrongKey),
                _ => Ok(()),
            }
        };
        let start = Instant::now();
        let results = probe_home_addresses(&addresses, 4000, &answers);
        let elapsed = start.elapsed();
        println!("four 300 ms home probes: {elapsed:?}");
        assert_eq!(
            results,
            [
                Ok(()),
                Err(ProbeFailure::Refused),
                Err(ProbeFailure::WrongKey),
                Ok(())
            ]
        );
        // Every probe was in flight together; one after another would peak at 1.
        assert_eq!(most.load(Ordering::SeqCst), addresses.len());

        let config = json!({"homeNetwork": {"enabled": true, "port": 4000}});
        let published = json!({"state": "ready", "homePort": 4000});
        let (_, route) = home_route(&config, Some(&published), &addresses, &answers, "R");
        assert_eq!(route.state, RouteState::Ready);
        assert_eq!(
            route.origins,
            [
                wonderd::lan::origin(addresses[0], 4000),
                wonderd::lan::origin(addresses[3], 4000)
            ]
        );
        let all_down = |_: std::net::IpAddr, _: u16| Err(ProbeFailure::Refused);
        let (_, route) = home_route(&config, Some(&published), &addresses, &all_down, "R");
        assert_eq!(route.state, RouteState::Unavailable);
    }

    /// Several addresses that all stall share the one retry window, so
    /// `routes` ends in about 2 seconds however many addresses there are.
    #[test]
    fn stalled_home_addresses_share_one_deadline() {
        let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = stream.write_all(&[0x16, 0x03, 0x03, 0x40, 0x00]);
                    while stream.write_all(&[0]).is_ok() {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                });
            }
        });
        let addresses: Vec<std::net::IpAddr> = [
            "127.0.0.1",
            "127.0.0.2",
            "127.0.0.3",
            "127.0.0.4",
            "127.0.0.5",
            "127.0.0.6",
        ]
        .iter()
        .map(|ip| ip.parse().unwrap())
        .collect();
        let answers =
            |ip, port| home_listener_answers(&[0; 91], std::net::SocketAddr::new(ip, port));
        let start = Instant::now();
        let results = probe_home_addresses(&addresses, port, &answers);
        let elapsed = start.elapsed();
        println!("six stalled home addresses: {elapsed:?}");
        assert!(
            results.iter().all(|r| *r == Err(ProbeFailure::Timeout)),
            "{results:?}"
        );
        // One 2 s window; six in a row would take 12 s.
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    }

    /// `cupola doctor` runs its probes at once but lists their checks in step
    /// order, exactly as running them one after another would.
    #[test]
    fn doctor_steps_run_at_once_and_keep_their_order() {
        static ACTIVE: AtomicUsize = AtomicUsize::new(0);
        static MOST: AtomicUsize = AtomicUsize::new(0);
        fn fixed(id: &'static str, status: CheckStatus, delay_ms: u64) -> DoctorStep<'static> {
            Box::new(move || {
                MOST.fetch_max(ACTIVE.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(delay_ms));
                ACTIVE.fetch_sub(1, Ordering::SeqCst);
                Ok(vec![Check {
                    id,
                    status,
                    summary: format!("{id} summary"),
                    fix: (status == CheckStatus::Failed).then(|| format!("fix {id}")),
                }])
            })
        }
        fn steps() -> Vec<DoctorStep<'static>> {
            vec![
                fixed("slow", CheckStatus::Ok, 300),
                fixed("failing", CheckStatus::Failed, 50),
                Box::new(|| Ok(Vec::new())),
                fixed("fast", CheckStatus::Warning, 0),
                Box::new(|| {
                    Ok(vec![
                        Check {
                            id: "pair-a",
                            status: CheckStatus::Ok,
                            summary: "a".into(),
                            fix: None,
                        },
                        Check {
                            id: "pair-b",
                            status: CheckStatus::Unavailable,
                            summary: "b".into(),
                            fix: None,
                        },
                    ])
                }),
            ]
        }
        let render = |checks: &[Check]| serde_json::to_string_pretty(checks).unwrap();

        let mut sequential = Vec::new();
        let start = Instant::now();
        for step in steps() {
            sequential.extend(step().unwrap());
        }
        let one_by_one = start.elapsed();
        let start = Instant::now();
        let together = run_doctor_steps(steps()).unwrap();
        let at_once = start.elapsed();
        println!("doctor stubs: one by one {one_by_one:?}, at once {at_once:?}");
        assert_eq!(render(&together), render(&sequential));
        assert_eq!(
            together.iter().map(|c| c.id).collect::<Vec<_>>(),
            ["slow", "failing", "fast", "pair-a", "pair-b"]
        );
        // The slow, failing and fast stubs were in flight together; the
        // sequential run above peaked at 1 and shared the counter first.
        assert!(
            MOST.load(Ordering::SeqCst) >= 3,
            "{}",
            MOST.load(Ordering::SeqCst)
        );
    }

    /// A step that cannot report at all fails the command, as the first
    /// failing step did when they ran in sequence.
    #[test]
    fn a_doctor_step_that_cannot_report_fails_the_command() {
        let steps: Vec<DoctorStep> = vec![
            Box::new(|| Ok(Vec::new())),
            Box::new(|| Err("launchctl failed".into())),
            Box::new(|| Err("later failure".into())),
        ];
        let error = run_doctor_steps(steps).unwrap_err();
        assert_eq!(error.to_string(), "launchctl failed");
    }

    #[test]
    fn home_network_choice_reaches_the_daemon_and_wonder_routes() {
        let home = ["127.0.0.1".parse().unwrap()];
        let (listener, port) = free_listener();
        let fixture = ServeFixture::start_with(false, json!({"enabled": true, "port": port}));
        assert_eq!(fixture.read("lan-port").trim(), port.to_string());
        assert_eq!(fixture.state()["homePort"], port);
        let paths = Paths {
            data: fixture.root().into(),
            state: fixture.root().join("headless"),
        };
        let probe = home_listener_probe(&paths);
        let mut config = load_config(&paths).unwrap();
        let published = published_status(&paths).expect("the supervisor runs");

        // The fixture daemon is a shell script that binds nothing: the started
        // port alone is not readiness.
        let (_, route) = home_route(&config, Some(&published), &home, &probe, "R");
        assert_eq!(
            (route.state, route.origins),
            (RouteState::Unavailable, vec![])
        );
        // The test holds the port bound without accepting, so the probe
        // waits out its retries; only another key blames another program.
        let detail = route.detail.unwrap();
        assert!(detail.contains("within 2 seconds"), "{detail}");
        assert!(!detail.contains("Another program"), "{detail}");

        pinned_listener(fixture.root(), listener);
        let relay = relay_route(&wonderd::relay::RelayView::default(), true);
        let routes = all_routes(&config, Some(&published), &home, &probe, "R", relay);
        let rows: Vec<_> = routes
            .iter()
            .map(|route| (route.kind, route.state, route.origins.clone()))
            .collect();
        assert_eq!(
            rows,
            [
                (
                    RouteKind::Home,
                    RouteState::Ready,
                    vec![format!("https://127.0.0.1:{port}")]
                ),
                (
                    RouteKind::Tailscale,
                    RouteState::Ready,
                    vec!["https://owned.example.ts.net:8444".to_owned()]
                ),
                (RouteKind::Relay, RouteState::Off, vec![]),
            ]
        );
        // Turned off while running: the live listener stays until a restart.
        config["homeNetwork"]["enabled"] = json!(false);
        let (_, off) = home_route(&config, Some(&published), &home, &probe, "R");
        assert_eq!(off.state, RouteState::RestartNeeded);
        fixture.stop();
        assert!(published_status(&paths).is_none());
        assert_eq!(
            home_route(&config, None, &home, &probe, "R").1.state,
            RouteState::Off
        );

        // Turned off (`cupola lan off`), with WONDER_LAN_PORT exported to the host.
        let off = ServeFixture::start_with(false, json!({"enabled": false, "port": port}));
        assert_eq!(off.read("lan-port").trim(), "");
        assert!(off.state()["homePort"].is_null());
        off.stop();
        // No choice saved: the host serves the home network on the default port.
        let plain = ServeFixture::start();
        assert_eq!(
            plain.read("lan-port").trim(),
            cli::DEFAULT_LAN_PORT.to_string()
        );
        assert_eq!(plain.state()["homePort"], cli::DEFAULT_LAN_PORT);
        plain.stop();
    }

    #[test]
    fn home_route_state_follows_the_choice_the_host_and_the_addresses() {
        let on = |port: u16| json!({"tunnel": "tailscale", "homeNetwork": {"enabled": true, "port": port}});
        let off = json!({"tunnel": "tailscale", "homeNetwork": {"enabled": false, "port": 3778}});
        let never = json!({"tunnel": "tailscale"});
        let line = |state: &str, port: Option<u16>| -> Value {
            serde_json::from_str(origin_line(state, None, port, Some("tailscale")).trim()).unwrap()
        };
        let home: Vec<std::net::IpAddr> =
            vec!["192.168.1.20".parse().unwrap(), "fd12::1".parse().unwrap()];
        let both = vec![
            "https://192.168.1.20:3778".to_owned(),
            "https://[fd12::1]:3778".to_owned(),
        ];
        use RouteState::*;
        type Case<'a> = (
            &'a str,
            Value,
            Option<Value>,
            &'a [std::net::IpAddr],
            RouteState,
            Vec<String>,
        );
        let cases: Vec<Case> = vec![
            // No choice saved yet: same Wi-Fi is on, except on Windows (no listener).
            (
                "never chosen, host stopped",
                never.clone(),
                None,
                &home,
                if cfg!(windows) { Off } else { Stopped },
                if cfg!(windows) { vec![] } else { both.clone() },
            ),
            (
                "never chosen, serving",
                never.clone(),
                Some(line("ready", (!cfg!(windows)).then_some(3778))),
                &home,
                if cfg!(windows) { Off } else { Ready },
                if cfg!(windows) { vec![] } else { both.clone() },
            ),
            ("off, host stopped", off.clone(), None, &home, Off, vec![]),
            (
                "off, host without it",
                off.clone(),
                Some(line("ready", None)),
                &home,
                Off,
                vec![],
            ),
            (
                "off, host still serving",
                off.clone(),
                Some(line("ready", Some(3778))),
                &home,
                RestartNeeded,
                vec![],
            ),
            (
                "on, host stopped",
                on(3778),
                None,
                &home,
                Stopped,
                both.clone(),
            ),
            (
                "on, serving",
                on(3778),
                Some(line("ready", Some(3778))),
                &home,
                Ready,
                both.clone(),
            ),
            (
                "on, Tailscale recovering",
                on(3778),
                Some(line("unavailable", Some(3778))),
                &home,
                Ready,
                both.clone(),
            ),
            (
                "on, daemon not started",
                on(3778),
                Some(line("starting", None)),
                &home,
                Starting,
                both.clone(),
            ),
            (
                "on, host without it",
                on(3778),
                Some(line("ready", None)),
                &home,
                RestartNeeded,
                both.clone(),
            ),
            (
                "on, other port live",
                on(3778),
                Some(line("ready", Some(4443))),
                &home,
                RestartNeeded,
                both.clone(),
            ),
            (
                "on, no home address",
                on(3778),
                Some(line("ready", Some(3778))),
                &[],
                Unavailable,
                vec![],
            ),
        ];
        let all = |_: std::net::IpAddr, _: u16| Ok(());
        for (name, config, published, addresses, state, origins) in cases {
            let (port, route) = home_route(&config, published.as_ref(), addresses, &all, "R");
            assert_eq!(port, 3778, "{name}");
            assert_eq!((route.state, route.origins), (state, origins), "{name}");
            assert_eq!(route.enabled, home_port(&config).is_some(), "{name}");
            assert_eq!(
                route.detail.is_some(),
                matches!(state, RestartNeeded | Unavailable),
                "{name}: only a state that needs action explains itself"
            );
        }
        // The host started with the port, but its listener answers on fewer
        // addresses (or none, when another program holds the port).
        let serving = line("ready", Some(3778));
        let only_v4 = |ip: std::net::IpAddr, _: u16| {
            if ip.is_ipv4() {
                Ok(())
            } else {
                Err(ProbeFailure::WrongKey)
            }
        };
        let (_, partial) = home_route(&on(3778), Some(&serving), &home, &only_v4, "R");
        assert_eq!(
            (partial.state, partial.origins),
            (Ready, vec!["https://192.168.1.20:3778".to_owned()])
        );
        // No address answers: the most telling failure explains it, and only
        // another key blames another program.
        for (v4, v6, phrase, restart) in [
            (
                ProbeFailure::Refused,
                ProbeFailure::Refused,
                "not listening",
                true,
            ),
            (
                ProbeFailure::Refused,
                ProbeFailure::Timeout,
                "within 2 seconds",
                false,
            ),
            (
                ProbeFailure::Timeout,
                ProbeFailure::TlsError,
                "secure connection",
                true,
            ),
            (
                ProbeFailure::WrongKey,
                ProbeFailure::Refused,
                "Another program",
                true,
            ),
        ] {
            let failing = |ip: std::net::IpAddr, _: u16| Err(if ip.is_ipv4() { v4 } else { v6 });
            let (_, route) = home_route(&on(3778), Some(&serving), &home, &failing, "R");
            assert_eq!((route.state, route.origins), (Unavailable, vec![]));
            let detail = route.detail.unwrap();
            assert!(detail.contains(phrase), "{v4:?}/{v6:?}: {detail}");
            assert_eq!(
                detail.to_lowercase().contains("another program"),
                phrase == "Another program",
                "{detail}"
            );
            assert_eq!(detail.ends_with("R"), restart, "{detail}");
        }
    }

    /// `cupola routes` and `cupola lan status` through their command functions,
    /// with a temp data dir, a supervisor holding its lock and a real TLS
    /// listener: host stopped, running, Tailscale off, another key, nothing
    /// listening.
    #[test]
    fn routes_and_lan_status_agree_with_the_running_host() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::at(Some(directory.path().into())).unwrap();
        std::fs::create_dir_all(&paths.state).unwrap();
        let (listener, port) = free_listener();
        pinned_listener(&paths.data, listener);
        let pin = wonderd::lan::load_or_create_host_key(&paths.data)
            .unwrap()
            .pin();
        let configure = |port: u16| {
            save_config(
                &paths,
                &json!({
                    "tunnel": "tailscale",
                    "listenAddr": "127.0.0.1:3777",
                    "homeNetwork": {"enabled": true, "port": port},
                }),
            )
            .unwrap();
        };
        let publish = |line: String| {
            std::fs::write(paths.origin_status(), line).unwrap();
        };
        let home = [std::net::IpAddr::from([127, 0, 0, 1])];
        let run = |command: &str| -> (i32, String) {
            let mut out = Vec::new();
            let result = match command {
                "routes" => routes_at(&paths, &home, "R.", false, &mut out),
                _ => lan_at(LanCommand::Status, &paths, &home, "R.", false, &mut out),
            };
            let exit = result
                .err()
                .map_or(0, |error| ErrorOutput::from_error(error.as_ref()).1);
            (exit, String::from_utf8(out).unwrap())
        };
        let relay = format!("Relay         off\n              {RELAY_OFF_HINT}\n");
        let key = format!("Computer key  {pin}\n");

        configure(port);
        publish(origin_line(
            "ready",
            Some("https://studio.example.ts.net"),
            Some(port),
            Some("tailscale"),
        ));
        // A status file left by a host that has stopped: no supervisor holds the lock.
        assert_eq!(
            run("routes"),
            (
                0,
                format!(
                    "Home network  host not running  https://127.0.0.1:{port}\n\
                     Tailscale     host not running\n{relay}{key}"
                )
            ),
            "host stopped"
        );
        assert_eq!(
            run("lan"),
            (
                0,
                format!(
                    "Home network connections are on, at 127.0.0.1:{port} once the host runs (`cupola start`).\n\
                     Computer key: {pin}\n"
                )
            ),
            "host stopped"
        );

        let _supervisor = supervisor_lock(&paths).unwrap();
        write_private(
            &paths.supervisor_pid(),
            std::process::id().to_string().as_bytes(),
        )
        .unwrap();
        assert_eq!(
            run("routes"),
            (
                0,
                format!(
                    "Home network  ready             https://127.0.0.1:{port}\n\
                     Tailscale     ready             https://studio.example.ts.net\n{relay}{key}"
                )
            ),
            "host running"
        );
        assert_eq!(
            run("lan"),
            (
                0,
                format!(
                    "Home network connections are on. iPhones on this network can connect at 127.0.0.1:{port}.\n\
                     Computer key: {pin}\n"
                )
            ),
            "host running"
        );

        publish(origin_line(
            "unavailable",
            None,
            Some(port),
            Some("tailscale"),
        ));
        assert_eq!(
            run("routes"),
            (
                0,
                format!(
                    "Home network  ready             https://127.0.0.1:{port}\n\
                     Tailscale     unavailable\n              \
                     Tailscale is not answering on this computer. Check `tailscale status`; the host keeps retrying.\n\
                     {relay}{key}"
                )
            ),
            "Tailscale off"
        );
        assert_eq!(run("lan").0, 0);
        assert!(run("lan").1.contains("can connect at"), "Tailscale off");

        let other = tempfile::tempdir().unwrap();
        let (listener, taken) = free_listener();
        pinned_listener(other.path(), listener);
        configure(taken);
        publish(origin_line(
            "ready",
            Some("https://studio.example.ts.net"),
            Some(taken),
            Some("tailscale"),
        ));
        let another = format!(
            "Another program is using port {taken} on this computer's home network addresses. \
             Choose another port with `cupola lan on --port <port>`. R."
        );
        assert_eq!(
            run("routes"),
            (
                0,
                format!(
                    "Home network  unavailable\n              {another}\n\
                     Tailscale     ready             https://studio.example.ts.net\n{relay}{key}"
                )
            ),
            "wrong key"
        );
        assert_eq!(
            run("lan"),
            (
                0,
                format!("Home network connections are on.\n{another}\nComputer key: {pin}\n")
            ),
            "wrong key"
        );

        // Nothing listening: its own wording, never "another program".
        let (closed, refused) = free_listener();
        drop(closed);
        configure(refused);
        publish(origin_line("ready", None, Some(refused), Some("tailscale")));
        let (exit, out) = run("lan");
        assert_eq!(exit, 0);
        assert!(out.contains("is not listening on port"), "{out}");
        assert!(!out.contains("Another program"), "{out}");

        // A broken config.json fails both commands.
        std::fs::write(paths.config(), "{").unwrap();
        assert_eq!(run("routes").0, 1);
        assert_eq!(run("lan").0, 1);
    }

    #[test]
    fn routes_list_tailscale_as_one_route_among_several() {
        let custom = json!({"tunnel": "none", "origin": "https://wonder.example",
            "homeNetwork": {"enabled": false, "port": 3778}});
        let kinds = |routes: Vec<RouteStatus>| {
            routes
                .into_iter()
                .map(|route| (route.kind, route.enabled, route.state, route.origins))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds(all_routes(
                &custom,
                None,
                &[],
                &|_, _| Err(ProbeFailure::Refused),
                "R",
                relay_route(&wonderd::relay::RelayView::default(), false),
            )),
            [
                (RouteKind::Home, false, RouteState::Off, vec![]),
                (
                    RouteKind::Custom,
                    true,
                    RouteState::Stopped,
                    vec!["https://wonder.example".to_owned()]
                ),
                (RouteKind::Tailscale, false, RouteState::Off, vec![]),
                (RouteKind::Relay, false, RouteState::Off, vec![]),
            ]
        );
        let tailscale = json!({"tunnel": "tailscale"});
        let down: Value =
            serde_json::from_str(origin_line("unavailable", None, None, Some("tailscale")).trim())
                .unwrap();
        let main = main_route(&tailscale, Some(&down));
        assert_eq!(
            (main.kind, main.state),
            (RouteKind::Tailscale, RouteState::Unavailable)
        );
        assert!(main.detail.unwrap().contains("tailscale status"));

        // `routes` describes the running host: Tailscale turned off in
        // config.json after the host started still reads as Tailscale until
        // a restart, and the other way round.
        let ready = |tunnel: &str, origin: &str| -> Value {
            serde_json::from_str(origin_line("ready", Some(origin), None, Some(tunnel)).trim())
                .unwrap()
        };
        let running = ready("tailscale", "https://studio.example.ts.net");
        let main = main_route(&custom, Some(&running));
        assert_eq!(
            (main.kind, main.state, main.origins),
            (
                RouteKind::Tailscale,
                RouteState::Ready,
                vec!["https://studio.example.ts.net".to_owned()]
            )
        );
        let running = ready("none", "https://wonder.example");
        assert_eq!(
            main_route(&tailscale, Some(&running)).kind,
            RouteKind::Custom
        );
        // A host started before `tunnel` was published falls back to the config.
        let old: Value = json!({"state": "ready", "origin": "https://studio.example.ts.net"});
        assert_eq!(
            main_route(&tailscale, Some(&old)).kind,
            RouteKind::Tailscale
        );
    }

    #[test]
    fn home_network_choice_is_validated_and_survives_init() {
        let base = json!({"tunnel": "tailscale", "listenAddr": "127.0.0.1:3777"});
        for (home, ok) in [
            (Value::Null, true),
            (json!({"enabled": true, "port": 3778}), true),
            (json!({"enabled": false, "port": 1}), true),
            (json!({"enabled": true, "port": 0}), false),
            (json!({"enabled": true, "port": 65536}), false),
            (json!({"enabled": "yes", "port": 3778}), false),
            (json!({"enabled": true}), false),
        ] {
            let mut config = base.clone();
            config["homeNetwork"] = home.clone();
            assert_eq!(validate_config(&config).is_ok(), ok, "{home}");
        }
        let directory = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        let run = |command: &mut Command, args: &[&str]| {
            let output = command
                .args(["--exact", "tests::headless_cli_fixture", "--nocapture"])
                .env("WONDER_CLI_FIXTURE", args.join(" "))
                .env("WONDER_DATA_DIR", directory.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
        };
        run(
            &mut command,
            &[
                "init",
                "--tunnel",
                "none",
                "--origin",
                "https://wonder.example",
            ],
        );
        let paths = Paths::at(Some(directory.path().into())).unwrap();
        // A new setup serves the same Wi-Fi without `cupola lan on`.
        let config = load_config(&paths).unwrap();
        assert_eq!(
            home_port(&config),
            (!cfg!(windows)).then_some(cli::DEFAULT_LAN_PORT)
        );
        // `lan off` saves the choice even though none was saved before, and
        // setting up again keeps it.
        run(
            &mut Command::new(std::env::current_exe().unwrap()),
            &["lan", "off"],
        );
        run(
            &mut Command::new(std::env::current_exe().unwrap()),
            &["init"],
        );
        let config = load_config(&paths).unwrap();
        assert_eq!(home_port(&config), None);
        assert_eq!(config["homeNetwork"]["enabled"], false);
        run(
            &mut Command::new(std::env::current_exe().unwrap()),
            &["lan", "on", "--port", "4443"],
        );
        run(
            &mut Command::new(std::env::current_exe().unwrap()),
            &["init", "--name", "Studio"],
        );
        let config = load_config(&paths).unwrap();
        assert_eq!(home_port(&config), Some(4443));
        // Setting up again kept the route it was not told to change, and
        // saved the name where the host reads the owner's choice.
        assert_eq!(
            (&config["tunnel"], &config["origin"]),
            (&json!("none"), &json!("https://wonder.example"))
        );
        assert!(config["hostName"].is_null());
        assert_eq!(
            crate::cli_message_tests::stored_host_name(&paths).as_deref(),
            Some("Studio")
        );
        // `lan on` made the key the phone will pin, private to this user.
        let key = directory.path().join("host-key");
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// `init --computer` allows control, `computer control off|on` changes
    /// it, and the launcher names the directory the helper reads it from.
    #[cfg(target_os = "macos")]
    #[test]
    fn computer_control_choice_reaches_the_helper() {
        let root = tempfile::tempdir().unwrap();
        let directory = wonderd::canonical_path::canonicalize(root.path())
            .unwrap()
            .join("data");
        let run = |args: &[&str]| -> String {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::headless_cli_fixture", "--nocapture"])
                .env("WONDER_CLI_FIXTURE", args.join(" "))
                .env("WONDER_DATA_DIR", &directory)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        let status = |args: &[&str]| -> Value {
            // The test harness prints around the command's own output.
            let stdout = run(args);
            let start = stdout.find("\n{").unwrap() + 1;
            let end = start + stdout[start..].find("\n}").unwrap() + 2;
            serde_json::from_str(&stdout[start..end]).unwrap()
        };
        let paths = Paths::at(Some(directory.clone().into())).unwrap();
        let config = |paths: &Paths| load_config(paths).unwrap();
        run(&[
            "init",
            "--tunnel",
            "none",
            "--origin",
            "https://wonder.example",
        ]);
        assert_eq!(
            status(&["--json", "computer", "control", "status"]),
            json!({"viewing": false, "control": "off"})
        );
        assert_eq!(
            computer_control_check(&config(&paths), control_state(&paths)).summary,
            "Computer view is off: your iPhone cannot view or control this computer"
        );
        run(&[
            "init",
            "--tunnel",
            "none",
            "--origin",
            "https://wonder.example",
            "--computer",
        ]);
        assert_eq!(
            status(&["--json", "computer", "control", "status"]),
            json!({"viewing": true, "control": "on"})
        );
        assert!(run(&["computer", "control", "status"])
            .contains("Your iPhone can view and take control of this computer\n"));
        assert_eq!(
            status(&["--json", "computer", "control", "off"]),
            json!({"viewing": true, "control": "off"})
        );
        let off = computer_control_check(&config(&paths), control_state(&paths));
        assert_eq!(
            (off.status, off.summary.as_str(), off.fix.as_deref()),
            (
                CheckStatus::Ok,
                "Your iPhone can view this computer; control is off",
                Some("To allow Take control, run `cupola computer control on`.")
            )
        );
        assert_eq!(
            status(&["--json", "computer", "control", "on"]),
            json!({"viewing": true, "control": "on"})
        );
        let file = directory
            .join("Service")
            .join(control_preference::FILE_NAME);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let untrusted = computer_control_check(&config(&paths), control_state(&paths));
        assert_eq!(untrusted.status, CheckStatus::Warning);

        // The daemon (and the helper it starts) gets the same directory.
        std::env::set_var("WONDER_DAEMON_BIN", "/usr/bin/true");
        let mut viewing_off = config(&paths);
        viewing_off["computer"] = json!(false);
        let daemon = daemon_command(&paths, &viewing_off).unwrap();
        let service = daemon
            .get_envs()
            .find(|(name, _)| *name == "WONDER_SERVICE_DIR")
            .and_then(|(_, value)| value);
        assert_eq!(service, Some(directory.join("Service").as_os_str()));
    }

    #[test]
    fn firewall_fix_opens_the_port_only_to_the_home_ranges() {
        let ips = |list: &[&str]| -> Vec<std::net::IpAddr> {
            list.iter().map(|ip| ip.parse().unwrap()).collect()
        };
        assert_eq!(
            firewall_fix(
                Firewall::Ufw,
                3778,
                &ips(&["192.168.1.20", "10.0.0.5", "fd12::1", "192.168.7.1"])
            ),
            "If your iPhone cannot connect on the home network, allow the port: \
             sudo ufw allow from 10.0.0.0/8 to any port 3778 proto tcp && \
             sudo ufw allow from 192.168.0.0/16 to any port 3778 proto tcp && \
             sudo ufw allow from fc00::/7 to any port 3778 proto tcp"
        );
        assert_eq!(
            firewall_fix(Firewall::Firewalld, 4443, &ips(&["172.16.4.2"])),
            "If your iPhone cannot connect on the home network, allow the port: \
             sudo firewall-cmd --permanent --add-rich-rule='rule family=ipv4 source \
             address=172.16.0.0/12 port port=4443 protocol=tcp accept' && sudo firewall-cmd --reload"
        );
        // No home address yet: the usual home range, never "anywhere".
        assert!(firewall_fix(Firewall::Ufw, 3778, &[]).contains("from 192.168.0.0/16 "));
        let rows = [
            (None, Some(3778), CheckStatus::Ok, false),
            (Some(Firewall::Ufw), None, CheckStatus::Ok, false),
            (Some(Firewall::Ufw), Some(3778), CheckStatus::Warning, true),
            (
                Some(Firewall::Firewalld),
                Some(3778),
                CheckStatus::Warning,
                true,
            ),
        ];
        for (firewall, port, status, fix) in rows {
            let check = firewall_status(firewall, port, &[]);
            assert_eq!(check.id, "firewall");
            assert_eq!(
                (check.status, check.fix.is_some()),
                (status, fix),
                "{firewall:?} {port:?}"
            );
        }
    }

    /// `socketfilterfw` as this Mac printed it on 2026-10-20, and the doctor
    /// row each firewall state gives.
    #[test]
    fn mac_firewall_reads_socketfilterfw_and_names_the_fix() {
        let on = "Firewall is enabled. (State = 1)\n";
        let off = "Firewall is disabled. (State = 0)\n";
        let block_off = "Firewall has block all state set to disabled.\n";
        let block_on = "Firewall has block all state set to enabled.\n";
        let signed = "Automatically allow built-in signed software ENABLED.\n\
                      Automatically allow downloaded signed software ENABLED.\n";
        let unsigned = "Automatically allow built-in signed software ENABLED.\n\
                        Automatically allow downloaded signed software DISABLED.\n";
        let apps = mac_firewall_apps(
            "Total number of apps = 3 \n\
             1 : /Applications/Cupola CLI.app/Contents/MacOS/wonderd \n\
             \t     (Allow incoming connections)\n\
             2 : /Applications/GLKVM.app/Contents/Frameworks/GLKVM Helper.app \n\
             \t     (Allow incoming connections)\n\
             3 : /opt/dev/wonderd \n\
             \t     (Block incoming connections)\n",
        );
        assert_eq!(apps.len(), 3);
        let installed = "/Applications/Cupola CLI.app/Contents/MacOS/wonderd";
        assert_eq!(mac_firewall_rule(&apps, installed), Some(true));
        assert_eq!(mac_firewall_rule(&apps, "/opt/dev/wonderd"), Some(false));
        assert_eq!(
            mac_firewall_rule(
                &apps,
                "/Applications/GLKVM.app/Contents/Frameworks/GLKVM Helper.app/Contents/MacOS/x"
            ),
            Some(true),
            "a bundle's rule covers its executable"
        );
        assert_eq!(mac_firewall_rule(&apps, "/opt/dev/wonderd2"), None);

        let state = |global, block, allow, daemon: Option<(&str, bool, Option<bool>)>| {
            let mut firewall = MacFirewall::parse(global, block, allow).unwrap();
            firewall.daemon = daemon.map(|(path, signed, rule)| (path.to_owned(), signed, rule));
            Ok(firewall)
        };
        use CheckStatus::{Ok as Fine, Unavailable, Warning};
        let rows = vec![
            (
                "off",
                state(off, block_off, signed, None),
                Some(3778),
                Fine,
                "is off",
                false,
            ),
            (
                "home off",
                state(on, block_off, signed, None),
                None,
                Fine,
                "matters only",
                false,
            ),
            (
                "block all",
                state(on, block_on, signed, Some((installed, true, Some(true)))),
                Some(3778),
                Warning,
                "blocks all",
                true,
            ),
            (
                "allowed",
                state(
                    on,
                    block_off,
                    unsigned,
                    Some((installed, false, Some(true))),
                ),
                Some(3778),
                Fine,
                "allows the Cupola host",
                false,
            ),
            (
                "blocked",
                state(
                    on,
                    block_off,
                    signed,
                    Some(("/opt/dev/wonderd", true, Some(false))),
                ),
                Some(3778),
                Warning,
                "blocks the Cupola host",
                true,
            ),
            (
                "signed, allow-signed on",
                state(on, block_off, signed, Some((installed, true, None))),
                Some(3778),
                Fine,
                "signed apps",
                false,
            ),
            (
                "unsigned, unlisted",
                state(
                    on,
                    block_off,
                    signed,
                    Some(("/opt/dev/wonderd", false, None)),
                ),
                Some(3778),
                Warning,
                "no rule",
                true,
            ),
            (
                "signed, allow-signed off",
                state(on, block_off, unsigned, Some((installed, true, None))),
                Some(3778),
                Warning,
                "no rule",
                true,
            ),
            (
                "query failed",
                Err("socketfilterfw --getglobalstate failed (exit status: 1)".into()),
                Some(3778),
                Unavailable,
                "could not read",
                false,
            ),
            (
                "empty output",
                MacFirewall::parse("", "", ""),
                None,
                Unavailable,
                "did not report",
                false,
            ),
        ];
        for (name, firewall, port, status, phrase, fix) in rows {
            let check = mac_firewall_status(&firewall, port);
            assert_eq!(check.id, "firewall");
            assert_eq!(check.status, status, "{name}");
            assert!(check.summary.contains(phrase), "{name}: {}", check.summary);
            assert_eq!(check.fix.is_some(), fix, "{name}");
        }
        assert!(!MacFirewall::parse(off, block_off, signed).unwrap().enabled);
        assert!(
            MacFirewall::parse(on, block_off, signed)
                .unwrap()
                .allow_signed
        );
        assert!(
            !MacFirewall::parse(on, block_off, unsigned)
                .unwrap()
                .allow_signed
        );
        assert!(MacFirewall::parse("garbled\n", block_off, signed).is_err());
    }

    #[test]
    fn pairing_routes_say_when_they_cannot_carry_a_pairing() {
        let home_off =
            json!({"tunnel": "tailscale", "homeNetwork": {"enabled": false, "port": 3778}});
        let home_on =
            json!({"tunnel": "tailscale", "homeNetwork": {"enabled": true, "port": 3778}});
        // Each names the recovery step: turn the route on, or use another way in.
        for (route, config, next) in [
            (PairRoute::Home, &home_off, "`cupola lan on`"),
            (
                PairRoute::Relay,
                &home_on,
                "Pair on the home network or Tailscale first",
            ),
        ] {
            let error = pair_route_allowed(route, config).unwrap_err();
            let (output, exit) = ErrorOutput::from_error(error.as_ref());
            assert_eq!(
                (output.error.code, exit),
                (cli::ErrorCode::Unavailable, 69),
                "{route:?}"
            );
            assert!(
                output.error.message.contains(next),
                "{route:?}: {}",
                output.error.message
            );
        }
        assert!(pair_route_allowed(PairRoute::Home, &home_on).is_ok());
        // Same Wi-Fi pairing needs no `cupola lan on` on a new setup.
        assert_eq!(
            pair_route_allowed(PairRoute::Home, &json!({"tunnel": "tailscale"})).is_ok(),
            !cfg!(windows)
        );
        assert!(pair_route_allowed(PairRoute::Tailscale, &home_on).is_ok());
        // Only a pair without --route falls back to same Wi-Fi, and only while it is on.
        let fresh = json!({"tunnel": "tailscale"});
        assert_eq!(
            pair_via(None, &fresh).unwrap(),
            PairVia::Any {
                home: !cfg!(windows)
            }
        );
        assert_eq!(
            pair_via(None, &home_off).unwrap(),
            PairVia::Any { home: false }
        );
        assert_eq!(
            pair_via(Some(PairRoute::Tailscale), &home_on).unwrap(),
            PairVia::Main
        );
        assert_eq!(
            pair_via(Some(PairRoute::Home), &home_on).unwrap(),
            PairVia::Home
        );
    }

    /// `cupola relay on|off|status` with the host stopped (the data
    /// directory) and running (`relay.set` on the host). Allowing it is its
    /// own state until a phone finishes turning it on, never "off".
    #[tokio::test]
    async fn relay_on_off_and_status_with_the_host_stopped_and_running() {
        use wonderd::relay::{
            read_settings, write_settings, Environment, RelaySettings, STATUS_FILE,
        };
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::at(Some(directory.path().into())).unwrap();
        let allowed = "Relay allowed. Turn on Reach from anywhere on your iPhone to finish.";
        let off = format!("Relay off. {RELAY_OFF_HINT}");
        let run = |host: Option<Local>, command| {
            let paths = &paths;
            async move {
                let output = relay_at(paths, host.as_ref(), Environment::disabled(), command)
                    .await
                    .unwrap();
                relay_text(&output)
            }
        };

        // Stopped: the setting is written here; the host acts on it at start.
        assert_eq!(run(None, RelayCommand::Status).await, off);
        assert_eq!(run(None, RelayCommand::On).await, allowed);
        assert!(read_settings(&paths.data).allowed);
        assert_eq!(run(None, RelayCommand::Status).await, allowed);
        assert_eq!(run(None, RelayCommand::Off).await, off);
        assert!(!read_settings(&paths.data).allowed);

        // Running: on and off go to the host, which answers with its view.
        let (host, seen) = stub_daemon(|_, _| {
            (
                200,
                r#"{"allowed":true,"enabled":false,"state":"off","origin":null,
                    "detail":"Turn on Reach from anywhere on your iPhone to finish."}"#,
            )
        })
        .await;
        assert_eq!(run(Some(host), RelayCommand::On).await, allowed);
        let (host, seen_off) = stub_daemon(|_, _| {
            (
                200,
                r#"{"allowed":false,"enabled":false,"state":"off","origin":null,"detail":null}"#,
            )
        })
        .await;
        assert_eq!(run(Some(host), RelayCommand::Off).await, off);
        let requests: Vec<_> = [seen, seen_off]
            .iter()
            .flat_map(|seen| seen.lock().unwrap().clone())
            .map(|seen| (seen.method, seen.path, seen.body))
            .collect();
        let settings = |allowed: bool| {
            (
                "PUT".to_owned(),
                "/api/v1/relay/settings".to_owned(),
                json!({ "allowed": allowed }).to_string(),
            )
        };
        assert_eq!(requests, [settings(true), settings(false)]);

        // Status reads the tunnel's state; a stopped host's tunnel is not running.
        write_settings(&paths.data, RelaySettings { allowed: true }).unwrap();
        std::fs::write(
            paths.data.join(STATUS_FILE),
            r#"{"enabled":true,"state":"ready","label":"k7xq2abcdefghijklmnopqrstu",
                "origin":"https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app","detail":null}"#,
        )
        .unwrap();
        let (host, seen) = stub_daemon(|_, _| (500, "{}")).await;
        assert_eq!(
            run(Some(host), RelayCommand::Status).await,
            "Relay ready at https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app."
        );
        assert!(seen.lock().unwrap().is_empty(), "status only reads");
        assert_eq!(
            run(None, RelayCommand::Status).await,
            "Relay host not running at https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app."
        );
    }

    /// Runs one `wonder` command line in this test binary, so tests can drive
    /// the real command handlers in a child process with its own environment.
    #[test]
    fn headless_cli_fixture() {
        let Some(line) = std::env::var_os("WONDER_CLI_FIXTURE") else {
            return;
        };
        let line = line.into_string().unwrap();
        let cli = Cli::try_parse_from(std::iter::once("cupola").chain(line.split(' '))).unwrap();
        let json = cli.json;
        match cli.command.unwrap() {
            HostCommand::Init(args) => init(&args, json),
            HostCommand::Lan { command } => lan(command, json),
            HostCommand::Computer { command } => computer(command, json),
            HostCommand::Routes => routes(json),
            HostCommand::Relay { command } => relay_command(command, json),
            other => panic!("not a fixture command: {other:?}"),
        }
        .unwrap();
    }

    #[test]
    fn lost_serve_mapping_is_recovered_while_host_stays_running() {
        let fixture = ServeFixture::start();
        // Other ports/handlers must survive the targeted recovery command.
        let mut expected: Value = serde_json::from_str(&fixture.read("owned.json")).unwrap();
        expected["TCP"]["9443"] = json!({"HTTPS": true});
        expected["Web"]["other.example.ts.net:9443"] =
            json!({"Handlers": {"/other": {"Proxy": "http://127.0.0.1:9999"}}});
        expected["AllowFunnel"] = json!({"other.example.ts.net:9443": true});
        fixture.set("owned.json", &expected.to_string());
        let mut missing = expected.clone();
        missing["TCP"].as_object_mut().unwrap().remove("8444");
        missing["Web"]
            .as_object_mut()
            .unwrap()
            .remove("owned.example.ts.net:8444");
        let start = Instant::now();
        fixture.set("verify-gate", "");
        fixture.set("serve.json", &missing.to_string());
        fixture.wait("lost Serve mapping recovery", || {
            !fixture.read("mutations").is_empty()
        });
        fixture.wait("origin withdrawn during recovery", || {
            fixture.state()["state"] == "starting"
        });
        std::fs::remove_file(fixture.root().join("verify-gate")).unwrap();
        fixture.wait("verified ready", || fixture.state()["state"] == "ready");
        assert_eq!(
            fixture.state()["origin"],
            "https://owned.example.ts.net:8444"
        );
        assert_eq!(fixture.read("mutations").lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&fixture.read("serve.json")).unwrap(),
            expected
        );
        assert!(!fixture.host.exited().unwrap());
        println!("lost mapping recovery: {:?}", start.elapsed());
        println!(
            "recovery origin log:\n{}recovery command log:\n{}",
            fixture.read("headless/origin-status.jsonl"),
            fixture.read("calls")
        );
        fixture.stop();
    }

    #[test]
    fn host_starts_and_keeps_retrying_while_tailscale_is_turned_off() {
        let (listener, port) = free_listener();
        drop(listener);
        let home = json!({"enabled": true, "port": port});
        let fixture = ServeFixture::launch_with_lan(home, &["stopped"]);
        fixture.wait("daemon started without Tailscale", || {
            !fixture.read("descendant.pid").is_empty()
        });
        fixture.wait("route unavailable", || {
            fixture.state()["state"] == "unavailable"
        });
        assert!(fixture.state()["origin"].is_null());
        // The home network route is served meanwhile.
        assert_eq!(fixture.read("lan-port").trim(), port.to_string());
        assert_eq!(fixture.state()["homePort"], port);
        fixture.wait("home network listener", || {
            fixture.root().join("lan-serving").exists()
        });
        let paths = Paths {
            data: fixture.root().into(),
            state: fixture.root().join("headless"),
        };
        assert_eq!(
            home_listener_probe(&paths)(std::net::Ipv4Addr::LOCALHOST.into(), port),
            Ok(())
        );
        fixture.wait("repeated checks", || {
            fixture
                .read("calls")
                .lines()
                .filter(|line| *line == "status --json")
                .count()
                >= 3
        });
        assert!(!fixture.host.exited().unwrap());
        assert!(fixture.read("mutations").is_empty());
        std::fs::remove_file(fixture.root().join("stopped")).unwrap();
        fixture.wait("Tailscale turned back on", || {
            fixture.state()["state"] == "ready"
        });
        assert_eq!(
            fixture.state()["origin"],
            "https://owned.example.ts.net:8444"
        );
        fixture.stop();
        let log = fixture.read("host.log");
        // The test harness prints its own lines into the same file.
        let host_lines: Vec<&str> = log
            .lines()
            .filter(|line| {
                ["Cupola host", "Tailscale", "daemon fixture"]
                    .iter()
                    .any(|text| line.contains(text))
            })
            .collect();
        for line in &host_lines {
            let (date, rest) = line.split_once(' ').unwrap();
            let (time, _) = rest.split_once(' ').unwrap();
            assert!(
                chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok()
                    && chrono::NaiveTime::parse_from_str(time, "%H:%M:%S%.3f").is_ok(),
                "unstamped host log line: {line}"
            );
        }
        let has = |text: &str| host_lines.iter().filter(|line| line.contains(text)).count();
        assert_eq!(has(" starting (pid "), 1, "{log}");
        assert_eq!(has("daemon fixture output"), 1, "{log}");
        assert_eq!(has("daemon fixture warning"), 1, "{log}");
        // Logged once, not on every retry.
        let reason = tailscale_status_identity(true, b"").unwrap_err();
        assert_eq!(
            has(&format!("Tailscale route unavailable: {reason}")),
            1,
            "{log}"
        );
        assert_eq!(
            has("Tailscale route ready at https://owned.example.ts.net:8444"),
            1
        );
        assert_eq!(has("Cupola host stopped"), 1, "{log}");
    }

    #[test]
    fn every_line_of_a_multi_line_host_message_is_stamped() {
        assert_eq!(
            stamped_lines(
                "T",
                "Tailscale route unavailable: tailscale failed:\nline two\r\n\n"
            ),
            "T Tailscale route unavailable: tailscale failed:\nT line two\n"
        );
        assert_eq!(stamped_lines("T", "one"), "T one\n");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn tailscale_status_explains_each_state_the_person_can_fix() {
        let cases: [(&[u8], &str); 7] = [
            (b"", "Tailscale is turned off: open Tailscale and Connect"),
            (b"\n", "Tailscale is turned off: open Tailscale and Connect"),
            (
                br#"{"BackendState":"Stopped"}"#,
                "Tailscale is turned off: open Tailscale and Connect",
            ),
            (
                br#"{"BackendState":"NeedsLogin"}"#,
                "This computer is not signed in to Tailscale: open Tailscale and sign in",
            ),
            (
                br#"{"BackendState":"NeedsMachineAuth"}"#,
                "Tailscale is waiting for the tailnet admin to approve this computer",
            ),
            (
                br#"{"BackendState":"Starting"}"#,
                "Tailscale is still connecting; the host keeps retrying",
            ),
            (b"{", "Tailscale's status could not be read"),
        ];
        for (stdout, expected) in cases {
            let error = tailscale_status_identity(true, stdout).unwrap_err();
            assert!(
                error.to_string().starts_with(expected),
                "{}: {error}",
                String::from_utf8_lossy(stdout)
            );
        }
        // Doctor shows the reason itself, not a parse error or a generic line.
        let check = serde_json::to_value(tailscale_doctor_check(
            tailscale_status_identity(true, b""),
            || panic!("no identity"),
        ))
        .unwrap();
        assert_eq!(
            check["summary"],
            "Tailscale is turned off: open Tailscale and Connect"
        );
        assert_eq!(
            check["fix"],
            "Needs the person: open Tailscale and click Connect."
        );
        let running = br#"{"BackendState":"Running",
            "Self":{"ID":"node","DNSName":"host.example.ts.net."}}"#;
        assert_eq!(
            tailscale_status_identity(true, running).unwrap(),
            ServeIdentity {
                name: "host.example.ts.net".into(),
                node: "node".into()
            }
        );
    }

    #[test]
    fn healthy_serve_is_probed_without_mutation_or_origin_log_churn() {
        let fixture = ServeFixture::start();
        let origin_log = fixture.read("headless/origin-status.jsonl");
        fixture.wait("repeated healthy checks", || {
            fixture
                .read("calls")
                .lines()
                .filter(|line| *line == "status --json")
                .count()
                >= 4
        });
        assert!(fixture.read("mutations").is_empty());
        assert_eq!(fixture.read("headless/origin-status.jsonl"), origin_log);
        fixture.stop();
    }

    #[test]
    fn unsafe_or_unknown_serve_state_is_withdrawn_and_retried_without_overwrite() {
        let fixture = ServeFixture::start();
        let identity = fixture.read("identity.json");
        let owned = fixture.read("owned.json");
        let mut replacement: Value = serde_json::from_str(&owned).unwrap();
        replacement["Web"]["owned.example.ts.net:8444"]["Handlers"]["/"] =
            json!({"Proxy": "http://127.0.0.1:9999"});
        let mut extra_handler: Value = serde_json::from_str(&owned).unwrap();
        extra_handler["Web"]["owned.example.ts.net:8444"]["Handlers"]["/other"] =
            json!({"Path": "/tmp"});
        let funnel = json!({"AllowFunnel": {"owned.example.ts.net:8444": true}});
        let other_name = json!({"Web": {"replacement.example.ts.net:8444": {
            "Handlers": {"/": {"Proxy": "http://127.0.0.1:9999"}}}}});
        let foreground =
            json!({"Foreground": {"session": serde_json::from_str::<Value>(&owned).unwrap()}});
        let mut changed_node: Value = serde_json::from_str(&identity).unwrap();
        changed_node["Self"]["ID"] = json!("another-node");
        let mut changed_name: Value = serde_json::from_str(&identity).unwrap();
        changed_name["Self"]["DNSName"] = json!("changed.example.ts.net.");
        let cases = [
            ("replacement service", "serve.json", replacement.to_string()),
            ("extra handler", "serve.json", extra_handler.to_string()),
            ("Funnel", "serve.json", funnel.to_string()),
            ("another hostname", "serve.json", other_name.to_string()),
            ("foreground owner", "serve.json", foreground.to_string()),
            ("JSON scalar", "serve.json", "null".into()),
            ("malformed map", "serve.json", r#"{"TCP":false}"#.into()),
            (
                "null port entry",
                "serve.json",
                r#"{"TCP":{"8444":null}}"#.into(),
            ),
            (
                "malformed web entry",
                "serve.json",
                r#"{"Web":{"owned.example.ts.net:8444":null}}"#.into(),
            ),
            (
                "unknown configuration",
                "serve.json",
                r#"{"Services":{"unknown":{}}}"#.into(),
            ),
            ("invalid JSON", "serve.json", "{".into()),
            ("changed node", "identity.json", changed_node.to_string()),
            ("changed name", "identity.json", changed_name.to_string()),
            (
                "signed out",
                "identity.json",
                r#"{"BackendState":"NeedsLogin"}"#.into(),
            ),
            ("malformed identity", "identity.json", "[]".into()),
            ("status unavailable", "unavailable", "".into()),
        ];
        for (label, file, value) in cases {
            // A changed identity must not claim even a completely free port.
            if file == "identity.json" {
                fixture.set("serve.json", "{}");
            }
            let before = fixture.read("calls").lines().count();
            fixture.set(file, &value);
            fixture.wait(label, || fixture.state()["state"] == "unavailable");
            assert!(fixture.state()["origin"].is_null(), "{label}");
            std::thread::sleep(Duration::from_millis(250));
            assert!(fixture.read("mutations").is_empty(), "{label}");
            assert!(
                fixture.read("calls").lines().count() - before <= 8,
                "busy retry: {label}"
            );
            assert!(!fixture.host.exited().unwrap(), "{label}");
            fixture.set("identity.json", &identity);
            fixture.set("serve.json", &owned);
            let _ = std::fs::remove_file(fixture.root().join("unavailable"));
            fixture.wait("safe transient recovery", || {
                fixture.state()["state"] == "ready"
            });
            println!("no-overwrite and retry verified: {label}");
        }
        fixture.stop();
    }

    #[test]
    fn recovery_withholds_ready_until_mapping_and_identity_are_verified() {
        let fixture = ServeFixture::start();
        fixture.set("verify-gate", "");
        fixture.set("serve.json", "{}");
        fixture.wait("post-mutation verification", || {
            fixture.root().join("verifying").exists()
        });
        fixture.wait("starting without origin", || {
            fixture.state()["state"] == "starting"
        });
        assert!(fixture.state()["origin"].is_null());
        // Change identity while the read-back command is in flight. Neither
        // its old origin nor a new identity's origin may become ready.
        let original = fixture.read("identity.json");
        let mut changed: Value = serde_json::from_str(&original).unwrap();
        changed["Self"]["ID"] = json!("replacement-node");
        fixture.set("identity.json", &changed.to_string());
        std::fs::remove_file(fixture.root().join("verify-gate")).unwrap();
        fixture.wait("changed identity after mutation", || {
            fixture.state()["state"] == "unavailable"
        });
        assert!(fixture.state()["origin"].is_null());
        fixture.set("identity.json", &original);
        fixture.wait("original identity verified", || {
            fixture.state()["state"] == "ready"
        });
        assert_eq!(fixture.read("mutations").lines().count(), 1);
        fixture.stop();
    }

    #[test]
    fn successful_serve_command_without_a_mapping_is_not_ready() {
        let fixture = ServeFixture::start();
        fixture.set("no-apply", "");
        fixture.set("serve.json", "{}");
        fixture.wait("failed read-back", || {
            fixture.state()["state"] == "unavailable"
        });
        assert!(fixture.state()["origin"].is_null());
        assert!(!fixture.read("mutations").is_empty());
        std::fs::remove_file(fixture.root().join("no-apply")).unwrap();
        fixture.wait("later verified recovery", || {
            fixture.state()["state"] == "ready"
        });
        fixture.stop();
    }

    #[test]
    fn identity_changed_during_healthy_probe_is_not_published_as_ready() {
        let fixture = ServeFixture::start();
        fixture.set("inspect-gate", "");
        fixture.wait("healthy Serve read in flight", || {
            fixture.root().join("verifying").exists()
        });
        let original = fixture.read("identity.json");
        let mut changed: Value = serde_json::from_str(&original).unwrap();
        changed["Self"]["ID"] = json!("replacement-node");
        fixture.set("identity.json", &changed.to_string());
        std::fs::remove_file(fixture.root().join("inspect-gate")).unwrap();
        fixture.wait("identity changed during healthy probe", || {
            fixture.state()["state"] == "unavailable"
        });
        assert!(fixture.state()["origin"].is_null());
        assert!(fixture.read("mutations").is_empty());
        fixture.set("identity.json", &original);
        fixture.wait("original identity returns", || {
            fixture.state()["state"] == "ready"
        });
        fixture.stop();
    }

    #[test]
    fn stop_during_probe_cancels_and_reaps_without_waiting_for_command_deadline() {
        let fixture = ServeFixture::start();
        fixture.set("hang", "");
        fixture.wait("in-flight check", || {
            fixture.root().join("probe.pid").exists()
        });
        fixture.stop();
        for file in ["probe.pid", "daemon.pid", "descendant.pid"] {
            fixture.assert_process_gone(file);
        }
    }

    #[test]
    fn in_flight_probe_does_not_extend_the_existing_child_shutdown_grace() {
        let fixture = ServeFixture::start_prepared(true, &[("stop-grace", "3000")]);
        fixture.set("hang", "");
        fixture.wait("in-flight check", || {
            fixture.root().join("probe.pid").exists()
        });
        let start = Instant::now();
        fixture.stop_with_budget(Duration::from_secs(5));
        assert!(start.elapsed() >= Duration::from_secs(3));
        for file in ["probe.pid", "daemon.pid", "descendant.pid"] {
            fixture.assert_process_gone(file);
        }
        println!(
            "uncooperative child stop during probe: {:?}",
            start.elapsed()
        );
    }

    // A stop signals the daemon alone and waits for it to finish saving:
    // Codex, in the daemon's process group, must not be stopped under it.
    #[test]
    fn stop_lets_the_daemon_drain_before_its_process_group_is_stopped() {
        let fixture = ServeFixture::start_prepared(false, &[("drain", "")]);
        let start = Instant::now();
        fixture.stop_with_budget(Duration::from_secs(10));
        assert!(start.elapsed() >= Duration::from_secs(1));
        assert_eq!(fixture.read("drained").trim(), "drained");
        assert_eq!(fixture.read("helper-term"), "", "helper signalled directly");
        for file in ["daemon.pid", "helper.pid", "descendant.pid"] {
            fixture.assert_process_gone(file);
        }
    }

    #[test]
    fn daemon_crash_during_probe_still_stops_host_and_reaps_descendants() {
        let fixture = ServeFixture::start();
        fixture.set("hang", "");
        fixture.wait("in-flight check", || {
            fixture.root().join("probe.pid").exists()
        });
        let pid: i32 = fixture.read("daemon.pid").trim().parse().unwrap();
        let start = Instant::now();
        unsafe { libc::kill(pid, libc::SIGKILL) };
        fixture.wait("crashed daemon stops host", || {
            fixture.host.exited().unwrap()
        });
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(fixture.state()["state"], "stopped");
        for file in ["probe.pid", "daemon.pid", "descendant.pid"] {
            fixture.assert_process_gone(file);
        }
        println!("crash during probe cleanup: {:?}", start.elapsed());
    }

    fn process_gone(pid: &str) -> bool {
        let output = Command::new("ps")
            .args(["-p", pid.trim(), "-o", "stat="])
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&output.stdout);
        status.trim().is_empty() || status.trim().starts_with('Z')
    }

    #[test]
    fn crashed_daemon_is_replaced_after_its_descendants_stop() {
        let fixture = ServeFixture::start_restarting(5, 300);
        let (crashed, descendant, bridge) = fixture.crash_daemon();
        fixture.wait("route withdrawn while restarting", || {
            fixture.state()["state"] == "starting"
        });
        assert!(fixture.state()["origin"].is_null());
        fixture.wait("replacement daemon", || fixture.daemon_started());
        assert_ne!(fixture.read("daemon.pid"), crashed);
        // The crashed daemon's bridge, in a process group of its own, was
        // stopped before the replacement started its own.
        assert!(process_gone(&bridge), "bridge {bridge} outlived its daemon");
        fixture.wait("route offered again", || {
            fixture.state()["state"] == "ready"
        });
        assert_eq!(
            fixture.state()["origin"],
            "https://owned.example.ts.net:8444"
        );
        fixture.write_pid("crashed.pid", &crashed);
        fixture.write_pid("crashed-descendant.pid", &descendant);
        for file in ["crashed.pid", "crashed-descendant.pid"] {
            fixture.assert_process_gone(file);
        }
        assert!(!process_gone(&fixture.read("bridge.pid")));
        assert!(!fixture.host.exited().unwrap());
        assert!(fixture.read("host.log").contains("Restarted the host"));
        // A deliberate stop ends the host; nothing replaces the daemon.
        let replacement = fixture.read("daemon.pid");
        fixture.stop();
        for file in ["daemon.pid", "descendant.pid", "bridge.pid"] {
            fixture.assert_process_gone(file);
        }
        assert_eq!(fixture.read("daemon.pid"), replacement);
    }

    #[test]
    fn a_daemon_that_keeps_crashing_stops_the_host() {
        let fixture = ServeFixture::start_restarting(2, 50);
        for round in 0..2 {
            let (crashed, ..) = fixture.crash_daemon();
            fixture.wait(&format!("replacement {round}"), || {
                fixture.daemon_started() && fixture.read("daemon.pid") != crashed
            });
        }
        fixture.crash_daemon();
        fixture.wait("host gives up", || fixture.host.exited().unwrap());
        assert_eq!(fixture.state()["state"], "stopped");
        assert!(
            fixture
                .read("host.log")
                .contains("the host stopped unexpectedly 3 times within 10 minutes"),
            "{}",
            fixture.read("host.log")
        );
        fixture.assert_process_gone("daemon.pid");
    }

    #[test]
    fn stop_during_restart_backoff_is_prompt_and_starts_nothing() {
        let fixture = ServeFixture::start_restarting(5, 30_000);
        let (crashed, ..) = fixture.crash_daemon();
        fixture.wait("restart pending", || {
            fixture.read("host.log").contains("restarting it in 30.0 s")
        });
        fixture.stop();
        assert_eq!(fixture.read("daemon.pid"), crashed);
        assert!(fixture.read("descendant.pid").is_empty());
    }

    #[test]
    fn crash_restarts_back_off_and_give_up_only_within_the_window() {
        assert_eq!(
            CrashRestarts::for_host() == CrashRestarts::MAC,
            cfg!(target_os = "macos")
        );
        let mut history = CrashHistory::new(CrashRestarts::MAC);
        let start = Instant::now();
        let delays: Vec<_> = (0..6)
            .map(|crash| history.restart_after(start + Duration::from_secs(crash * 30)))
            .collect();
        let seconds = |delay: u64| Some(Duration::from_secs(delay));
        assert_eq!(
            delays,
            [
                seconds(1),
                seconds(2),
                seconds(4),
                seconds(8),
                seconds(16),
                None
            ]
        );
        // Crashes older than ten minutes no longer count.
        assert_eq!(
            history.restart_after(start + Duration::from_secs(601)),
            seconds(16)
        );
        assert_eq!(
            CrashHistory::new(CrashRestarts::NEVER).restart_after(start),
            None
        );
    }

    #[test]
    fn only_a_failure_exit_counts_as_a_crash() {
        let status = |script: &str| Command::new("sh").args(["-c", script]).status().unwrap();
        assert!(!crashed(status("exit 0")));
        assert!(!crashed(status("kill -TERM $$")));
        assert!(!crashed(status("kill -INT $$")));
        assert!(crashed(status("exit 1")));
        assert!(crashed(status("kill -KILL $$")));
        assert!(crashed(status("kill -SEGV $$")));
    }

    #[test]
    fn timed_out_probe_is_reaped_and_later_retry_recovers() {
        let fixture = ServeFixture::start();
        fixture.set("hang", "");
        fixture.wait("in-flight check", || {
            fixture.root().join("probe.pid").exists()
        });
        let start = Instant::now();
        let calls_while_blocked = fixture.read("calls");
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(
            fixture.read("calls"),
            calls_while_blocked,
            "overlapping probe while blocked"
        );
        fixture.wait_for("15-second command timeout", Duration::from_secs(17), || {
            fixture.state()["state"] == "unavailable"
        });
        assert!(start.elapsed() >= Duration::from_secs(14));
        println!("probe timeout observed: {:?}", start.elapsed());
        fixture.assert_process_gone("probe.pid");
        std::fs::remove_file(fixture.root().join("hang")).unwrap();
        fixture.wait("retry after timeout", || {
            fixture.state()["state"] == "ready"
        });
        fixture.stop();
    }

    #[test]
    fn npm_codex_uses_its_exact_native_platform_package() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("node_modules/@openai/codex");
        let shim = package.join("bin/codex.js");
        std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
        std::fs::write(&shim, "").unwrap();
        let native = if cfg!(target_os = "macos") {
            "codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex"
        } else {
            "codex-linux-arm64/vendor/aarch64-unknown-linux-musl/bin/codex"
        };
        let vendor = package.parent().unwrap().join(native);
        std::fs::create_dir_all(vendor.parent().unwrap()).unwrap();
        std::fs::write(&vendor, "native").unwrap();
        assert_eq!(npm_codex_binary(&shim, "aarch64"), Some(vendor));
        assert_eq!(npm_codex_binary(&shim, "x86_64"), None);
    }

    #[test]
    fn listener_and_origin_are_private_and_unambiguous() {
        let mut config = json!({"tunnel": "none", "origin": "https://host.example", "listenAddr": "127.0.0.1:3777"});
        assert!(validate_config(&config).is_ok());
        for listen in [
            "0.0.0.0:3777",
            "[::]:3777",
            "100.64.0.1:3777",
            "localhost:3777",
            "127.0.0.1:0",
        ] {
            config["listenAddr"] = json!(listen);
            assert!(validate_config(&config).is_err(), "{listen}");
        }
        config["listenAddr"] = json!("[::1]:3777");
        assert!(validate_config(&config).is_ok());
        for origin in [
            "http://host.example",
            "https://user@host.example",
            "https://host.example/path",
            "https://host.example?query",
            "https://host.example/#fragment",
        ] {
            config["origin"] = json!(origin);
            assert!(validate_config(&config).is_err(), "{origin}");
        }
        config["origin"] = json!("https://host.example:8444");
        for tunnel in ["quick", "token", "unknown"] {
            config["tunnel"] = json!(tunnel);
            assert!(validate_config(&config).is_err(), "{tunnel}");
        }
    }

    #[test]
    fn serve_does_not_replace_or_reuse_another_service() {
        let target = "http://127.0.0.1:3777";
        let mut existing = json!({
            "TCP": {"443": {"HTTPS": true}},
            "Web": {"host.example.ts.net:443": {"Handlers": {"/": {"Proxy": target}}}}
        });
        assert!(serve_already_matches(&existing, "host.example.ts.net", 443, target).unwrap());
        assert!(!serve_already_matches(&existing, "host.example.ts.net", 8444, target).unwrap());
        existing["Web"]["host.example.ts.net:443"]["Handlers"]["/other"] = json!({"Path": "/tmp"});
        assert!(serve_already_matches(&existing, "host.example.ts.net", 443, target).is_err());
        existing["Web"]["host.example.ts.net:443"]["Handlers"]
            .as_object_mut()
            .unwrap()
            .remove("/other");
        existing["AllowFunnel"] = json!({"host.example.ts.net:443": true});
        assert!(serve_already_matches(&existing, "host.example.ts.net", 443, target).is_err());
        existing["AllowFunnel"] = json!({});
        existing["Web"]["host.example.ts.net:443"]["Handlers"]["/"] =
            json!({"Proxy": "http://127.0.0.1:8888"});
        assert!(serve_already_matches(&existing, "host.example.ts.net", 443, target).is_err());
    }

    #[test]
    fn stale_pid_cannot_claim_the_supervisor_lock() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            data: directory.path().into(),
            state: directory.path().into(),
        };
        write_private(
            &paths.supervisor_pid(),
            std::process::id().to_string().as_bytes(),
        )
        .unwrap();
        assert_eq!(running_supervisor(&paths), None);
        let owner = supervisor_lock(&paths).unwrap();
        assert_eq!(running_supervisor(&paths), Some(std::process::id() as i32));
        assert!(supervisor_lock(&paths).is_err());
        drop(owner);
        assert!(supervisor_lock(&paths).is_ok());
    }

    #[test]
    fn opencode_counts_only_with_node_and_its_cli() {
        // A bare Ubuntu image has the bundled bridge but neither Node nor
        // `opencode`; doctor must not call that an installed provider.
        let runtimes = |node: bool, cli: bool| LocalRuntimes {
            codex: None,
            claude_bridge: None,
            node: node.then(|| PathBuf::from("/usr/bin/node")),
            opencode_bridge: Some(PathBuf::from("/w/acp-runtime/main.mjs")),
            opencode_cli: cli,
            cursor_cli: None,
        };
        for (node, cli, detail) in [(false, true, "Node.js"), (true, false, "OpenCode CLI")] {
            let opencode = runtimes(node, cli).opencode();
            assert!(!opencode.installed && opencode.detail.contains(detail));
        }
        assert!(runtimes(true, true).opencode().installed);
    }

    #[test]
    #[cfg(unix)]
    fn opencode_is_found_where_the_runtime_looks() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let install = |path: &Path, mode: u32| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let home = root.path().join("home");
        let empty = Some(std::ffi::OsString::new());
        // OpenCode's own installer puts it in ~/.opencode/bin, which a
        // service's PATH lacks; the runtime still finds it there (unless
        // this machine has one in /usr/local/bin or Homebrew, searched first).
        if locate_opencode(None, empty.clone(), None).is_none() {
            assert_eq!(
                locate_opencode(None, empty.clone(), Some(home.clone())),
                None
            );
            let own = home.join(".opencode/bin/opencode");
            install(&own, 0o755);
            assert_eq!(
                locate_opencode(None, empty.clone(), Some(home.clone())),
                Some(own)
            );
        }
        // WONDER_OPENCODE_BIN wins even with nothing on PATH, and only
        // counts when it can run.
        let custom = root.path().join("tools/oc");
        install(&custom, 0o644);
        let set = Some(custom.clone().into_os_string());
        assert_eq!(locate_opencode(set.clone(), empty.clone(), None), None);
        install(&custom, 0o755);
        assert_eq!(locate_opencode(set, empty, None), Some(custom));
        // PATH comes before the fixed directories.
        let bin = root.path().join("bin");
        install(&bin.join("opencode"), 0o755);
        assert_eq!(
            locate_opencode(None, Some(bin.clone().into_os_string()), Some(home)),
            Some(bin.join("opencode"))
        );
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn tailscale_serve_needs_root_or_the_operator() {
        // Measured on Ubuntu 24.04 with Tailscale 1.104: a non-root user that
        // is not the operator gets "serve config denied" on every start.
        for prefs in [
            json!({}),
            json!({"OperatorUser": ""}),
            json!({"OperatorUser": "other"}),
        ] {
            let error = serve_permission(&prefs, "fresh").unwrap_err().to_string();
            assert!(
                error.contains("`sudo tailscale set --operator=fresh`"),
                "{error}"
            );
        }
        serve_permission(&json!({"OperatorUser": "fresh"}), "fresh").unwrap();
    }

    #[test]
    fn doctor_treats_a_missing_tailscale_as_optional_while_same_wifi_is_on() {
        let signed_out = || tailscale_doctor_check(Err("signed out".into()), || Ok(()));
        let warned = optional_tailscale(signed_out(), true);
        assert_eq!(warned.status, CheckStatus::Warning);
        assert!(warned.summary.contains("same Wi-Fi"), "{}", warned.summary);
        assert!(warned.fix.unwrap().starts_with("Needs the person:"));
        // Without the home network, Tailscale is the only way in.
        assert_eq!(
            optional_tailscale(signed_out(), false).status,
            CheckStatus::Failed
        );
        // Same Wi-Fi counts only when it is on and has a private address.
        let lan: std::net::IpAddr = "192.168.1.20".parse().unwrap();
        let fresh = json!({"tunnel": "tailscale"});
        let off = json!({"tunnel": "tailscale", "homeNetwork": {"enabled": false, "port": 3778}});
        assert_eq!(same_wifi_reachable(&fresh, &[lan]), !cfg!(windows));
        assert_eq!(
            optional_tailscale(signed_out(), same_wifi_reachable(&fresh, &[])).status,
            CheckStatus::Failed,
            "no private address: nothing can reach this computer"
        );
        assert!(!same_wifi_reachable(&off, &[lan]), "turned off");
        let ready = || {
            tailscale_doctor_check(
                Ok(ServeIdentity {
                    name: "host.example.ts.net".into(),
                    node: "node".into(),
                }),
                || Ok(()),
            )
        };
        assert_eq!(optional_tailscale(ready(), true).summary, ready().summary);
    }

    #[test]
    fn doctor_json_requires_the_person_for_tailscale_sign_in_or_permission() {
        let identity = || {
            Ok(ServeIdentity {
                name: "host.example.ts.net".into(),
                node: "node".into(),
            })
        };
        // Existing JSON table owns field spelling. This exercises the real
        // doctor's permission/sign-in decisions, which must not tell an agent
        // to change an administrator's settings on its own.
        for check in [
            tailscale_doctor_check(Err("signed out".into()), || panic!("no identity")),
            tailscale_doctor_check(identity(), || {
                Err("run `sudo tailscale set --operator=fresh`".into())
            }),
        ] {
            let value = serde_json::to_value(check).unwrap();
            assert_eq!(value["id"], "tailscale");
            assert_eq!(value["status"], "failed");
            assert!(value["fix"]
                .as_str()
                .unwrap()
                .starts_with("Needs the person:"));
        }
        let ready = serde_json::to_value(tailscale_doctor_check(identity(), || Ok(()))).unwrap();
        assert_eq!(ready["status"], "ok");
        assert!(ready["fix"].is_null());
    }

    #[cfg(not(target_os = "macos"))]
    fn display_check_ids(checks: &[Check]) -> Vec<(String, CheckStatus)> {
        checks.iter().map(|c| (c.id.to_owned(), c.status)).collect()
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn computer_view_asks_only_for_what_the_chosen_display_needs() {
        let nothing = |_: &str| false;
        let everything = |_: &str| true;
        let running = |_: &str| true;
        // A desktop that is already running needs no Xvfb or openbox.
        let checks = linux_display_checks(Some(":0"), false, running, |t| {
            t == "xdotool" || t == "xclip"
        });
        assert_eq!(
            display_check_ids(&checks),
            [("computer".into(), CheckStatus::Ok)]
        );
        assert!(checks[0].summary.contains(":0"));
        // The private display does, and says how to install them.
        let checks = linux_display_checks(None, false, running, |t| t == "xdotool" || t == "xclip");
        assert_eq!(checks[0].status, CheckStatus::Failed);
        assert!(checks[0].summary.contains("Xvfb, openbox"));
        assert!(checks[0]
            .fix
            .as_deref()
            .unwrap()
            .contains("apt-get install xvfb openbox"));
        // Everything installed: one passing check, nothing demanded.
        let checks = linux_display_checks(None, false, running, everything);
        assert_eq!(
            display_check_ids(&checks),
            [("computer".into(), CheckStatus::Ok)]
        );
        // Missing xdotool/xclip only costs control and clipboard.
        let checks = linux_display_checks(Some(":0"), false, running, nothing);
        assert_eq!(checks[1].id, "computer-input");
        assert_eq!(checks[1].status, CheckStatus::Warning);
        assert!(checks[1].summary.contains("xdotool, xclip"));
        // A display nobody serves is a failure that names it.
        let checks = linux_display_checks(Some(":7"), false, |_| false, everything);
        assert_eq!(checks[0].status, CheckStatus::Failed);
        assert!(checks[0].summary.contains(":7"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn wayland_gets_a_plain_x11_message() {
        assert!(session_is_wayland(Some("wayland"), false));
        assert!(session_is_wayland(None, true));
        assert!(!session_is_wayland(Some("x11"), false));
        assert!(!session_is_wayland(Some("tty"), false));
        let shared = linux_display_checks(Some(":0"), true, |_| true, |_| true);
        assert_eq!(shared[0].status, CheckStatus::Failed);
        assert!(shared[0]
            .summary
            .starts_with("Computer view needs an X11 session"));
        // The private display still works under Wayland, but is not the desktop.
        let private = linux_display_checks(None, true, |_| true, |_| true);
        assert_eq!(private[0].status, CheckStatus::Warning);
        assert!(private[0]
            .summary
            .starts_with("Computer view needs an X11 session"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn python_screen_sharing_uses_the_venv_beside_the_installation() {
        let prefix = tempfile::tempdir().unwrap();
        let exe = prefix.path().join("versions/1.0.0/cupola");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        assert_eq!(installed_computer_python(&exe), None);
        let python = prefix.path().join("computer-venv/bin/python");
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, "").unwrap();
        assert_eq!(installed_computer_python(&exe), Some(python));
        // A binary outside an installation's versions folder never claims a venv.
        assert_eq!(
            installed_computer_python(&prefix.path().join("bin/cupola")),
            None
        );
        let fix = python_setup_fix(&exe);
        assert!(fix.contains("python3-venv") && fix.contains("computer-venv/bin/pip install -r"));
        assert!(fix.contains(&format!("{}/computer-venv", prefix.path().display())));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn x11_socket_names_follow_the_display() {
        assert_eq!(x11_socket(":0"), Path::new("/tmp/.X11-unix/X0"));
        assert_eq!(x11_socket(":12.0"), Path::new("/tmp/.X11-unix/X12"));
    }

    #[test]
    fn service_carries_the_chosen_display_into_the_unit() {
        let env = service_environment::collect(
            [
                ("WONDER_COMPUTER_DISPLAY".into(), ":0".into()),
                ("XAUTHORITY".into(), "/run/user/1000/gdm/Xauthority".into()),
                ("DISPLAY".into(), ":9".into()),
                ("XDG_SESSION_TYPE".into(), "wayland".into()),
                ("WAYLAND_DISPLAY".into(), "wayland-0".into()),
            ],
            Path::new("/fixture/state"),
        );
        assert_eq!(env["WONDER_COMPUTER_DISPLAY"], ":0");
        // The chosen mode survives; a login's display/cookie/type do not.
        for key in [
            "DISPLAY",
            "XAUTHORITY",
            "XDG_SESSION_TYPE",
            "WAYLAND_DISPLAY",
        ] {
            assert!(!env.contains_key(key));
        }
        assert!(service_environment::valid(&env));
        let manual = service_environment::collect(
            [("XAUTHORITY".into(), "/fixture/manual-cookie".into())],
            Path::new("/fixture/state"),
        );
        assert_eq!(manual["XAUTHORITY"], "/fixture/manual-cookie");
    }

    // Runs in a child process, like the supervisor fixture, so the fake
    // `tailscale` binary never leaks into parallel tests.
    #[test]
    fn unpublish_fixture() {
        let Some(case) = std::env::var_os("WONDER_UNPUBLISH_FIXTURE") else {
            return;
        };
        let config =
            json!({"tunnel": "tailscale", "httpsPort": 8444, "listenAddr": "127.0.0.1:3777"});
        let removed = tailscale_unpublish(&config);
        std::fs::write(
            Path::new(&std::env::var_os("TAILSCALE_BIN").unwrap()).with_file_name("outcome"),
            match (case.to_str().unwrap(), removed) {
                (_, Ok(true)) => "removed".to_owned(),
                (_, Ok(false)) => "untouched".to_owned(),
                (_, Err(error)) => format!("error: {error}"),
            },
        )
        .unwrap();
    }

    fn run_unpublish(serve: &Value) -> (String, String) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("serve.json"), serve.to_string()).unwrap();
        std::fs::write(
            root.join("identity.json"),
            json!({"BackendState": "Running",
                "Self": {"ID": "owned-node", "DNSName": "owned.example.ts.net."}})
            .to_string(),
        )
        .unwrap();
        let tailscale = root.join("tailscale");
        std::fs::write(
            &tailscale,
            r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
printf '%s\n' "$*" >> calls
case "$*" in
  'status --json') cat identity.json ;;
  'serve status --json') cat serve.json ;;
  'serve --https=8444 off') echo '{}' > serve.json ;;
  *) exit 99 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&tailscale, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::unpublish_fixture", "--nocapture"])
            .env("WONDER_UNPUBLISH_FIXTURE", "1")
            .env("TAILSCALE_BIN", &tailscale)
            .env_remove("TAILSCALE_SOCKET")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (
            std::fs::read_to_string(root.join("outcome")).unwrap(),
            std::fs::read_to_string(root.join("calls")).unwrap_or_default(),
        )
    }

    #[test]
    fn unpublish_removes_only_cupolas_own_route() {
        let owned = json!({"TCP": {"8444": {"HTTPS": true}},
            "Web": {"owned.example.ts.net:8444": {"Handlers": {
                "/": {"Proxy": "http://127.0.0.1:3777"}}}}});
        let (outcome, calls) = run_unpublish(&owned);
        assert_eq!(outcome, "removed");
        assert!(calls.contains("serve --https=8444 off"), "{calls}");
        // Another program's route on the port stays.
        let other = json!({"TCP": {"8444": {"HTTPS": true}},
            "Web": {"owned.example.ts.net:8444": {"Handlers": {
                "/": {"Proxy": "http://127.0.0.1:9999"}}}}});
        let (outcome, calls) = run_unpublish(&other);
        assert_eq!(outcome, "untouched");
        assert!(!calls.contains(" off"), "{calls}");
        // Nothing published: nothing to do.
        let (outcome, calls) = run_unpublish(&json!({}));
        assert_eq!(outcome, "untouched");
        assert!(!calls.contains(" off"), "{calls}");
    }

    #[test]
    fn origin_status_line_matches_the_daemon_contract() {
        let line = origin_line(
            "ready",
            Some("https://host.example"),
            Some(3778),
            Some("tailscale"),
        );
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["state"], "ready");
        assert_eq!(value["origin"], "https://host.example");
        assert_eq!(value["homePort"], 3778);
        assert_eq!(value["tunnel"], "tailscale");
    }
}

/// `wonder chat` against a scripted daemon socket that replays a recorded
/// turn: the envelopes are built from the host's own event types, so their
/// wire shape is the one `events.subscribe` delivers.
#[cfg(test)]
mod chat_tests {
    use super::*;
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use wonder_api::{DeliveryState, HostEventEnvelope, WonderEvent};

    const THREAD: &str = "11111111-1111-4111-8111-111111111111";
    const MESSAGE: &str = "msg-1";

    fn envelope(
        sequence: u64,
        message: Option<&str>,
        turn: Option<&str>,
        event: WonderEvent,
    ) -> Value {
        serde_json::to_value(HostEventEnvelope {
            event_id: format!("e{sequence}"),
            host_epoch: "epoch".into(),
            sequence,
            occurred_at: "2026-10-18T00:00:00Z".into(),
            request_id: None,
            device_id: None,
            conversation_id: Some(THREAD.into()),
            message_id: message.map(str::to_owned),
            thread_id: Some("codex-thread".into()),
            turn_id: turn.map(str::to_owned),
            item_id: None,
            approval_id: None,
            event,
        })
        .unwrap()
    }

    fn state(state: DeliveryState) -> WonderEvent {
        WonderEvent::MessageState { state }
    }

    fn activity(category: &str, state: &str, detail: Option<&str>) -> WonderEvent {
        WonderEvent::Activity {
            category: category.into(),
            state: state.into(),
            detail: detail.map(str::to_owned),
        }
    }

    /// A turn as the host journals it: dispatch, a tool, an approval, the
    /// reply in pieces, then completion. Another message's turn in the same
    /// thread and another thread's event are interleaved and must not show.
    fn recorded_turn() -> Vec<Value> {
        let text = |text: &str| WonderEvent::AssistantDelta { text: text.into() };
        vec![
            envelope(1, Some("earlier"), Some("turn-0"), text("not ours")),
            envelope(
                2,
                Some(MESSAGE),
                None,
                state(DeliveryState::DispatchingToCodex),
            ),
            envelope(
                3,
                Some(MESSAGE),
                Some("turn-1"),
                state(DeliveryState::AcceptedByCodex),
            ),
            // Naming the turn re-reads its approvals.
            step("pause"),
            envelope(
                4,
                None,
                Some("turn-1"),
                activity("command", "started", Some("ls")),
            ),
            envelope(
                5,
                None,
                Some("turn-1"),
                activity("reasoning", "working", None),
            ),
            envelope(
                6,
                None,
                Some("turn-1"),
                activity("reasoning", "working", None),
            ),
            envelope(
                7,
                None,
                Some("turn-0"),
                activity("command", "started", Some("old")),
            ),
            envelope(
                8,
                None,
                Some("turn-1"),
                WonderEvent::ApprovalOpened {
                    request_id: "req-9".into(),
                },
            ),
            step("pause"),
            envelope(
                9,
                None,
                Some("turn-1"),
                WonderEvent::ApprovalResolved {
                    request_id: "req-9".into(),
                    decision: "accept".into(),
                },
            ),
            step("pause"),
            envelope(10, Some(MESSAGE), Some("turn-1"), text("Hello")),
            envelope(11, Some(MESSAGE), Some("turn-1"), text(", world")),
            {
                let mut other = envelope(12, None, None, text("elsewhere"));
                other["conversationId"] = json!("another-thread");
                other
            },
            envelope(
                13,
                Some(MESSAGE),
                Some("turn-1"),
                WonderEvent::AssistantCompleted {
                    text: "Hello, world".into(),
                },
            ),
            envelope(
                14,
                Some(MESSAGE),
                Some("turn-1"),
                state(DeliveryState::Completed),
            ),
        ]
    }

    #[test]
    fn a_recorded_turn_classifies_into_the_reply_tools_approvals_and_its_end() {
        let mut follow = ChatFollow {
            thread: THREAD.into(),
            message: MESSAGE.into(),
            turn: None,
        };
        let steps: Vec<_> = recorded_turn()
            .iter()
            .filter_map(|e| follow.classify(e))
            .collect();
        let tool = |category: &str, state: &str, detail: Option<&str>| {
            ChatStep::Emit(ChatEvent::Tool {
                category: category.into(),
                state: state.into(),
                detail: detail.map(str::to_owned),
            })
        };
        assert_eq!(
            steps,
            vec![
                ChatStep::Emit(ChatEvent::State {
                    state: "dispatching_to_codex".into()
                }),
                ChatStep::Emit(ChatEvent::State {
                    state: "accepted_by_codex".into()
                }),
                tool("command", "started", Some("ls")),
                tool("reasoning", "working", None),
                tool("reasoning", "working", None),
                ChatStep::Approvals(None),
                ChatStep::Approvals(Some("accept".into())),
                ChatStep::Emit(ChatEvent::Text {
                    text: "Hello".into()
                }),
                ChatStep::Emit(ChatEvent::Text {
                    text: ", world".into()
                }),
                ChatStep::Emit(ChatEvent::Message {
                    text: "Hello, world".into()
                }),
                ChatStep::End(ChatStatus::Completed),
            ]
        );
        assert_eq!(follow.turn.as_deref(), Some("turn-1"));
        // Each way a send can end has its own status and exit code.
        for (state, status, code) in [
            (DeliveryState::Completed, ChatStatus::Completed, 0),
            (DeliveryState::Failed, ChatStatus::Failed, 1),
            (DeliveryState::Uncertain, ChatStatus::Failed, 1),
            (DeliveryState::Interrupted, ChatStatus::Interrupted, 3),
        ] {
            let step = follow.classify(&envelope(1, Some(MESSAGE), None, self::state(state)));
            assert_eq!(step, Some(ChatStep::End(status)));
            assert_eq!(status.exit_code(), code);
        }
        assert_eq!(ChatStatus::TimedOut.exit_code(), 75);
    }

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    /// Script steps besides events: `pause` waits for the client's next
    /// `approvals.list`; `close` drops the connection (the rest replays to
    /// a subscription `after` the last event sent); `ended` ends the stream
    /// with `resync` and loses the rest, which the thread then shows ended;
    /// `hang` stops the host answering any call.
    fn step(name: &str) -> Value {
        json!({ name: true })
    }

    /// What the scripted host has done, across its connections.
    #[derive(Default)]
    struct Host {
        pending: std::collections::VecDeque<Value>,
        open: bool,
        subscriptions: u32,
        last: u64,
        ended: bool,
        /// A subscription from now after the stream ended: the thread then
        /// reads the message completed, as if it ended in between.
        resubscribed: bool,
        hung: bool,
    }

    /// The frames to send now: events from the script until a step.
    /// True when the connection is then dropped.
    fn flush(host: &mut Host, frames: &mut Vec<String>) -> bool {
        while let Some(event) = host.pending.pop_front() {
            let subscription = format!("s{}", host.subscriptions);
            if event["pause"] == true {
                return false;
            }
            if event["close"] == true {
                return true;
            }
            if event["hang"] == true {
                host.hung = true;
                return false;
            }
            if event["ended"] == true {
                host.pending.clear();
                host.ended = true;
                frames.push(
                    json!({"jsonrpc": "2.0", "method": "events.ended",
                    "params": {"subscriptionId": subscription, "reason": "resync",
                               "detail": "epochChanged"}})
                    .to_string(),
                );
                return false;
            }
            let kind = event["event"]["type"].clone();
            if kind == "approval_opened" || kind == "approval_resolved" {
                host.open = kind == "approval_opened";
            }
            host.last = event["sequence"].as_u64().unwrap_or(host.last);
            frames.push(
                json!({"jsonrpc": "2.0", "method": "event",
                "params": {"subscriptionId": subscription, "event": event}})
                .to_string(),
            );
        }
        false
    }

    /// A daemon socket that answers `wonder chat`'s calls from a script. A
    /// send queues `events`; they go out after each `approvals.list`
    /// answer up to the script's next step, so a run is deterministic.
    fn scripted_daemon(events: Vec<Value>) -> (Local, Calls) {
        let calls: Calls = Arc::default();
        let host: Arc<Mutex<Host>> = Arc::default();
        let log = calls.clone();
        let app = axum::Router::new().route(
            "/api/v1/rpc",
            axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
                let (log, events, host) = (log.clone(), events.clone(), host.clone());
                async move {
                    ws.on_upgrade(move |socket| async move {
                        let (mut sink, mut stream) = socket.split();
                        // Events stream only on a connection that subscribed.
                        let mut subscribed = false;
                        while let Some(Ok(Message::Text(text))) = stream.next().await {
                            let request: Value = serde_json::from_str(text.as_str()).unwrap();
                            let method = request["method"].as_str().unwrap().to_owned();
                            let params = request["params"].clone();
                            log.lock().unwrap().push((method.clone(), params.clone()));
                            let (frames, close) = {
                            let mut host = host.lock().unwrap();
                            let hung = host.hung;
                            let receipt = json!({"wonderMessageId": MESSAGE, "conversationId": THREAD,
                                "deliveryState": "accepted_by_wonder", "queued": false,
                                "codexTurnId": null});
                            let approval = json!({"approvalId": "ap-1", "conversationId": THREAD,
                                "method": "item/commandExecution/requestApproval", "actionNonce": "n1",
                                "params": {"command": "npm ci", "reason": "Install dependencies?",
                                           "turnId": "turn-1",
                                           "availableDecisions": ["accept", "decline"]}});
                            let mut flushes = method == "approvals.list";
                            let reply = match method.as_str() {
                                "initialize" => Ok(json!({
                                    "protocolVersion": 1,
                                    "serverInfo": {"name": "stub", "version": "0", "apiVersion": "v1"},
                                    "authority": "owner", "hostEpoch": "epoch", "hostInstallationId": "i",
                                    "methods": [], "notifications": [],
                                    "capabilities": {"compactView": false, "maxInFlight": 32, "maxRequestBytes": 1024},
                                })),
                                "events.subscribe" => {
                                    subscribed = true;
                                    host.subscriptions += 1;
                                    match params["after"]["sequence"].as_u64() {
                                        // A resume replays what followed the
                                        // client's cursor: it must be the last
                                        // event sent.
                                        Some(after) if host.subscriptions > 1 => {
                                            assert_eq!(after, host.last, "resumed from a stale cursor");
                                            flushes = true;
                                        }
                                        _ => host.resubscribed = host.ended,
                                    }
                                    Ok(json!({"subscriptionId": format!("s{}", host.subscriptions),
                                        "cursor": {"hostEpoch": "epoch", "sequence": host.last}}))
                                }
                                "projects.list" => Ok(json!({"projects": [
                                    {"id": "p-fixture", "name": "Overnight fixtures 2026-10-18", "lastFamily": "claude"},
                                    {"id": "p-other", "name": "Owner chats", "lastFamily": "codex"},
                                ]})),
                                "models.list" => Ok(json!({"models": [
                                    {"id": "gpt-x", "agentFamily": "codex", "isDefault": true},
                                    {"id": "claude-a", "agentFamily": "claude", "isDefault": false},
                                    {"id": "claude-b", "agentFamily": "claude", "isDefault": true},
                                ]})),
                                "threads.create" => {
                                    host.pending.extend(events.clone());
                                    Ok(json!({"conversation": {"conversationId": THREAD}, "receipt": receipt}))
                                }
                                "threads.send" => {
                                    host.pending.extend(events.clone());
                                    Ok(receipt)
                                }
                                // Another thread's approval is never this run's.
                                "approvals.list" => Ok(json!([{"approvalId": "ap-elsewhere",
                                    "conversationId": "another-thread", "method": "item/tool/call",
                                    "actionNonce": "n2", "params": {}}])
                                    .as_array()
                                    .cloned()
                                    .map(|mut list| {
                                        if host.open {
                                            list.push(approval.clone());
                                        }
                                        Value::Array(list)
                                    })
                                    .unwrap()),
                                "approvals.resolve" => Err(wonderd::rpc::RpcError::from_http(
                                    401, b"paired device required")),
                                "threads.queue" => Ok(json!([{"id": "q-1", "clientMessageId": "c",
                                    "body": "next", "revision": 4, "attachmentIds": []}])),
                                "threads.read" => {
                                    let state = if host.resubscribed { "completed" } else { "streaming" };
                                    Ok(json!({"messages": [{"messageId": MESSAGE, "state": state,
                                        "codexTurnId": "turn-1"}],
                                        "thread": {"turns": [
                                        {"id": "turn-0", "status": "completed"},
                                        {"id": "turn-1", "status": "inProgress"}]}}))
                                }
                                "queue.edit" => Ok(json!({})),
                                other => panic!("unexpected {other}"),
                            };
                            let id = request["id"].clone();
                            let frame = match reply {
                                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                                Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
                            };
                            let mut frames = vec![frame.to_string()];
                            let close = flushes && subscribed && flush(&mut host, &mut frames);
                            if hung {
                                frames.clear();
                            }
                            (frames, close)
                            };
                            for frame in frames {
                                sink.send(Message::Text(frame.into())).await.unwrap();
                            }
                            if close {
                                return;
                            }
                        }
                    })
                }
            }),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap()
        });
        (Local::at(base, "cap".into()).unwrap(), calls)
    }

    fn chat_args(line: &[&str]) -> cli::ChatArgs {
        let cli = Cli::try_parse_from([&["wonder", "chat"], line].concat()).unwrap();
        match cli.command {
            Some(HostCommand::Chat(args)) => args,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_new_thread_streams_its_turn_and_ends_with_the_turns_status() {
        let (local, calls) = scripted_daemon(recorded_turn());
        let args = chat_args(&[
            "-p",
            "Say hello",
            "--project",
            "overnight FIXTURES 2026-10-18",
        ]);
        let mut seen = Vec::new();
        let status = chat_run(&local, &args, Duration::from_secs(60), &mut |e| {
            seen.push(e.clone())
        })
        .await
        .unwrap();
        assert_eq!(status, ChatStatus::Completed);
        let lines: Vec<Value> = seen
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        assert_eq!(
            lines,
            vec![
                json!({"type": "sent", "projectId": "p-fixture", "threadId": THREAD,
                       "messageId": MESSAGE, "created": true, "queued": false}),
                json!({"type": "state", "state": "dispatching_to_codex"}),
                json!({"type": "state", "state": "accepted_by_codex"}),
                json!({"type": "tool", "category": "command", "state": "started", "detail": "ls"}),
                json!({"type": "tool", "category": "reasoning", "state": "working", "detail": null}),
                json!({"type": "approval", "approvalId": "ap-1",
                       "kind": "item/commandExecution/requestApproval",
                       "summary": "The agent wants to run `npm ci`. Install dependencies?",
                       "decisions": ["accept", "decline"]}),
                json!({"type": "approvalResolved", "approvalId": "ap-1", "decision": "accept"}),
                json!({"type": "text", "text": "Hello"}),
                json!({"type": "text", "text": ", world"}),
                json!({"type": "message", "text": "Hello, world"}),
                json!({"type": "end", "status": "completed", "exitCode": 0, "detail": null}),
            ]
        );
        let calls = calls.lock().unwrap().clone();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "events.subscribe",
                "projects.list",
                "models.list",
                "threads.create",
                "approvals.list",
                "approvals.list",
                "approvals.list",
                "approvals.list"
            ]
        );
        // The Project's last provider and that provider's default model; the
        // sender is the session, never a param.
        let create = &calls[4].1;
        assert_eq!(create["projectId"], "p-fixture");
        assert_eq!(
            (&create["family"], &create["model"]),
            (&json!("claude"), &json!("claude-b"))
        );
        assert_eq!(create["body"], "Say hello");
        assert!(uuid::Uuid::parse_str(create["idempotencyKey"].as_str().unwrap()).is_ok());
        assert!(create.get("deviceId").is_none());
    }

    #[tokio::test]
    async fn an_existing_thread_switches_model_and_a_failed_turn_exits_nonzero() {
        let failed = vec![envelope(
            1,
            Some(MESSAGE),
            Some("t"),
            state(DeliveryState::Failed),
        )];
        let (local, calls) = scripted_daemon(failed);
        let args = chat_args(&["-p", "again", "--thread", THREAD, "--model", "gpt-x"]);
        let mut seen = Vec::new();
        let status = chat_run(&local, &args, Duration::from_secs(60), &mut |e| {
            seen.push(e.clone())
        })
        .await
        .unwrap();
        assert_eq!(status, ChatStatus::Failed);
        assert!(matches!(
            seen.last(),
            Some(ChatEvent::End { exit_code: 1, .. })
        ));
        let calls = calls.lock().unwrap().clone();
        let send = calls.iter().find(|(m, _)| m == "threads.send").unwrap();
        assert_eq!(send.1["threadId"], THREAD);
        assert_eq!(
            send.1["projectModel"],
            json!({"family": "codex", "model": "gpt-x"})
        );
    }

    /// Send now: the message waits behind another message's turn, then
    /// joins it (Codex) or starts its own (Claude). The turn ahead's reply
    /// is not this message's; once joined, the turn's events and its end
    /// count although they name the message that started it.
    #[test]
    fn a_message_sent_into_a_running_reply_follows_the_turn_it_joined() {
        let text = |text: &str| WonderEvent::AssistantDelta { text: text.into() };
        let completed = || state(DeliveryState::Completed);
        let mut follow = ChatFollow {
            thread: THREAD.into(),
            message: MESSAGE.into(),
            turn: None,
        };
        let mut classify = |e: Value| follow.classify(&e);
        // Queued: the running turn's reply, tools and end are not ours.
        assert_eq!(
            classify(envelope(1, Some("earlier"), Some("turn-0"), text("ahead"))),
            None
        );
        assert_eq!(
            classify(envelope(
                2,
                None,
                Some("turn-0"),
                activity("command", "started", Some("ls"))
            )),
            None
        );
        assert_eq!(
            classify(envelope(3, None, None, activity("plan", "updated", None))),
            None
        );
        // Send now joins turn-0.
        assert_eq!(
            classify(envelope(
                4,
                Some(MESSAGE),
                Some("turn-0"),
                state(DeliveryState::AcceptedByCodex)
            )),
            Some(ChatStep::Emit(ChatEvent::State {
                state: "accepted_by_codex".into()
            }))
        );
        assert_eq!(
            classify(envelope(5, Some("earlier"), Some("turn-0"), text("joined"))),
            Some(ChatStep::Emit(ChatEvent::Text {
                text: "joined".into()
            }))
        );
        // Another message's state in the turn is not an end, its end is.
        assert_eq!(
            classify(envelope(
                6,
                Some("later"),
                Some("turn-0"),
                state(DeliveryState::AcceptedByCodex)
            )),
            None
        );
        assert_eq!(
            classify(envelope(7, Some("earlier"), Some("turn-0"), completed())),
            Some(ChatStep::End(ChatStatus::Completed))
        );

        // Claude answers a Send now as its own turn; the turn it was sent
        // into ends interrupted and is not this message's end.
        let mut follow = ChatFollow {
            thread: THREAD.into(),
            message: MESSAGE.into(),
            turn: None,
        };
        let mut classify = |e: Value| follow.classify(&e);
        classify(envelope(
            1,
            Some(MESSAGE),
            Some("turn-2"),
            state(DeliveryState::AcceptedByCodex),
        ));
        assert_eq!(
            classify(envelope(
                2,
                Some("earlier"),
                Some("turn-0"),
                state(DeliveryState::Interrupted)
            )),
            None
        );
        assert_eq!(
            classify(envelope(3, Some("earlier"), Some("turn-0"), text("old"))),
            None
        );
        assert_eq!(
            classify(envelope(4, Some(MESSAGE), Some("turn-2"), text("mine"))),
            Some(ChatStep::Emit(ChatEvent::Text {
                text: "mine".into()
            }))
        );

        // A Send now that missed the reply goes back to the queue: the turn
        // it named is no longer its own, even if the envelope still names it.
        let mut follow = ChatFollow {
            thread: THREAD.into(),
            message: MESSAGE.into(),
            turn: Some("turn-0".into()),
        };
        follow.classify(&envelope(
            1,
            Some(MESSAGE),
            Some("turn-0"),
            state(DeliveryState::AcceptedByWonder),
        ));
        assert_eq!(follow.turn, None);
        assert_eq!(
            follow.classify(&envelope(2, Some("earlier"), Some("turn-0"), completed())),
            None
        );
    }

    /// The connection drops mid-turn: the CLI resumes after the last event
    /// it read (the host replays the rest), so the reply and its end arrive.
    #[tokio::test]
    async fn a_connection_lost_mid_turn_resumes_after_its_cursor_and_sees_the_end() {
        let text = |text: &str| WonderEvent::AssistantDelta { text: text.into() };
        let (local, calls) = scripted_daemon(vec![
            envelope(
                1,
                Some(MESSAGE),
                Some("turn-1"),
                state(DeliveryState::AcceptedByCodex),
            ),
            step("pause"),
            envelope(2, Some(MESSAGE), Some("turn-1"), text("Hel")),
            step("close"),
            envelope(3, Some(MESSAGE), Some("turn-1"), text("lo")),
            envelope(
                4,
                Some(MESSAGE),
                Some("turn-1"),
                state(DeliveryState::Completed),
            ),
        ]);
        let args = chat_args(&["-p", "hi", "--thread", THREAD]);
        let mut seen = Vec::new();
        let status = chat_run(&local, &args, Duration::from_secs(60), &mut |e| {
            seen.push(e.clone())
        })
        .await
        .unwrap();
        assert_eq!(status, ChatStatus::Completed);
        let text: String = seen
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        // Nothing is missing, so nothing says so.
        assert!(!seen.iter().any(|e| matches!(e, ChatEvent::Notice { .. })));
        let calls = calls.lock().unwrap().clone();
        let subscribes: Vec<&Value> = calls
            .iter()
            .filter(|(m, _)| m == "events.subscribe")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(
            subscribes,
            [
                &json!({}),
                &json!({"after": {"hostEpoch": "epoch", "sequence": 2}})
            ]
        );
        assert_eq!(calls.iter().filter(|(m, _)| m == "initialize").count(), 2);
    }

    /// The host cannot replay (`resync`) and the turn ends before the CLI
    /// follows again: it subscribes from now before reading the thread, so
    /// it sees that end rather than waiting for one already past.
    #[tokio::test]
    async fn a_stream_that_resyncs_mid_turn_still_sees_an_end_committed_in_the_gap() {
        let (local, calls) = scripted_daemon(vec![
            envelope(
                1,
                Some(MESSAGE),
                Some("turn-1"),
                state(DeliveryState::AcceptedByCodex),
            ),
            step("pause"),
            step("ended"),
            envelope(
                2,
                Some(MESSAGE),
                Some("turn-1"),
                state(DeliveryState::Completed),
            ),
        ]);
        let args = chat_args(&["-p", "hi", "--thread", THREAD]);
        let mut seen = Vec::new();
        let status = chat_run(&local, &args, Duration::from_secs(60), &mut |e| {
            seen.push(e.clone())
        })
        .await
        .unwrap();
        assert_eq!(status, ChatStatus::Completed);
        assert!(seen.iter().any(|e| matches!(e, ChatEvent::Notice { .. })));
        let calls = calls.lock().unwrap().clone();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        let resubscribe = methods
            .iter()
            .rposition(|m| *m == "events.subscribe")
            .unwrap();
        let read = methods.iter().position(|m| *m == "threads.read").unwrap();
        assert!(resubscribe > 0 && resubscribe < read, "{methods:?}");
        assert_eq!(calls[resubscribe].1, json!({}));
    }

    /// The connection drops right after an approval opens, while the CLI
    /// reads approvals: the read fails, or replaces the connection without
    /// the subscription. Either way the CLI resumes after its cursor on the
    /// new connection, so it still shows the approval and the turn's end.
    #[tokio::test]
    async fn a_connection_replaced_by_a_call_resumes_the_stream_on_the_new_one() {
        let (local, calls) = scripted_daemon(vec![
            envelope(
                1,
                Some(MESSAGE),
                Some("turn-1"),
                state(DeliveryState::AcceptedByCodex),
            ),
            step("pause"),
            envelope(
                2,
                None,
                Some("turn-1"),
                WonderEvent::ApprovalOpened {
                    request_id: "r".into(),
                },
            ),
            step("close"),
            envelope(
                3,
                Some(MESSAGE),
                Some("turn-1"),
                state(DeliveryState::Completed),
            ),
        ]);
        let args = chat_args(&["-p", "hi", "--thread", THREAD]);
        let mut seen = Vec::new();
        let status = chat_run(&local, &args, Duration::from_secs(60), &mut |e| {
            seen.push(e.clone())
        })
        .await
        .unwrap();
        assert_eq!(status, ChatStatus::Completed, "{seen:?}");
        let approvals = seen
            .iter()
            .filter(
                |e| matches!(e, ChatEvent::Approval { approval_id, .. } if approval_id == "ap-1"),
            )
            .count();
        assert_eq!(approvals, 1, "{seen:?}");
        let calls = calls.lock().unwrap().clone();
        assert!(calls.iter().any(|(m, p)| m == "events.subscribe"
            && p == &json!({"after": {"hostEpoch": "epoch", "sequence": 2}})));
    }

    /// The host stops answering mid-turn: `--timeout` still ends the
    /// command on time with `end` (exit 75), not the call's own timeout or
    /// an error.
    #[tokio::test]
    async fn a_host_that_stops_answering_ends_timed_out_at_the_deadline() {
        let (local, _) = scripted_daemon(vec![
            envelope(
                1,
                Some(MESSAGE),
                Some("turn-1"),
                state(DeliveryState::AcceptedByCodex),
            ),
            step("hang"),
        ]);
        let args = chat_args(&["-p", "hi", "--thread", THREAD]);
        let mut seen = Vec::new();
        let started = std::time::Instant::now();
        let mut record = |e: &ChatEvent| seen.push(e.clone());
        let run = chat_run(&local, &args, Duration::from_millis(500), &mut record);
        let status = tokio::time::timeout(Duration::from_secs(60), run)
            .await
            .expect("followed past --timeout")
            .unwrap();
        assert_eq!(status, ChatStatus::TimedOut);
        // The 500 ms --timeout ended it, well before the hang guard; 10 s
        // leaves a busy Mac room while still telling the two apart.
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(matches!(
            seen.last(),
            Some(ChatEvent::End { exit_code: 75, .. })
        ));
    }

    #[tokio::test]
    async fn a_turn_still_running_at_the_timeout_ends_timed_out() {
        let (local, _) = scripted_daemon(vec![]);
        let args = chat_args(&["-p", "slow", "--thread", THREAD]);
        let mut seen = Vec::new();
        let status = chat_run(&local, &args, Duration::from_millis(300), &mut |e| {
            seen.push(e.clone())
        })
        .await
        .unwrap();
        assert_eq!(status, ChatStatus::TimedOut);
        assert!(matches!(
            seen.last(),
            Some(ChatEvent::End { exit_code: 75, .. })
        ));
    }

    #[tokio::test]
    async fn an_unknown_project_names_the_ones_there_and_sends_nothing() {
        let (local, calls) = scripted_daemon(vec![]);
        let args = chat_args(&["-p", "hi", "--project", "Nope"]);
        let error = chat_run(&local, &args, Duration::from_secs(60), &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "No Project is named “Nope”. Projects here: Overnight fixtures 2026-10-18, Owner chats."
        );
        assert!(!calls
            .lock()
            .unwrap()
            .iter()
            .any(|(m, _)| m.starts_with("threads.")));
    }

    #[tokio::test]
    async fn send_now_steers_the_queued_message_into_the_running_turn() {
        let (local, calls) = scripted_daemon(vec![]);
        chat_send_now(&local, "q-1", THREAD, true).await.unwrap();
        let calls = calls.lock().unwrap().clone();
        let edit = calls.iter().find(|(m, _)| m == "queue.edit").unwrap();
        assert_eq!(
            edit.1,
            json!({"threadId": THREAD, "messageId": "q-1", "expectedRevision": 4,
                   "expectedTurnId": "turn-1"})
        );
        let missing = chat_send_now(&local, "q-9", THREAD, true)
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("not waiting"));
    }

    #[tokio::test]
    async fn the_owners_approval_answer_is_refused_by_the_host_and_says_where_to_answer() {
        let (local, _) = scripted_daemon(vec![]);
        let error = chat_answer(&local, "ap-1", true, true).await.unwrap_err();
        assert!(error.to_string().contains("No approval"), "{error}");
        // A turn opens `ap-1`; the owner's answer reaches the host, which
        // accepts only a paired device's signed decision.
        let opened = vec![
            envelope(
                1,
                Some(MESSAGE),
                Some("turn-1"),
                state(DeliveryState::AcceptedByCodex),
            ),
            step("pause"),
            envelope(
                2,
                None,
                Some("turn-1"),
                WonderEvent::ApprovalOpened {
                    request_id: "r".into(),
                },
            ),
        ];
        let (local, calls) = scripted_daemon(opened);
        let args = chat_args(&["-p", "x", "--thread", THREAD]);
        let mut seen = Vec::new();
        // The approval must arrive before the limit; a loaded Windows/Wine runner
        // needs far longer than 300 ms to connect, subscribe and send.
        let status = chat_run(&local, &args, Duration::from_secs(10), &mut |e| {
            seen.push(e.clone())
        })
        .await
        .unwrap();
        assert_eq!(status, ChatStatus::TimedOut);
        assert!(seen.iter().any(
            |e| matches!(e, ChatEvent::Approval { approval_id, .. } if approval_id == "ap-1")
        ));
        let error = chat_answer(&local, "ap-1", false, true).await.unwrap_err();
        assert!(error.downcast_ref::<Unavailable>().is_some(), "{error}");
        assert!(error.to_string().contains("on your iPhone"));
        let calls = calls.lock().unwrap().clone();
        let resolve = calls
            .iter()
            .find(|(m, _)| m == "approvals.resolve")
            .unwrap();
        assert_eq!(resolve.1["approvalId"], "ap-1");
        assert_eq!(resolve.1["decision"], "decline");
        assert_eq!(resolve.1["actionNonce"], "n1");
    }

    #[test]
    fn chat_parses_a_send_or_one_subcommand() {
        assert!(Cli::try_parse_from(["wonder", "chat", "queue", "--thread", "t"]).is_ok());
        assert!(Cli::try_parse_from(["wonder", "chat", "approve", "a1"]).is_ok());
        assert!(Cli::try_parse_from(["wonder", "chat", "send-now", "q", "--thread", "t"]).is_ok());
        // A send needs a message and a Project or thread, never both thread and --new.
        assert!(Cli::try_parse_from(["wonder", "chat", "--project", "P"]).is_err());
        assert!(Cli::try_parse_from(["wonder", "chat", "-p", "m"]).is_err());
        assert!(
            Cli::try_parse_from(["wonder", "chat", "-p", "m", "--thread", "t", "--new"]).is_err()
        );
        let args = chat_args(&[
            "-p",
            "m",
            "--thread",
            "t",
            "--provider",
            "opencode",
            "--json",
        ]);
        assert_eq!(args.provider, Some(Provider::Opencode));
        // Cursor is one of the host's providers, so the command line takes it.
        let args = chat_args(&["-p", "m", "--project", "P", "--provider", "cursor"]);
        assert_eq!(args.provider.map(Provider::as_str), Some("cursor"));
        for provider in ["codex", "claude", "opencode", "cursor"] {
            assert!(Cli::try_parse_from(["cupola", "providers", "login", provider]).is_ok());
            assert!(Cli::try_parse_from([
                "cupola",
                "providers",
                "login",
                provider,
                "--account",
                "added"
            ])
            .is_err());
            assert!(Cli::try_parse_from(["cupola", "providers", "accounts", provider]).is_err());
            assert!(Cli::try_parse_from(["cupola", "providers", "logout", provider]).is_err());
        }
    }
}

#[cfg(test)]
mod status_wording_tests {
    use super::*;

    #[test]
    fn fresh_host_without_an_agent_explains_setup_on_its_own_computer() {
        for platform in ["linux", "windows", "macos"] {
            let host = json!({"platform":platform,"hostName":"Fresh computer",
                "state":"degraded","publicOrigin":"https://fresh.example.ts.net",
                "execution":{"ready":false,"reason":"runtime_unavailable",
                    "detail":"This agent is reconnecting."}});
            let providers = json!({"providers":[
                {"id":"codex","running":false,"starting":false,"incompatible":false},
                {"id":"claude","running":false,"starting":false,"incompatible":false}]});
            let mut output = Vec::new();
            write_host_status(&mut output, &host, Some(&providers)).unwrap();
            let output = String::from_utf8(output).unwrap();
            println!("{platform}:\n{output}");
            assert!(output.contains("State:    waiting for agent"), "{output}");
            assert!(output.contains("No agent is connected yet."), "{output}");
            assert!(output.contains("cupola providers"), "{output}");
            assert!(output.contains("cupola doctor"), "{output}");
            assert!(
                !output.contains("reconnecting") && !output.contains("your Mac"),
                "{output}"
            );
        }
    }

    #[test]
    fn status_keeps_runtime_failures_recovery_and_unknown_reports_visible() {
        let host = json!({"state":"degraded", "execution":{
            "ready":false,"reason":"runtime_unavailable","detail":"Check the agent runtime."}});
        let idle = json!({"providers":[{"running":false,"starting":false,"incompatible":false}]});
        for (reason, providers) in [
            ("runtime_incompatible", Some(idle.clone())),
            ("storage_unavailable", Some(idle.clone())),
            ("recovering", Some(idle)),
            ("runtime_unavailable", None),
            ("runtime_unavailable", Some(json!({"providers":[]}))),
            ("runtime_unavailable", Some(json!({"providers":[{}]}))),
            (
                "runtime_unavailable",
                Some(json!({"providers":[{"running":false,"starting":true,"incompatible":false}]})),
            ),
            (
                "runtime_unavailable",
                Some(json!({"providers":[{"running":false,"starting":false,"incompatible":true}]})),
            ),
            (
                "runtime_unavailable",
                Some(json!({"providers":[{"running":true,"starting":false,"incompatible":false}]})),
            ),
        ] {
            let mut host = host.clone();
            host["execution"]["reason"] = reason.into();
            let mut output = Vec::new();
            write_host_status(&mut output, &host, providers.as_ref()).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("State:    degraded"), "{reason}: {output}");
            assert!(
                output.contains("Agents:   Check the agent runtime."),
                "{output}"
            );
        }
    }
}

/// Golden output for the people-facing text of `init`, `devices`,
/// `providers` and `doctor`: the whole text, so a regression in wording,
/// order or alignment shows as a diff.
#[cfg(test)]
mod cli_message_tests {
    use super::*;
    use cli::{Permission, PermissionState};

    const NOW: u64 = 1_790_000_000_000;
    const MINUTE: u64 = 60_000;

    fn text(write: impl FnOnce(&mut Vec<u8>) -> std::io::Result<()>) -> String {
        let mut out = Vec::new();
        write(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// A second `init` while a host holds the lock fails with the stop-first
    /// hint and leaves the running host's settings alone.
    #[test]
    fn init_while_a_host_runs_says_to_stop_it_first() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::at(Some(directory.path().into())).unwrap();
        secure_dirs(&paths).unwrap();
        let _host = supervisor_lock(&paths).unwrap();
        let error = init_at(&paths, &init_args(None), false).unwrap_err();
        assert_eq!(
            format!("cupola: {error}"),
            "cupola: Cupola host is running. Stop it first with `cupola stop`, then try again."
        );
        assert!(!paths.config().exists());
        assert!(!paths.capability().exists());
    }

    pub(crate) fn stored_host_name(paths: &Paths) -> Option<String> {
        let url = format!(
            "sqlite://{}?mode=rwc",
            paths.data.join("wonder.sqlite3").display()
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let store = wonder_store::Store::connect(&url).await.unwrap();
                store.host_name().await.unwrap()
            })
    }

    fn init_args(name: Option<&str>) -> InitArgs {
        InitArgs {
            name: name.map(str::to_owned),
            tunnel: None,
            origin: None,
            https_port: None,
            listen: None,
            computer: false,
            no_computer: false,
        }
    }

    // Contract: set up again with only --name, the host keeps its port and
    // computer view; the name replaces an earlier chosen one, since
    // `host_name::current` shows the stored name over the computer's own.
    #[test]
    fn init_again_with_only_a_name_keeps_the_other_settings() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::at(Some(directory.path().into())).unwrap();
        let first = InitArgs {
            https_port: Some(8443),
            computer: true,
            ..init_args(Some(" Studio "))
        };
        init_at(&paths, &first, true).unwrap();
        assert_eq!(stored_host_name(&paths).as_deref(), Some("Studio"));
        let before = load_config(&paths).unwrap();
        assert_eq!(
            (&before["httpsPort"], &before["computer"]),
            (&json!(8443), &json!(true))
        );

        init_at(&paths, &init_args(Some("Work Mac")), true).unwrap();
        let after = load_config(&paths).unwrap();
        assert_eq!(after, before);
        assert_eq!(stored_host_name(&paths).as_deref(), Some("Work Mac"));
        // Without --name the chosen name stays; --no-computer turns view off.
        let off = InitArgs {
            no_computer: true,
            ..init_args(None)
        };
        init_at(&paths, &off, true).unwrap();
        assert_eq!(load_config(&paths).unwrap()["computer"], false);
        assert_eq!(stored_host_name(&paths).as_deref(), Some("Work Mac"));
        let error = init_at(&paths, &init_args(Some("Two\nlines")), true).unwrap_err();
        assert!(error.to_string().contains("--name"), "{error}");
    }

    // A changed away-from-home address is reported, and only phones that
    // cannot use the same Wi-Fi route are told to pair again.
    #[test]
    fn init_reports_a_changed_route_without_claiming_phones_stay_paired() {
        let previous = json!({"tunnel": "tailscale", "httpsPort": 8443});
        let same = init_next_step(Some(&previous), &previous);
        assert!(same.contains("Paired phones stay paired"), "{same}");
        let moved = json!({"tunnel": "tailscale", "httpsPort": 443});
        let text = init_next_step(Some(&previous), &moved);
        assert!(!text.contains("stay paired"), "{text}");
        assert!(
            text.contains("from Tailscale on port 8443 to Tailscale on port 443"),
            "{text}"
        );
        if home_port(&moved).is_some() {
            assert!(text.contains("still connect on the same Wi-Fi"), "{text}");
        }
        let lan_off = json!({"tunnel": "none", "origin": "https://wonder.example",
                             "homeNetwork": {"enabled": false, "port": 3778}});
        let text = init_next_step(Some(&previous), &lan_off);
        assert!(text.contains("to https://wonder.example"), "{text}");
        assert!(
            text.contains("cannot reach it at the new address"),
            "{text}"
        );
    }

    /// Bare `permissions` runs the status path end to end: it prints each
    /// permission and never asks the system for one.
    #[test]
    fn bare_permissions_shows_status_without_prompting() {
        let Some(HostCommand::Permissions { command }) =
            Cli::try_parse_from(["cupola", "permissions"])
                .unwrap()
                .command
        else {
            panic!("permissions");
        };
        let out = text(|out| {
            show_permissions(
                command,
                false,
                out,
                || panic!("bare `permissions` must not prompt"),
                || {
                    Ok(PermissionsOutput {
                        permissions: vec![Permission {
                            id: "screenRecording",
                            state: PermissionState::Denied,
                            summary: "Screen Recording: not allowed".into(),
                            fix: Some("Run `cupola permissions grant`.".into()),
                        }],
                    })
                },
            )
            .map_err(|error| std::io::Error::other(error.to_string()))
        });
        assert_eq!(
            out,
            "Screen Recording: not allowed\n  Run `cupola permissions grant`.\n"
        );
        let none = text(|out| {
            show_permissions(
                None,
                false,
                out,
                || panic!("prompted"),
                || {
                    Ok(PermissionsOutput {
                        permissions: Vec::new(),
                    })
                },
            )
            .map_err(|error| std::io::Error::other(error.to_string()))
        });
        let platform = if cfg!(windows) { "Windows" } else { "Linux" };
        assert_eq!(
            none,
            format!("Cupola needs no system permissions on {platform}.\n")
        );
    }

    #[test]
    fn init_keeps_existing_phones() {
        let config = json!({"tunnel": "tailscale", "httpsPort": 443});
        assert_eq!(
            init_next_step(Some(&config), &config),
            "Next: `cupola start` to apply these settings. Paired phones stay paired; \
             there is no need to pair again."
        );
        let fresh = init_next_step(None, &config);
        assert!(fresh.contains("`cupola pair`"), "{fresh}");
        if cfg!(windows) {
            // Same-Wi-Fi pairing is refused on Windows, so the first setup says Tailscale.
            assert!(fresh.contains("connects through Tailscale"), "{fresh}");
            assert!(!fresh.contains("Wi-Fi"), "{fresh}");
        } else {
            // A first setup pairs on the same Wi-Fi; Tailscale is not needed for it.
            assert!(
                fresh.ends_with("on the same Wi-Fi as this computer."),
                "{fresh}"
            );
            assert!(!fresh.contains("Tailscale"), "{fresh}");
        }
    }

    #[test]
    fn devices_say_when_each_phone_was_last_seen() {
        let list = [
            json!({"id":"a1","label":"Work iPhone","lastSeenAt":(NOW - 30_000).to_string(),"revokedAt":null}),
            json!({"id":"b2","label":"iPad","lastSeenAt":(NOW - 5 * MINUTE).to_string(),"revokedAt":null}),
            json!({"id":"c3","label":"Old phone","lastSeenAt":"2026-09-01T10:00:00Z","revokedAt":"1789000000000"}),
            json!({"id":"d4","label":"New","lastSeenAt":null,"revokedAt":null}),
        ];
        assert_eq!(
            text(|out| write_devices(out, &list, NOW)),
            "\
a1  Work iPhone  last seen just now
b2  iPad         last seen 5 minutes ago
c3  Old phone    last seen 20 days ago  (disconnected)
d4  New          last seen never
Rename one with `cupola devices rename <id> <name>`; disconnect one with `cupola devices revoke <id>`.
"
        );
        assert_eq!(
            text(|out| write_devices(out, &[], NOW)),
            "No paired phones. Run `cupola pair`.\n"
        );
    }

    #[test]
    fn relative_times_read_as_people_say_them() {
        let ago = |ms: u64| relative_time(&(NOW - ms).to_string(), NOW);
        assert_eq!(ago(59_999), "just now");
        assert_eq!(ago(MINUTE), "1 minute ago");
        assert_eq!(ago(59 * MINUTE), "59 minutes ago");
        assert_eq!(ago(60 * MINUTE), "1 hour ago");
        assert_eq!(ago(23 * 60 * MINUTE), "23 hours ago");
        assert_eq!(ago(30 * 60 * MINUTE), "yesterday");
        assert_eq!(ago(3 * 24 * 60 * MINUTE), "3 days ago");
        assert_eq!(ago(40 * 24 * 60 * MINUTE), "on 2026-08-12");
        // A clock ahead of this computer's reads as now, never negative.
        assert_eq!(relative_time(&(NOW + MINUTE).to_string(), NOW), "just now");
        assert_eq!(relative_time("soon", NOW), "soon");
    }

    #[test]
    fn providers_print_each_sign_in_from_the_running_host() {
        let entry = |id: &str, state: &str| {
            json!({"id":id,"running":false,"starting":false,"incompatible":false,
                "updatePending":false,"detail":null,"state":state,"version":"",
                "account":"","working":false})
        };
        let mut codex = entry("codex", "signedIn");
        codex["account"] = "ChatGPT account".into();
        codex["version"] = "0.163.0".into();
        codex["working"] = true.into();
        let mut cursor = entry("cursor", "unavailable");
        cursor["detail"] = "Cursor is reconnecting.".into();
        let list = [
            codex,
            entry("claude", "signedOut"),
            entry("opencode", "installed"),
            cursor,
        ];
        assert_eq!(
            text(|out| write_providers(out, &list)),
            "\
Codex        Signed in as ChatGPT account (0.163.0), working now
Claude Code  Signed out. Sign in to your Claude subscription with `cupola providers login claude`.
OpenCode     Installed
Cursor       Not available right now. Run `cupola doctor`.
             Cursor is reconnecting.
"
        );
        let mut subscription = entry("claude", "needsSubscription");
        subscription["account"] = "Claude Free".into();
        assert_eq!(
            text(|out| write_providers(
                out,
                &[
                    subscription,
                    entry("cursor", "notInstalled"),
                    entry("codex", "unsupported")
                ]
            )),
            format!(
                "\
Claude Code  Signed in as Claude Free, but the account's plan does not include Claude Code
Cursor       {}
Codex        This version is not supported yet. Update Cupola on this computer.
",
                cursor_install_hint()
            )
        );
        assert!(cursor_install_hint().contains("`agent login`"));
        // Cursor's sign-in is the Cursor CLI's own; OpenCode has none of
        // Cupola's, only the providers configured in OpenCode.
        assert_eq!(
            text(|out| write_providers(
                out,
                &[entry("cursor", "signedOut"), entry("opencode", "signedOut")]
            )),
            "\
Cursor    Signed out. Sign in to Cursor with `cupola providers login cursor` (runs the Cursor CLI's `agent login`).
OpenCode  Signed out. Uses OpenCode's own providers; add one with `opencode auth login` if you need more models.
"
        );
    }

    #[test]
    fn providers_fall_back_to_what_is_installed_when_the_host_is_down() {
        let runtimes = LocalRuntimes {
            codex: Some((
                PathBuf::from("/opt/codex"),
                Some("codex-cli 0.163.0".into()),
            )),
            claude_bridge: None,
            node: Some(PathBuf::from("/usr/bin/node")),
            opencode_bridge: Some(PathBuf::from("/opt/acp-runtime")),
            opencode_cli: false,
            cursor_cli: Some(PathBuf::from("/opt/agent")),
        };
        assert_eq!(
            text(|out| write_local_providers(out, &runtimes, false)),
            "\
Cupola host is not running, so sign-in is not shown. Start it with `cupola start` to see it; installed here:
Codex:    /opt/codex (codex-cli 0.163.0)
Claude:   runtime bridge not installed (share/wonder/claude-runtime).
OpenCode: Not installed. Install the OpenCode CLI (https://opencode.ai/docs/).
Cursor:   Sign in to Cursor with `cupola providers login cursor` (runs the Cursor CLI's `agent login`).
"
        );
        assert!(runtimes.cursor().installed);
        assert!(text(|out| write_local_providers(out, &runtimes, true))
            .starts_with("The host did not report sign-in; showing what is installed here.\n"));
    }

    #[test]
    fn doctor_calls_cua_driver_optional_and_links_its_guide() {
        let checks = [
            cua_driver_check(None),
            cua_driver_check(Some(Path::new("/usr/local/bin/cua-driver"))),
        ];
        assert_eq!(
            text(|out| write_checks(out, &checks)),
            "\
warn  Cua Driver is not installed (optional): agents can't use apps on this computer
      Optional. To let agents use apps here, install Cua Driver from its guide (https://cua.ai/docs/cua-driver/quickstart), then run `cupola doctor` again.
ok    Cua Driver at /usr/local/bin/cua-driver
"
        );
        assert!(!checks[0].fix.as_deref().unwrap().contains("| "));
    }

    #[test]
    fn device_actions_say_what_changed() {
        let output = |state, label: Option<&str>| DeviceActionOutput {
            device_id: "d1".into(),
            state,
            label: label.map(str::to_owned),
        };
        assert_eq!(
            device_action_text(&output(DeviceState::Revoked, None)),
            "Disconnected d1. It can no longer reach this computer; \
             `cupola devices forget d1` removes it from the list."
        );
        assert_eq!(
            device_action_text(&output(DeviceState::Forgotten, None)),
            "Removed d1 from the list."
        );
        assert_eq!(
            device_action_text(&output(DeviceState::Renamed, Some("Work iPhone"))),
            "Renamed d1 to \"Work iPhone\"."
        );
    }
}
