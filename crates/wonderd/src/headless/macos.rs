//! One windowless bundle owns the host and every TCC-using process.
//! LaunchServices severs stdio, so helper clients use a private authenticated
//! Unix socket. The wire format remains the daemon's existing JSONL protocol.
use super::*;
use cli::{Channel, Permission, PermissionState, ServiceCommand, ServiceOutput, UpdateOutput};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};

const BUNDLE_ID: &str = "com.saimun.wonder";
const STAGING_BUNDLE_ID: &str = "com.saimun.wonder.cli-staging";
const TEAM_ID: &str = "8KKNVD7758";
const PROXY: &str = "__macos-computer-use";
const SESSION: &str = "__macos-helper-session";
const SERVE: &str = "__macos-serve";
const GRANT: &str = "__macos-permissions-grant";

fn bundle_for_executable(exe: &Path) -> Result<PathBuf> {
    let exe = wonderd::canonical_path::canonicalize(exe)?;
    let contents = exe
        .parent()
        .and_then(Path::parent)
        .ok_or("missing app bundle")?;
    if contents.file_name().is_none_or(|name| name != "Contents")
        || exe
            .parent()
            .and_then(Path::file_name)
            .is_none_or(|name| name != "MacOS")
        || !contents.join("Info.plist").is_file()
    {
        return Err("Run the signed cupola CLI inside Cupola.app (or its PATH symlink).".into());
    }
    Ok(contents.parent().ok_or("missing app bundle")?.to_owned())
}

fn bundle() -> Result<PathBuf> {
    bundle_for_executable(&executable_path()?)
}

fn checked(command: &mut Command) -> Result<process::Output> {
    let output = process::output(command)?;
    if !output.status.success() {
        return Err(format!(
            "{} failed: {}",
            command.get_program().to_string_lossy(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(output)
}

fn open_command(app: &Path, args: &[std::ffi::OsString]) -> Command {
    // -n is essential: an existing host must not swallow a second helper's args.
    let mut command = Command::new("/usr/bin/open");
    command
        .args(["-g", "-n", "-a"])
        .arg(app)
        .arg("--args")
        .args(args);
    command
}

fn open_app(app: &Path, args: &[std::ffi::OsString]) -> Result<()> {
    checked(&mut open_command(app, args))?;
    Ok(())
}

pub(super) fn dispatch_internal() -> Option<Result<()>> {
    let mut args = std::env::args_os();
    let invocation = args.next()?;
    if Path::new(&invocation)
        .file_name()
        .is_some_and(|name| name == "wonder-computer-use")
    {
        return Some(bundle().and_then(|app| proxy(&app, args.collect())));
    }
    let command = args.next()?;
    match command.to_str()? {
        PROXY => Some(bundle().and_then(|app| proxy(&app, args.collect()))),
        SESSION => Some(helper_session(args.collect())),
        SERVE => Some(serve(args.collect())),
        GRANT => Some(run_helper(&[
            "--permissions".into(),
            "--request-screen".into(),
            "--request-input".into(),
        ])),
        _ => None,
    }
}

pub(super) fn helper_launcher() -> Result<PathBuf> {
    let path = bundle()?.join("Contents/Helpers/wonder-computer-use");
    if !path.is_file() {
        return Err("The bundled computer-use launcher is missing; reinstall Cupola.".into());
    }
    Ok(path)
}

fn wait_for_helper(command: &mut Command, mut lifetime: Option<UnixStream>) -> Result<()> {
    // Preserve the LaunchServices principal for the child's entire lifetime.
    // Keep its signed executable available while ScreenCaptureKit resolves the
    // responsible app, instead of replacing it with a plain helper.
    let child = OwnedChild::spawn(command)?;
    while !child.exited()? {
        if let Some(lifetime) = lifetime.as_mut() {
            // JSONL stdin can half-close while the helper drains queued replies.
            // This separate connection closes only when the proxy ends, including
            // SIGKILL from the daemon's kill_on_drop/timeout cleanup. The session
            // owns the real helper's process group, not the detached proxy's group.
            let mut poll = libc::pollfd {
                fd: lifetime.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 100) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.into());
                }
            } else if ready > 0 {
                match lifetime.read(&mut [0]) {
                    // Drop kills and reaps even a helper ignoring EOF/SIGTERM.
                    Ok(0) => return Ok(()),
                    Ok(_) => return Err("unexpected helper lifetime data".into()),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let status = child.finish()?;
    if !status.success() {
        return Err(format!("The bundled computer helper exited with {status}.").into());
    }
    Ok(())
}

const DISCOVERY_HELPER: &str = "WonderComputerUseDiscovery";
const DISCOVERY_MISSING: &str =
    "Cupola.app is missing its screen list helper, so windows and screens cannot be listed. Reinstall Cupola.";

/// The helper executable for these arguments. macOS's screen capture service
/// (replayd) tells clients apart by executable path, so the long-lived source
/// discovery worker runs from its own copy: sharing the computer view's path,
/// starting a view's stream interrupted both (ScreenCaptureKit -3805). The copy
/// carries the same signing identity, so the Screen Recording grant covers it.
/// A bundle without the copy is damaged: discovery is refused rather than run
/// from the capture path. The dialog watcher, `--raise-dialog`, `--automation`
/// and `--permissions` use the capture path; they must stay off ScreenCaptureKit.
fn helper_binary(args: &[std::ffi::OsString]) -> Result<std::path::PathBuf> {
    helper_binary_in(&bundle()?.join("Contents/Helpers"), args)
}

fn helper_binary_in(helpers: &Path, args: &[std::ffi::OsString]) -> Result<PathBuf> {
    if args.first().is_some_and(|arg| arg == "--discover-sources") {
        let discovery = helpers.join(DISCOVERY_HELPER);
        if !discovery.is_file() {
            return Err(DISCOVERY_MISSING.into());
        }
        return Ok(discovery);
    }
    Ok(helpers.join("WonderComputerUse"))
}

fn run_helper(args: &[std::ffi::OsString]) -> Result<()> {
    wait_for_helper(Command::new(helper_binary(args)?).args(args), None)
}

fn helper_session(args: Vec<std::ffi::OsString>) -> Result<()> {
    if args.is_empty() {
        return Err("missing private helper connection".into());
    }
    let (stream, lifetime) = connect_to_proxy(Path::new(&args[0]), &executable_path()?)?;
    let output = stream.try_clone()?;
    wait_for_helper(
        Command::new(helper_binary(&args[1..])?)
            .args(&args[1..])
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(stream)))
            .stdout(Stdio::from(std::os::fd::OwnedFd::from(output)))
            .stderr(Stdio::null()),
        Some(lifetime),
    )
}

/// The session's side of the private connection: the secret comes from the
/// owner-only file next to the socket (never from the arguments `open` passes),
/// and it is handed over only to a proxy that is the Cupola host itself.
fn connect_to_proxy(socket: &Path, host: &Path) -> Result<(UnixStream, UnixStream)> {
    let directory = socket.parent().ok_or("missing private helper connection")?;
    let token = wonderd::computer_sessions::helper_channel::read_token_file(
        &directory.join(wonderd::computer_sessions::helper_channel::TOKEN_FILE),
    )?;
    let connect = || -> Result<UnixStream> {
        let mut stream = UnixStream::connect(socket)?;
        wonderd::computer_sessions::helper_channel::require_peer_executable(&stream, host)?;
        stream.write_all(token.as_bytes())?;
        Ok(stream)
    };
    Ok((connect()?, connect()?))
}

const PROXY_DIRECTORY: &str = "wonder-cu-";
/// How long a proxy waits for its LaunchServices session to connect. Its
/// socket directory exists only during this window.
const PROXY_CONNECT: Duration = Duration::from_secs(15);

fn proxy(app: &Path, args: Vec<std::ffi::OsString>) -> Result<()> {
    let (mut stream, lifetime) = connect_session(args, &executable_path()?, |launch| {
        // `open` hands its environment to the session: only the helper's share.
        checked(open_command(app, launch).env_clear().envs(
            wonderd::computer_sessions::helper_environment(std::env::vars_os()),
        ))?;
        Ok(())
    })?;
    let mut input = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut input);
        let _ = input.shutdown(std::net::Shutdown::Write);
    });
    let result = std::io::copy(&mut stream, &mut std::io::stdout().lock());
    drop(lifetime);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    result?;
    Ok(())
}

/// Starts the LaunchServices session with `open` and returns its standard
/// I/O and lifetime connections. Each is authenticated twice: the process on
/// the other end must be the `host` executable, and it must present the
/// one-time token. The token sits in a 0600 file in the private directory; the
/// launch arguments carry only the socket path.
fn connect_session(
    args: Vec<std::ffi::OsString>,
    host: &Path,
    open: impl FnOnce(&[std::ffi::OsString]) -> Result<()>,
) -> Result<(UnixStream, UnixStream)> {
    // /tmp keeps sockaddr_un below Darwin's 104-byte limit even in worktrees.
    // The directory is private (0700) and no public listener exists.
    let directory = tempfile::Builder::new()
        .prefix(PROXY_DIRECTORY)
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")?;
    let socket = directory.path().join("pipe");
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    wonderd::computer_sessions::helper_channel::write_token_file(directory.path(), &token)?;
    let mut launch = vec![SESSION.into(), socket.into_os_string()];
    launch.extend(args);
    open(&launch)?;
    let deadline = Instant::now() + PROXY_CONNECT;
    let stream = accept_helper(&listener, &token, host, deadline)?;
    let lifetime = accept_helper(&listener, &token, host, deadline)?;
    drop(listener);
    // Both connections are authenticated, so the path has no further use.
    // Removing it now means a later SIGKILL (the daemon's timeout, a stopped
    // dialog watcher) cannot leave the directory behind.
    let _ = directory.close();
    Ok((stream, lifetime))
}

/// Removes socket directories that proxies killed before they could clean
/// up left in `root` (earlier releases kept them until the proxy exited).
/// Only this user's real directories, not symlinks, holding nothing but the
/// proxy's socket, and older than `before`. A proxy uses the path only while
/// its session connects (`PROXY_CONNECT`), so a directory that old is never
/// one a running proxy still needs; its open connections do not use the path.
fn sweep_proxy_directories(root: &Path, before: std::time::SystemTime) -> usize {
    let uid = unsafe { libc::getuid() };
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(PROXY_DIRECTORY))
            && std::fs::symlink_metadata(&path).is_ok_and(|metadata| {
                metadata.file_type().is_dir()
                    && metadata.uid() == uid
                    && metadata.modified().is_ok_and(|at| at < before)
            });
        if !stale {
            continue;
        }
        let Ok(contents) = std::fs::read_dir(&path) else {
            continue;
        };
        let contents: Vec<_> = contents.flatten().take(2).collect();
        let only_socket = match contents.as_slice() {
            [] => true,
            [pipe] => {
                use std::os::unix::fs::FileTypeExt;
                pipe.file_name() == "pipe"
                    && std::fs::symlink_metadata(pipe.path()).is_ok_and(|metadata| {
                        metadata.file_type().is_socket() && metadata.uid() == uid
                    })
            }
            _ => false,
        };
        if !only_socket {
            continue;
        }
        if contents
            .first()
            .is_none_or(|pipe| std::fs::remove_file(pipe.path()).is_ok())
            && std::fs::remove_dir(&path).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

fn accept_helper(
    listener: &UnixListener,
    token: &str,
    host: &Path,
    deadline: Instant,
) -> Result<UnixStream> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Before reading anything a stranger could have sent. Another
                // program of this user that finds the socket is dropped, and
                // the session keeps waiting for the host's own connection.
                if wonderd::computer_sessions::helper_channel::require_peer_executable(
                    &stream, host,
                )
                .is_ok()
                {
                    return authenticate_helper(stream, token);
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => {
                return Err(format!("LaunchServices helper did not connect: {error}").into())
            }
        }
    }
}

fn authenticate_helper(mut stream: UnixStream, token: &str) -> Result<UnixStream> {
    let mut received = [0; 32];
    match os::read_exact_within(&mut stream, &mut received, Duration::from_secs(2)) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err("helper disconnected before authentication".into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return Err("helper authentication timed out".into())
        }
        Err(error) => return Err(error.into()),
    }
    if received != token.as_bytes() {
        return Err("helper connection authentication failed".into());
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

// LaunchServices drops the invoking shell: persist the explicit selection.
fn launch_environment(paths: &Paths) -> Value {
    Value::Object(super::service_environment::collect(
        std::env::vars(),
        &service_data_dir(paths),
    ))
}

pub(super) fn private_environment(path: &Path) -> Result<serde_json::Map<String, Value>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 64 * 1024
    {
        return Err("unsafe host launch configuration".into());
    }
    let value: Value = serde_json::from_reader(file)?;
    let env = value
        .as_object()
        .ok_or("invalid host launch configuration")?;
    if !super::service_environment::valid(env) {
        return Err("invalid host launch environment".into());
    }
    Ok(env.clone())
}

/// launchd points stdout and stderr at the host log, so a failure is logged
/// stamped here rather than printed bare by `main`.
fn serve(args: Vec<std::ffi::OsString>) -> Result<()> {
    if let Err(error) = serve_host(args) {
        host_log(format_args!("cupola: {error}"));
        std::process::exit(1);
    }
    Ok(())
}

fn serve_host(args: Vec<std::ffi::OsString>) -> Result<()> {
    let path = PathBuf::from(args.first().ok_or("missing host launch configuration")?);
    for (key, value) in private_environment(&path)? {
        std::env::set_var(key, value.as_str().unwrap());
    }
    let paths = Paths::resolve_for_start()?;
    if let Err(error) = follow_removed_app(&paths) {
        host_log(format_args!(
            "Could not point the login service at this app: {error}"
        ));
    }
    std::fs::create_dir_all(paths.logs())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path(&paths))?;
    unsafe {
        if libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO) < 0
            || libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    // Loading the login service runs it once, also while a host started
    // with `cupola start` is running; that run has nothing to do.
    if let Some(pid) = running_supervisor(&paths) {
        host_log(format_args!(
            "Cupola host already running (pid {pid}); this launch exits"
        ));
        return Ok(());
    }
    let swept = sweep_proxy_directories(
        Path::new("/tmp"),
        std::time::SystemTime::now() - PROXY_CONNECT * 40,
    );
    if swept > 0 {
        host_log(format_args!(
            "Removed {swept} leftover computer helper folders"
        ));
    }
    supervise(&paths, &load_config(&paths)?)
}

pub(super) fn start(paths: &Paths, as_json: bool) -> Result<()> {
    let env_path = paths.state.join("launch-environment.json");
    write_private(&env_path, &serde_json::to_vec(&launch_environment(paths))?)?;
    open_app(&bundle()?, &[SERVE.into(), env_path.into_os_string()])?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        if let Some(pid) = running_supervisor(paths) {
            break pid;
        }
        if Instant::now() >= deadline {
            return Err("Cupola did not start; inspect `cupola logs`.".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if as_json {
        print_json(&StartOutput {
            state: "starting",
            supervisor_pid: pid as u32,
            log_path: log_path(paths).display().to_string(),
        });
    } else {
        println!(
            "Starting Cupola host (pid {pid}). Logs: {}",
            log_path(paths).display()
        );
    }
    Ok(())
}

fn permission_output(screen: bool, input: bool) -> PermissionsOutput {
    PermissionsOutput {
        permissions: [
            ("screenRecording", "Screen Recording", screen),
            ("accessibility", "Accessibility", input),
        ]
        .into_iter()
        .map(|(id, name, granted)| Permission {
            id,
            state: if granted {
                PermissionState::Granted
            } else {
                PermissionState::Denied
            },
            summary: format!(
                "{name}: {}",
                if granted { "granted" } else { "not granted" }
            ),
            fix: (!granted).then(|| {
                "Needs the person: run `cupola permissions grant`, then enable Cupola in System Settings > Privacy & Security.".into()
            }),
        })
        .collect(),
    }
}

pub(super) fn permission_status() -> Result<PermissionsOutput> {
    // This command contains no request flags. Both native APIs are preflight only.
    let output = checked(Command::new(helper_launcher()?).arg("--permissions"))?;
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let screen = value["screenRecording"]
        .as_bool()
        .ok_or("helper omitted Screen Recording status")?;
    let input = value["accessibility"]
        .as_bool()
        .ok_or("helper omitted Accessibility status")?;
    Ok(permission_output(screen, input))
}

/// Shows the helper's own permission prompts and opens both privacy panes.
pub(super) fn grant_permissions() -> Result<()> {
    open_app(&bundle()?, &[GRANT.into()])?;
    for pane in ["Privacy_ScreenCapture", "Privacy_Accessibility"] {
        checked(Command::new("/usr/bin/open").arg(format!(
            "x-apple.systempreferences:com.apple.preference.security?{pane}"
        )))?;
    }
    Ok(())
}

pub(super) fn doctor_checks() -> Vec<Check> {
    let mut checks = permission_checks();
    if bundle().is_ok_and(|app| {
        !app.join("Contents/Helpers")
            .join(DISCOVERY_HELPER)
            .is_file()
    }) {
        checks.push(Check {
            id: "computer-discovery",
            status: CheckStatus::Failed,
            summary: DISCOVERY_MISSING.into(),
            fix: Some("Reinstall the signed Cupola.app.".into()),
        });
    }
    checks
}

fn permission_checks() -> Vec<Check> {
    match permission_status() {
        Ok(output) => output
            .permissions
            .into_iter()
            .map(|p| Check {
                id: p.id,
                status: if p.state == PermissionState::Granted {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Failed
                },
                summary: p.summary,
                fix: p.fix,
            })
            .collect(),
        Err(error) => vec![Check {
            id: "computer",
            status: CheckStatus::Failed,
            summary: error.to_string(),
            fix: Some("Reinstall the signed Cupola.app.".into()),
        }],
    }
}

fn service_path(home: &Path, service_id: &str) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{service_id}.plist"))
}

fn service_target(service_id: &str) -> String {
    format!("gui/{}/{}", unsafe { libc::getuid() }, service_id)
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn service_plist(app: &Path, env: &Path, log: &Path, service_id: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>{service_id}</string><key>ProgramArguments</key><array><string>/usr/bin/open</string><string>-g</string><string>-n</string><string>-a</string><string>{}</string><string>--args</string><string>{SERVE}</string><string>{}</string></array><key>RunAtLoad</key><true/><key>LimitLoadToSessionType</key><string>Aqua</string><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string></dict></plist>", xml(&app.to_string_lossy()), xml(&env.to_string_lossy()), xml(&log.to_string_lossy()), xml(&log.to_string_lossy()))
}

/// The LaunchAgent text with the files it names under `old` moved to `new`,
/// or `None` when it names none of them.
fn relocate_plist(text: &str, old: &Paths, new: &Paths) -> Option<String> {
    let env = |paths: &Paths| {
        xml(&paths
            .state
            .join("launch-environment.json")
            .to_string_lossy())
    };
    let log = |paths: &Paths| xml(&log_path(paths).to_string_lossy());
    text.contains(&env(old)).then(|| {
        text.replace(&env(old), &env(new))
            .replace(&log(old), &log(new))
    })
}

/// See `ServiceJob::follow_removed_app`.
fn follow_removed_app(paths: &Paths) -> Result<()> {
    let Ok(app) = bundle() else {
        return Ok(());
    };
    let service_id = format!("{}.cli", bundle_identity(&app)?);
    ServiceJob::new(&app, Path::new(&std::env::var("HOME")?), paths, &service_id)
        .follow_removed_app()
}

pub(super) fn relocate_service(legacy: &Path, target: &Path) -> Result<()> {
    // A development build outside the app installs no LaunchAgent.
    let Ok(app) = bundle() else {
        return Ok(());
    };
    let service_id = format!("{}.cli", bundle_identity(&app)?);
    relocate_service_file(
        &service_path(Path::new(&std::env::var("HOME")?), &service_id),
        legacy,
        target,
    )
}

fn relocate_service_file(plist: &Path, legacy: &Path, target: &Path) -> Result<()> {
    let Ok(text) = std::fs::read_to_string(plist) else {
        return Ok(());
    };
    let (old, new) = (
        Paths::at(Some(legacy.into()))?,
        Paths::at(Some(target.into()))?,
    );
    if let Some(text) = relocate_plist(&text, &old, &new) {
        write_private(plist, text.as_bytes())?;
    }
    Ok(())
}

struct LaunchctlOutput {
    code: Option<i32>,
    stderr: String,
}

type Launchctl<'a> = dyn FnMut(&[&std::ffi::OsStr]) -> Result<LaunchctlOutput> + 'a;

/// How long `service install` and `restart` wait for the host, as `start` does.
const SERVICE_START: Duration = Duration::from_millis(if cfg!(test) { 600 } else { 15_000 });
/// When the job ran but no host appeared yet, ask launchd to run it again.
const SERVICE_KICK: Duration = Duration::from_millis(if cfg!(test) { 200 } else { 3_000 });
/// How long a host that came up must stay up before install and restart
/// report it running.
const SERVICE_SETTLE: Duration = Duration::from_millis(if cfg!(test) { 50 } else { 500 });
const LAUNCHCTL_RETRY: Duration = Duration::from_millis(if cfg!(test) { 10 } else { 300 });
/// launchctl's codes for "Input/output error" and "Operation already in
/// progress": bootstrap right after a bootout, before launchd finished it.
const LAUNCHCTL_BUSY: [i32; 2] = [5, 37];

fn launchctl_ok(launchctl: &mut Launchctl, args: &[&std::ffi::OsStr]) -> Result<()> {
    let output = launchctl(args)?;
    if output.code == Some(0) {
        return Ok(());
    }
    Err(format!(
        "launchctl {} failed: {}",
        args[0].to_string_lossy(),
        output.stderr
    )
    .into())
}

fn job_loaded(launchctl: &mut Launchctl, target: &str) -> Result<bool> {
    Ok(launchctl(&["print".as_ref(), target.as_ref()])?.code == Some(0))
}

/// Unloads the job and waits until launchd has, since bootout returns before
/// it finishes.
fn bootout(launchctl: &mut Launchctl, target: &str) -> Result<()> {
    if !job_loaded(launchctl, target)? {
        return Ok(());
    }
    launchctl_ok(launchctl, &["bootout".as_ref(), target.as_ref()])?;
    let deadline = Instant::now() + SERVICE_START;
    while job_loaded(launchctl, target)? {
        if Instant::now() >= deadline {
            return Err("launchd did not unload the Cupola login service".into());
        }
        std::thread::sleep(LAUNCHCTL_RETRY);
    }
    Ok(())
}

/// Loads the job so launchd runs it at every login. Loading also runs it
/// once; a host that is already running makes that run exit at once. Returns
/// whether the job was loaded only now.
fn register_job(launchctl: &mut Launchctl, target: &str, plist: &Path) -> Result<bool> {
    if job_loaded(launchctl, target)? {
        return Ok(false);
    }
    let domain = target.rsplit_once('/').map_or(target, |(domain, _)| domain);
    let deadline = Instant::now() + SERVICE_START;
    loop {
        let output = launchctl(&["bootstrap".as_ref(), domain.as_ref(), plist.as_os_str()])?;
        match output.code {
            Some(0) => return Ok(true),
            Some(code) if LAUNCHCTL_BUSY.contains(&code) && Instant::now() < deadline => {
                std::thread::sleep(LAUNCHCTL_RETRY)
            }
            _ => return Err(format!("launchctl bootstrap failed: {}", output.stderr).into()),
        }
    }
}

/// Loads the job (which runs it once) or, when it is already loaded, runs it
/// again, then waits for the host. The job only opens Cupola.app, so a
/// loaded job is never the host itself: `host_up` is the answer.
fn launch_host(
    launchctl: &mut Launchctl,
    target: &str,
    plist: &Path,
    host_up: &dyn Fn() -> bool,
) -> Result<()> {
    let kickstart = |launchctl: &mut Launchctl| {
        launchctl_ok(launchctl, &["kickstart".as_ref(), target.as_ref()])
    };
    if !register_job(launchctl, target, plist)? {
        kickstart(launchctl)?;
    }
    let started = Instant::now();
    let mut kicked = false;
    while !host_up() {
        if started.elapsed() >= SERVICE_START {
            return Err(format!(
                "the host did not start within {} seconds",
                SERVICE_START.as_secs()
            )
            .into());
        }
        if !kicked && started.elapsed() >= SERVICE_KICK {
            kicked = true;
            kickstart(launchctl)?;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// The host is up once its supervisor holds the lock and has started the
/// daemon. The supervisor rewrites its origin status with one line when it
/// starts and appends the next only after the daemon runs, so a host that
/// fails during startup never gets that far.
fn host_up(paths: &Paths) -> impl Fn() -> bool + '_ {
    move || {
        running_supervisor(paths).is_some()
            && std::fs::read_to_string(paths.origin_status()).is_ok_and(|status| {
                // A previous run's file ends with its "stopped" line.
                let last = status
                    .lines()
                    .last()
                    .and_then(|line| serde_json::from_str::<Value>(line).ok());
                status.lines().count() >= 2 && last.is_some_and(|line| line["state"] != "stopped")
            })
    }
}

/// Says why the service did not leave the host running, with the host's
/// last log line when it wrote one.
fn not_started(paths: &Paths, error: Box<dyn std::error::Error>) -> Box<dyn std::error::Error> {
    use std::io::{Seek, SeekFrom};
    // Only the end of the log matters; it can be large.
    let last = std::fs::File::open(log_path(paths))
        .ok()
        .and_then(|mut log| {
            let length = log.seek(SeekFrom::End(0)).ok()?;
            log.seek(SeekFrom::Start(length.saturating_sub(8 * 1024)))
                .ok()?;
            let mut tail = Vec::new();
            log.read_to_end(&mut tail).ok()?;
            String::from_utf8_lossy(&tail)
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(str::to_owned)
        });
    let mut message = format!("The Cupola login service is installed, but {error}.");
    if let Some(last) = last {
        message.push_str(&format!(" Last host log line: {last}"));
    }
    message.push_str(" Run `cupola logs` for more.");
    message.into()
}

pub(super) fn service_state() -> Result<ServiceOutput> {
    let paths = Paths::resolve()?;
    let service_id = format!("{}.cli", bundle_identity(&bundle()?)?);
    Ok(ServiceOutput {
        manager: "launchd",
        installed: service_path(Path::new(&std::env::var("HOME")?), &service_id).is_file(),
        running: running_supervisor(&paths).is_some(),
        starts_at_boot: None,
    })
}

pub(super) fn service(command: ServiceCommand, as_json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    let app = bundle()?;
    let service_id = format!("{}.cli", bundle_identity(&app)?);
    let job = ServiceJob::new(
        &app,
        Path::new(&std::env::var("HOME")?),
        &paths,
        &service_id,
    );
    let mut launchctl = |args: &[&std::ffi::OsStr]| {
        let output = process::output(Command::new("/bin/launchctl").args(args))?;
        Ok(LaunchctlOutput {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    };
    run_service(command, &paths, &job, &mut launchctl)?;
    let output = service_state()?;
    if as_json {
        print_json(&output);
    } else {
        println!(
            "Login service: {}. Host: {}.",
            if output.installed {
                "installed"
            } else {
                "not installed"
            },
            if output.running { "running" } else { "stopped" }
        );
    }
    Ok(())
}

/// The LaunchAgent as `service` manages it.
struct ServiceJob {
    plist: PathBuf,
    target: String,
    /// The app the job opens.
    app: PathBuf,
    /// The plist this installation writes; another one is left alone.
    contents: String,
}

/// Where a LaunchAgent from `service_plist` names the app it opens.
const PLIST_APP: (&str, &str) = ("<string>-a</string><string>", "</string>");

impl ServiceJob {
    fn new(app: &Path, home: &Path, paths: &Paths, service_id: &str) -> Self {
        Self {
            plist: service_path(home, service_id),
            app: app.into(),
            target: service_target(service_id),
            contents: service_plist(
                app,
                &paths.state.join("launch-environment.json"),
                &log_path(paths),
                service_id,
            ),
        }
    }

    /// The app a job opens when it is this job but for that app: the same
    /// identity and data directory, written by an app since renamed or moved
    /// (earlier releases installed `Cupola CLI.app`, now `Cupola.app`).
    fn moved_from(&self, text: &str) -> Option<PathBuf> {
        let start = text.find(PLIST_APP.0)? + PLIST_APP.0.len();
        let old = &text[start..start + text[start..].find(PLIST_APP.1)?];
        let here = xml(&self.app.to_string_lossy());
        let rewritten = format!("{}{here}{}", &text[..start], &text[start + old.len()..]);
        (old != here && rewritten == self.contents).then(|| {
            old.replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&apos;", "'")
                .replace("&amp;", "&")
                .into()
        })
    }

    /// Checks the installed job is this installation's, first pointing a
    /// job from a renamed or moved app at this one. Returns whether it did,
    /// so launchd, which keeps the job it loaded, can be told.
    fn claim(&self) -> Result<bool> {
        let text = std::fs::read_to_string(&self.plist)?;
        if text == self.contents {
            return Ok(false);
        }
        if self.moved_from(&text).is_none() {
            return Err(
                "LaunchAgent belongs to another Cupola installation; no changes made.".into(),
            );
        }
        write_private(&self.plist, self.contents.as_bytes())?;
        Ok(true)
    }

    /// At host start: a job naming an app that no longer exists would open
    /// nothing at the next login, so it is pointed here. One naming another
    /// copy that still exists is left to `service install` or `restart`.
    fn follow_removed_app(&self) -> Result<()> {
        let Ok(text) = std::fs::read_to_string(&self.plist) else {
            return Ok(());
        };
        if self.moved_from(&text).is_some_and(|old| !old.exists()) {
            write_private(&self.plist, self.contents.as_bytes())?;
        }
        Ok(())
    }
}

fn run_service(
    command: ServiceCommand,
    paths: &Paths,
    job: &ServiceJob,
    launchctl: &mut Launchctl,
) -> Result<()> {
    match command {
        ServiceCommand::Status => return Ok(()),
        ServiceCommand::Install => {
            load_config(paths)?;
            std::fs::create_dir_all(job.plist.parent().unwrap())?;
            std::fs::create_dir_all(paths.logs())?;
            write_private(
                &paths.state.join("launch-environment.json"),
                &serde_json::to_vec(&launch_environment(paths))?,
            )?;
            if job.plist.exists() {
                if job
                    .claim()
                    .map_err(|_| "A different Cupola LaunchAgent exists; uninstall it first.")?
                {
                    // Load the rewritten job instead of the old app's.
                    bootout(launchctl, &job.target)?;
                }
            } else {
                write_private(&job.plist, job.contents.as_bytes())?;
            }
            // A host started with `cupola start` keeps running; the job is
            // still loaded so it starts the host at the next login.
            if running_supervisor(paths).is_some() {
                register_job(launchctl, &job.target, &job.plist)
                    .map_err(|error| not_started(paths, error))?;
            } else {
                launch_host(launchctl, &job.target, &job.plist, &host_up(paths))
                    .map_err(|error| not_started(paths, error))?;
            }
        }
        ServiceCommand::Uninstall => {
            if job.plist.exists() {
                // Do not remove another service just because it shares our label.
                job.claim()?;
                if running_supervisor(paths).is_some() {
                    stop_host(paths)?;
                }
                bootout(launchctl, &job.target)?;
                std::fs::remove_file(&job.plist)?;
            }
            return Ok(());
        }
        ServiceCommand::Restart => {
            if !job.plist.is_file() {
                return Err("Run `cupola service install` first.".into());
            }
            job.claim()?;
            if running_supervisor(paths).is_some() {
                stop_host(paths)?;
            }
            // Reload so launchd runs the job as it is on disk now.
            bootout(launchctl, &job.target)?;
            launch_host(launchctl, &job.target, &job.plist, &host_up(paths))
                .map_err(|error| not_started(paths, error))?;
        }
    }
    // A host that exits during its first moments is not running.
    std::thread::sleep(SERVICE_SETTLE);
    if running_supervisor(paths).is_none() {
        return Err(not_started(
            paths,
            "the host stopped right after it started".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Requirements {
    app: String,
    helper: String,
}

fn designated_requirement(path: &Path) -> Result<String> {
    let output = checked(
        Command::new("/usr/bin/codesign")
            .args(["-d", "-r-"])
            .arg(path),
    )?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    text.lines()
        .find_map(|line| line.strip_prefix("designated => "))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or("missing designated requirement".into())
}

fn requirements(app: &Path) -> Result<Requirements> {
    Ok(Requirements {
        app: designated_requirement(app)?,
        helper: designated_requirement(&app.join("Contents/Helpers/WonderComputerUse"))?,
    })
}

fn reset_services(before: &Requirements, after: &Requirements) -> &'static [&'static str] {
    if before == after {
        &[]
    } else {
        &["ScreenCapture", "Accessibility", "AppleEvents"]
    }
}

fn plist_value(app: &Path, key: &str) -> Result<String> {
    let output = checked(
        Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", &format!("Print :{key}")])
            .arg(app.join("Contents/Info.plist")),
    )?;
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

fn bundle_identity(app: &Path) -> Result<String> {
    let id = plist_value(app, "CFBundleIdentifier")?;
    if !matches!(id.as_str(), BUNDLE_ID | STAGING_BUNDLE_ID) {
        return Err("Unsupported Cupola CLI bundle identity.".into());
    }
    Ok(id)
}

fn verify_update(app: &Path, expected_id: &str) -> Result<()> {
    if bundle_identity(app)? != expected_id
        || plist_value(app, "CFBundleExecutable")? != "cupola"
        || plist_value(app, "LSUIElement")? != "true"
    {
        return Err("Update must be the windowless Cupola CLI bundle.".into());
    }
    checked(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app),
    )?;
    let info = checked(Command::new("/usr/bin/codesign").arg("-dvv").arg(app))?;
    let info = String::from_utf8_lossy(&info.stderr);
    if !info
        .lines()
        .any(|line| line == format!("TeamIdentifier={TEAM_ID}"))
        || !info.contains("Authority=Developer ID Application:")
    {
        return Err("Update is not signed by Cupola's existing Developer ID team.".into());
    }
    checked(
        Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(app),
    )?;
    checked(
        Command::new("/usr/bin/xcrun")
            .args(["stapler", "validate"])
            .arg(app),
    )?;
    Ok(())
}

/// Keep the signed bundle intact; only its location changes. The old host is
/// paused through the same durable handoff as Linux before any files switch.
fn swap_app(app: &Path, replacement: &Path, previous: &Path) -> Result<()> {
    std::fs::rename(app, previous)?;
    if let Err(error) = std::fs::rename(replacement, app) {
        if let Err(restore) = std::fs::rename(previous, app) {
            return Err(format!("Replacement failed ({error}); recovery failed ({restore}). Previous app is retained at {}.", previous.display()).into());
        }
        return Err(error.into());
    }
    Ok(())
}

pub(super) async fn update(
    check: bool,
    channel: Channel,
    version: Option<String>,
    yes: bool,
    as_json: bool,
) -> Result<()> {
    ensure_self_update_installation()?;
    let app = bundle()?;
    let bundle_id = bundle_identity(&app)?;
    let current = plist_value(&app, "CFBundleVersion")?;
    let parent = app.parent().ok_or("Missing installation directory.")?;
    let _lock = lock_updates(&parent.join(format!(".{bundle_id}-update")))?;
    let stage = tempfile::Builder::new()
        .prefix(".wonder-update-")
        .tempdir_in(parent)?;
    let staged = std::env::var_os("WONDER_UPDATE_BUNDLE").map(PathBuf::from);
    let feed = if staged.is_none() {
        Some(platform_release(channel, version.as_deref()).await?)
    } else {
        None
    };
    let latest = match &feed {
        Some(feed) => feed.version.clone(),
        None => {
            let candidate = staged.as_ref().unwrap();
            verify_update(candidate, &bundle_id)?;
            let latest = plist_value(candidate, "CFBundleVersion")?;
            if version.as_ref().is_some_and(|v| v != &latest) {
                return Err("Staged update does not match --version.".into());
            }
            latest
        }
    };
    let available = if version.is_some() {
        latest != current
    } else {
        newer_version(&latest, &current)?
    };
    let mut output = UpdateOutput {
        channel,
        current,
        latest: Some(latest.clone()),
        update_available: available,
        installed: false,
        restarted: false,
        agents_ready: None,
    };
    if !available || check {
        report_platform_update(&output, as_json);
        return Ok(());
    }
    let candidate = match feed {
        Some(feed) => {
            let archive = stage.path().join("download.zip");
            feed.download(&archive).await?;
            let payload = stage.path().join("payload");
            unpack_platform_archive(&archive, &payload)?;
            payload.join("Cupola.app")
        }
        None => wonderd::canonical_path::canonicalize(staged.unwrap())?,
    };
    if candidate == app || candidate.starts_with(&app) {
        return Err("Update must be staged outside the installed app.".into());
    }
    verify_update(&candidate, &bundle_id)?;
    if plist_value(&candidate, "CFBundleVersion")? != latest {
        return Err("Downloaded app version does not match its manifest.".into());
    }
    verify_cli_version(&candidate.join("Contents/MacOS/cupola"), &latest)?;
    let before = requirements(&app)?;
    let after = requirements(&candidate)?;
    let replacement = stage.path().join("Replacement.app");
    checked(
        Command::new("/usr/bin/ditto")
            .arg(&candidate)
            .arg(&replacement),
    )?;
    verify_update(&replacement, &bundle_id)?;
    if requirements(&replacement)? != after {
        return Err("Signature changed while copying the release.".into());
    }
    let paths = Paths::resolve()?;
    secure_dirs(&paths)?;
    let plan = update_plan(&paths, yes, as_json).await?;
    if matches!(plan, UpdatePlan::Deferred) {
        report_platform_update(&output, as_json);
        return Ok(());
    }
    let running = matches!(plan, UpdatePlan::Restart { .. });
    // Never put the only good app under an auto-deleting TempDir. Even a
    // filesystem error during rollback must leave it available for recovery.
    let previous = parent.join(format!(
        ".{bundle_id}-rollback-{}.app",
        uuid::Uuid::new_v4().simple()
    ));
    let swapped = (|| -> Result<()> {
        if running {
            stop_host(&paths)?;
        }
        let _host_lock = supervisor_lock(&paths)?;
        swap_app(&app, &replacement, &previous)?;
        for service in reset_services(&before, &after) {
            checked(Command::new("/usr/bin/tccutil").args(["reset", service, &bundle_id]))?;
        }
        Ok(())
    })();
    let exe = app.join("Contents/MacOS/cupola");
    let applied = match swapped {
        Ok(()) if running => match start_updated(&exe) {
            Ok(()) => wait_updated_ready(&paths, &exe).await.map(Some),
            Err(error) => Err(error),
        },
        other => other.map(|()| None),
    };
    let agents_ready = match applied {
        Ok(ready) => ready,
        Err(error) => {
            let _ = stop_host(&paths);
            if previous.exists() {
                let restored = if app.exists() {
                    swap_app(&app, &previous, &stage.path().join("Failed.app"))
                } else {
                    std::fs::rename(&previous, &app).map_err(Into::into)
                };
                restored.map_err(|restore| format!("Update failed ({error}); recovery failed ({restore}). Previous signed app retained at {}.", previous.display()))?;
            }
            if running {
                let _ = start_updated(&app.join("Contents/MacOS/cupola"));
            }
            if let UpdatePlan::Restart { local, request } = plan {
                let _ = local.call("update.cancel", request).await;
            }
            return Err(format!("Update failed; previous app restored: {error}").into());
        }
    };
    let backup = parent.join(format!(".{bundle_id}-previous.app"));
    if backup.exists() {
        if bundle_identity(&backup)? != bundle_id {
            return Err("Update installed, but another app occupies the rollback location.".into());
        }
        std::fs::remove_dir_all(&backup)?;
    }
    std::fs::rename(previous, backup)?;
    output.installed = true;
    output.restarted = running;
    output.agents_ready = agents_ready;
    report_platform_update(&output, as_json);
    Ok(())
}

fn newer_version(candidate: &str, current: &str) -> Result<bool> {
    fn parts(version: &str) -> Result<Vec<u64>> {
        if version.is_empty() {
            return Err("empty bundle version".into());
        }
        version
            .split('.')
            .map(|part| {
                part.parse()
                    .map_err(|_| "bundle version must be numeric".into())
            })
            .collect()
    }
    let mut a = parts(candidate)?;
    let mut b = parts(current)?;
    let count = a.len().max(b.len());
    a.resize(count, 0);
    b.resize(count, 0);
    Ok(a > b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn bundle_swap_preserves_the_previous_app_when_replacement_rename_fails() {
        let directory = tempfile::tempdir().unwrap();
        let app = directory.path().join("Cupola.app");
        let previous = directory.path().join("Previous.app");
        let replacement = directory.path().join("New.app");
        std::fs::create_dir(&app).unwrap();
        std::fs::write(app.join("version"), b"old").unwrap();
        assert!(swap_app(&app, &replacement, &previous).is_err());
        assert_eq!(std::fs::read(app.join("version")).unwrap(), b"old");
        assert!(!previous.exists());
        std::fs::create_dir(&replacement).unwrap();
        std::fs::write(replacement.join("version"), b"new").unwrap();
        swap_app(&app, &replacement, &previous).unwrap();
        assert_eq!(std::fs::read(app.join("version")).unwrap(), b"new");
        assert_eq!(std::fs::read(previous.join("version")).unwrap(), b"old");
    }

    fn process_running(pid: i32) -> bool {
        let output = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&output.stdout);
        !state.trim().is_empty() && !state.trim().starts_with('Z')
    }

    struct HungHelper(tempfile::TempDir);

    impl HungHelper {
        fn pids(&self) -> Vec<i32> {
            std::fs::read_to_string(self.0.path().join("pids"))
                .unwrap_or_default()
                .split_whitespace()
                .map(|pid| pid.parse().unwrap())
                .collect()
        }
    }

    impl Drop for HungHelper {
        fn drop(&mut self) {
            // Also clean the real fixture processes if the regression fails.
            for pid in self.pids() {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }

    // Contract: daemon timeout/stop kills the LaunchServices proxy; its detached
    // session must kill the actual hung helper and its descendants as well.
    #[test]
    fn killing_proxy_stops_hung_helper_group() {
        let unrelated = OwnedChild::spawn(Command::new("/bin/sleep").arg("60")).unwrap();
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            let fixture = HungHelper(tempfile::tempdir().unwrap());
            let (lifetime, held_by_proxy) = UnixStream::pair().unwrap();
            let proxy = OwnedChild::spawn(
                Command::new("/bin/sleep")
                    .arg("60")
                    .stdin(Stdio::from(std::os::fd::OwnedFd::from(held_by_proxy))),
            )
            .unwrap();
            let pid_file = fixture.0.path().join("pids");
            let session = std::thread::spawn(move || {
                wait_for_helper(
                    Command::new("/bin/sh")
                        .args([
                            "-c",
                            "trap '' TERM; sleep 60 & printf '%s %s\\n' $$ $! > \"$1\"; wait",
                            "hung-helper",
                        ])
                        .arg(pid_file)
                        .process_group(0)
                        .stdin(Stdio::null()),
                    Some(lifetime),
                )
                .map_err(|error| error.to_string())
            });
            let deadline = Instant::now() + Duration::from_secs(60);
            while fixture.pids().len() != 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let pids = fixture.pids();
            assert_eq!(pids.len(), 2, "helper did not start");
            // stdin is already EOF, but the lifetime connection is still open.
            assert!(pids.iter().all(|pid| process_running(*pid)));
            assert!(!session.is_finished());
            let started = Instant::now();
            assert_eq!(unsafe { libc::kill(proxy.id() as i32, signal) }, 0);
            let deadline = started + Duration::from_secs(60);
            while !session.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                session.is_finished(),
                "hung helper survived proxy signal {signal}"
            );
            session.join().unwrap().unwrap();
            for pid in pids {
                assert!(!process_running(pid), "helper process {pid} survived");
            }
            std::fs::remove_file(fixture.0.path().join("pids")).unwrap();
            assert!(!unrelated.exited().unwrap());
            println!(
                "proxy signal {signal}: helper group stopped in {:?}",
                started.elapsed()
            );
        }
    }

    // Contract: a program that is not the Cupola host never gets a helper
    // connection, even with the right path: the session skips it and keeps
    // waiting, and times out rather than serving it.
    #[test]
    fn a_connection_from_another_program_is_not_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pipe");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.write_all(token.as_bytes()).unwrap();
        let result = accept_helper(
            &listener,
            token,
            Path::new("/bin/sh"),
            // Already expired: the queued stranger is judged, then the wait ends.
            Instant::now(),
        );
        assert!(
            result.is_err(),
            "a non-host peer with the token was accepted"
        );
    }

    // Contract: the session refuses a proxy that is not the Cupola host before
    // it sends the secret, so a squatter on the socket path never receives it.
    #[test]
    fn the_session_does_not_send_its_secret_to_another_program() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pipe");
        let listener = UnixListener::bind(&path).unwrap();
        wonderd::computer_sessions::helper_channel::write_token_file(
            directory.path(),
            "0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let result = connect_to_proxy(&path, Path::new("/bin/sh"));
        assert!(result.is_err(), "the session connected to a non-host proxy");
        let (mut squatter, _) = listener.accept().unwrap();
        squatter.set_nonblocking(true).unwrap();
        let mut received = [0; 1];
        let read = squatter.read(&mut received);
        assert!(
            !matches!(read, Ok(n) if n > 0),
            "the secret reached a non-host listener"
        );
    }

    #[test]
    fn helper_authentication_waits_for_token_on_a_nonblocking_listener() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pipe");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        for valid in [true, false] {
            let mut peer = UnixStream::connect(&path).unwrap();
            let (stream, _) = listener.accept().unwrap();
            let writer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                peer.write_all(if valid { token.as_bytes() } else { &[b'x'; 32] })
                    .unwrap();
            });
            let result = authenticate_helper(stream, token);
            assert_eq!(result.is_ok(), valid, "{result:?}");
            writer.join().unwrap();
        }
    }

    // Contract: the socket directory is private while the session connects
    // and gone once it has, so a proxy killed later leaves nothing in /tmp.
    // The session secret is a 0600 file there, never a launch argument.
    #[test]
    fn session_socket_directory_is_private_and_removed_once_connected() {
        let mut socket = None;
        let host = std::env::current_exe().unwrap();
        let (mut stream, _lifetime) =
            connect_session(vec!["--discover-sources".into()], &host, |launch| {
                assert_eq!(launch[0], SESSION);
                assert_eq!(
                    launch.len(),
                    3,
                    "secret or extra value in the launch: {launch:?}"
                );
                assert_eq!(launch[2], "--discover-sources");
                let path = PathBuf::from(&launch[1]);
                let directory = path.parent().unwrap();
                let mode = std::fs::metadata(directory).unwrap().mode();
                assert_eq!(mode & 0o777, 0o700);
                let token_file =
                    directory.join(wonderd::computer_sessions::helper_channel::TOKEN_FILE);
                assert_eq!(
                    std::fs::metadata(&token_file).unwrap().mode() & 0o777,
                    0o600
                );
                let token = std::fs::read_to_string(&token_file).unwrap();
                assert!(
                    launch
                        .iter()
                        .all(|arg| !arg.to_string_lossy().contains(&token)),
                    "the session secret is in the launch arguments: {launch:?}"
                );
                socket = Some(path.clone());
                let host = host.clone();
                std::thread::spawn(move || {
                    // What `helper_session` does from inside the LaunchServices launch.
                    let (mut stream, _lifetime) = connect_to_proxy(&path, &host).unwrap();
                    stream.write_all(b"ready\n").unwrap();
                    std::thread::sleep(Duration::from_millis(200));
                });
                Ok(())
            })
            .unwrap();
        let mut line = [0; 6];
        stream.read_exact(&mut line).unwrap();
        assert_eq!(&line, b"ready\n");
        let socket = socket.unwrap();
        assert!(
            !socket.parent().unwrap().exists(),
            "socket directory left behind"
        );
    }

    #[test]
    fn startup_sweep_removes_only_stale_private_proxy_directories() {
        let root = tempfile::tempdir().unwrap();
        let make = |name: &str| {
            let path = root.path().join(name);
            std::fs::create_dir(&path).unwrap();
            path
        };
        let stale = make("wonder-cu-stale");
        let _listener = UnixListener::bind(stale.join("pipe")).unwrap();
        let empty = make("wonder-cu-empty");
        let regular = make("wonder-cu-regular");
        std::fs::write(regular.join("pipe"), "not a socket").unwrap();
        let extra = make("wonder-cu-extra");
        let _extra = UnixListener::bind(extra.join("pipe")).unwrap();
        std::fs::write(extra.join("notes"), "kept").unwrap();
        let other = make("other-stale");
        let target = make("target");
        let _target = UnixListener::bind(target.join("pipe")).unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("wonder-cu-link")).unwrap();

        // Too recent: a proxy may still be waiting for its session.
        let now = std::time::SystemTime::now();
        assert_eq!(
            sweep_proxy_directories(root.path(), now - Duration::from_secs(3600)),
            0
        );
        assert!(stale.join("pipe").exists());

        let later = now + Duration::from_secs(3600);
        assert_eq!(sweep_proxy_directories(root.path(), later), 2);
        assert!(!stale.exists() && !empty.exists());
        for kept in [&regular, &extra, &other, &target] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
        assert!(target.join("pipe").exists(), "a symlink was followed");
        assert!(root.path().join("wonder-cu-link").exists());
    }

    // Contract: a bundle missing the discovery copy refuses discovery instead
    // of running it from the capture path, where it interrupts a live view.
    #[test]
    fn discovery_never_falls_back_to_the_capture_helper() {
        let helpers = tempfile::tempdir().unwrap();
        std::fs::write(helpers.path().join("WonderComputerUse"), "fixture").unwrap();
        let discover: [std::ffi::OsString; 1] = ["--discover-sources".into()];
        assert_eq!(
            helper_binary_in(helpers.path(), &discover)
                .unwrap_err()
                .to_string(),
            DISCOVERY_MISSING
        );
        assert_eq!(
            helper_binary_in(helpers.path(), &["--watch-dialogs".into()]).unwrap(),
            helpers.path().join("WonderComputerUse")
        );
        std::fs::write(helpers.path().join(DISCOVERY_HELPER), "fixture").unwrap();
        assert_eq!(
            helper_binary_in(helpers.path(), &discover).unwrap(),
            helpers.path().join(DISCOVERY_HELPER)
        );
    }

    #[test]
    fn bundle_resolution_follows_path_and_helper_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Cupola.app");
        let exe = app.join("Contents/MacOS/cupola");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(app.join("Contents/Helpers")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), "fixture").unwrap();
        std::fs::write(&exe, "fixture").unwrap();
        let cli = root.path().join("cupola");
        let helper = app.join("Contents/Helpers/wonder-computer-use");
        std::os::unix::fs::symlink(&exe, &cli).unwrap();
        std::os::unix::fs::symlink("../MacOS/cupola", &helper).unwrap();
        for invocation in [&exe, &cli, &helper] {
            assert_eq!(
                bundle_for_executable(invocation).unwrap(),
                wonderd::canonical_path::canonicalize(&app).unwrap()
            );
        }
    }

    // Contract: a staging update can never replace production (or vice versa).
    // Real plist reads own this boundary before signature/notarization checks.
    #[test]
    fn update_identity_is_fixed_and_cannot_cross_installations() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Cupola.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        for id in [BUNDLE_ID, STAGING_BUNDLE_ID, "com.example.foreign"] {
            std::fs::write(
                app.join("Contents/Info.plist"),
                format!("<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{id}</string><key>CFBundleExecutable</key><string>cupola</string></dict></plist>"),
            )
            .unwrap();
            assert_eq!(bundle_identity(&app).is_ok(), id != "com.example.foreign");
            let other = if id == BUNDLE_ID {
                STAGING_BUNDLE_ID
            } else {
                BUNDLE_ID
            };
            assert!(verify_update(&app, other).is_err());
        }
    }

    #[test]
    fn updates_preserve_grants_until_either_designated_requirement_changes() {
        let old = Requirements {
            app: "app-team".into(),
            helper: "helper-team".into(),
        };
        assert!(reset_services(&old, &old).is_empty());
        for new in [
            Requirements {
                app: "new-team".into(),
                helper: old.helper.clone(),
            },
            Requirements {
                app: old.app.clone(),
                helper: "new-helper".into(),
            },
        ] {
            assert_eq!(
                reset_services(&old, &new),
                ["ScreenCapture", "Accessibility", "AppleEvents"]
            );
        }
    }

    #[test]
    fn bundle_versions_are_numeric_and_never_downgrade() {
        assert!(newer_version("1.0.130", "1.0.99").unwrap());
        assert!(!newer_version("1.0.9", "1.0.99").unwrap());
        assert!(!newer_version("1.0", "1.0.0").unwrap());
        for invalid in ["", "1..2", "1.0-nightly", "1.-1"] {
            assert!(newer_version(invalid, "1").is_err());
        }
    }

    #[test]
    fn launch_configuration_refuses_shared_symlinked_and_unexpected_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env.json");
        write_private(
            &path,
            br#"{"PATH":"/usr/bin","WONDER_DATA_DIR":"/tmp/fixture"}"#,
        )
        .unwrap();
        assert_eq!(
            private_environment(&path).unwrap()["WONDER_DATA_DIR"],
            "/tmp/fixture"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(private_environment(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(private_environment(&link).is_err());
        write_private(&path, br#"{"DYLD_INSERT_LIBRARIES":"untrusted"}"#).unwrap();
        assert!(private_environment(&path).is_err());
    }

    #[test]
    fn a_moved_data_directory_moves_the_launch_agent_files() {
        let app = Path::new("/Applications/Cupola.app");
        let id = "com.saimun.wonder.cli";
        let old = Paths::at(Some("/Users/a&b/.wonder".into())).unwrap();
        let new = Paths::at(Some("/Users/a&b/.cupola".into())).unwrap();
        let installed = |paths: &Paths| {
            service_plist(
                app,
                &paths.state.join("launch-environment.json"),
                &log_path(paths),
                id,
            )
        };
        let moved = relocate_plist(&installed(&old), &old, &new).unwrap();
        // `service install` and `uninstall` recognize the moved file as theirs.
        assert_eq!(moved, installed(&new));
        assert_eq!(relocate_plist(&moved, &old, &new), None);
    }

    #[test]
    fn launch_agent_is_a_valid_plist_even_with_spaces_and_xml_characters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.plist");
        std::fs::write(
            &path,
            service_plist(
                Path::new("/tmp/A & B/Cupola.app"),
                Path::new("/tmp/O'Neil/env"),
                Path::new("/tmp/log"),
                "com.saimun.wonder.cli-staging.cli",
            ),
        )
        .unwrap();
        assert!(Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&path)
            .status()
            .unwrap()
            .success());
        let output = Command::new("/usr/bin/plutil")
            .args(["-extract", "ProgramArguments", "json", "-o", "-"])
            .arg(path)
            .output()
            .unwrap();
        let args: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(args[4], "/tmp/A & B/Cupola.app");
        assert_eq!(args[7], "/tmp/O'Neil/env");
    }

    #[test]
    fn permission_result_contains_only_preflight_values_and_recovery() {
        let value = serde_json::to_value(permission_output(true, false)).unwrap();
        assert_eq!(value["permissions"][0]["state"], "granted");
        assert_eq!(value["permissions"][0]["fix"], Value::Null);
        assert_eq!(value["permissions"][1]["state"], "denied");
    }

    /// launchd as `service` sees it: whether the job is loaded, how many
    /// bootstraps fail busy (as right after a bootout), how many polls an
    /// unload takes, and whether running the job brings the host up.
    struct FakeLaunchd {
        loaded: bool,
        busy_bootstraps: usize,
        unload_polls: usize,
        lingering: usize,
        runs_start_host: bool,
        calls: Vec<String>,
        host: std::rc::Rc<std::cell::Cell<bool>>,
        /// When set, running the job does what Cupola.app does with this
        /// data directory: start a host unless one already runs.
        data: Option<PathBuf>,
        hosts_started: usize,
    }

    impl FakeLaunchd {
        fn new(loaded: bool) -> Self {
            Self {
                loaded,
                busy_bootstraps: 0,
                unload_polls: 0,
                lingering: 0,
                runs_start_host: true,
                calls: Vec::new(),
                host: Default::default(),
                data: None,
                hosts_started: 0,
            }
        }

        fn run(&mut self, args: &[&std::ffi::OsStr]) -> Result<LaunchctlOutput> {
            let verb = args[0].to_string_lossy().into_owned();
            self.calls.push(verb.clone());
            let ok = |ok: bool, stderr: &str| LaunchctlOutput {
                code: Some(if ok { 0 } else { 113 }),
                stderr: stderr.into(),
            };
            Ok(match verb.as_str() {
                "print" if self.lingering > 0 => {
                    self.lingering -= 1;
                    ok(true, "")
                }
                "print" => ok(self.loaded, "Could not find service"),
                "bootout" => {
                    self.loaded = false;
                    self.lingering = self.unload_polls;
                    ok(true, "")
                }
                "bootstrap" if self.busy_bootstraps > 0 => {
                    self.busy_bootstraps -= 1;
                    LaunchctlOutput {
                        code: Some(5),
                        stderr: "Bootstrap failed: 5: Input/output error".into(),
                    }
                }
                "bootstrap" | "kickstart" => {
                    self.loaded = true;
                    if self.runs_start_host {
                        self.host.set(true);
                        if let Some(data) = &self.data {
                            if running_supervisor(&fixture_paths(data)).is_none() {
                                fake_host(data);
                                self.hosts_started += 1;
                            }
                        }
                    }
                    ok(true, "")
                }
                other => panic!("unexpected launchctl {other}"),
            })
        }
    }

    fn launch(fake: &mut FakeLaunchd) -> Result<()> {
        let host = fake.host.clone();
        let up = move || host.get();
        launch_host(
            &mut |args| fake.run(args),
            "gui/501/com.saimun.wonder.cli",
            Path::new("/tmp/fixture.plist"),
            &up,
        )
    }

    #[test]
    fn fresh_service_install_loads_the_job_and_waits_for_the_host() {
        let mut fake = FakeLaunchd::new(false);
        launch(&mut fake).unwrap();
        assert_eq!(fake.calls, ["print", "bootstrap"]);
    }

    #[test]
    fn loaded_job_without_a_host_is_run_again() {
        // A job loaded by an earlier install already ran once; install and
        // restart must not report the host as stopped and leave it there.
        let mut fake = FakeLaunchd::new(true);
        launch(&mut fake).unwrap();
        assert_eq!(fake.calls, ["print", "kickstart"]);
    }

    #[test]
    fn restart_waits_out_an_unfinished_bootout_and_a_busy_bootstrap() {
        let mut fake = FakeLaunchd::new(true);
        fake.unload_polls = 2;
        fake.busy_bootstraps = 2;
        let mut run = |args: &[&std::ffi::OsStr]| fake.run(args);
        bootout(&mut run, "gui/501/com.saimun.wonder.cli").unwrap();
        launch(&mut fake).unwrap();
        assert_eq!(
            fake.calls,
            [
                "print",
                "bootout",
                "print",
                "print",
                "print",
                "print",
                "bootstrap",
                "bootstrap",
                "bootstrap"
            ]
        );
    }

    #[test]
    fn a_host_that_never_comes_up_is_an_error_after_one_more_run() {
        let mut fake = FakeLaunchd::new(false);
        fake.runs_start_host = false;
        let error = launch(&mut fake).unwrap_err().to_string();
        assert!(error.contains("did not start"), "{error}");
        assert_eq!(fake.calls, ["print", "bootstrap", "kickstart"]);
    }

    #[test]
    fn a_failing_bootstrap_is_reported_with_launchd_words() {
        let mut fake = FakeLaunchd::new(false);
        fake.busy_bootstraps = usize::MAX;
        let error = launch(&mut fake).unwrap_err().to_string();
        assert!(error.contains("Input/output error"), "{error}");
    }

    fn fixture_paths(data: &Path) -> Paths {
        Paths {
            data: data.into(),
            state: data.join("headless"),
        }
    }

    /// A host as `service` observes it: the supervisor lock and pid, an
    /// origin status past startup, and a control socket that stops it.
    fn fake_host(data: &Path) {
        let paths = fixture_paths(data);
        let lock = supervisor_lock(&paths).unwrap();
        write_private(&paths.supervisor_pid(), b"4242").unwrap();
        write_private(
            &paths.origin_status(),
            b"{\"state\":\"starting\"}\n{\"state\":\"ready\"}\n",
        )
        .unwrap();
        let socket = paths.state.join("control.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut input = [0u8; 5];
                if stream.read_exact(&mut input).is_ok() && input == *b"stop\n" {
                    let _ = std::fs::remove_file(&socket);
                    let mut status = os::append_private(&paths.origin_status()).unwrap();
                    status.write_all(b"{\"state\":\"stopped\"}\n").unwrap();
                    drop(lock);
                    return;
                }
            }
        });
    }

    struct ServiceFixture {
        _directory: tempfile::TempDir,
        paths: Paths,
        job: ServiceJob,
        launchd: FakeLaunchd,
    }

    impl ServiceFixture {
        fn new(loaded: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let data = directory.path().join("data");
            let paths = fixture_paths(&data);
            std::fs::create_dir_all(&paths.state).unwrap();
            write_private(
                &paths.config(),
                json!({"tunnel": "tailscale", "httpsPort": 8444,
                    "listenAddr": "127.0.0.1:3777"})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
            let mut launchd = FakeLaunchd::new(loaded);
            launchd.data = Some(data);
            Self {
                job: ServiceJob {
                    plist: directory
                        .path()
                        .join("LaunchAgents/com.saimun.wonder.cli.plist"),
                    target: "gui/501/com.saimun.wonder.cli".into(),
                    app: "/Applications/Cupola.app".into(),
                    contents: "fixture plist".into(),
                },
                _directory: directory,
                paths,
                launchd,
            }
        }

        fn run(&mut self, command: ServiceCommand) -> Result<()> {
            let launchd = &mut self.launchd;
            run_service(command, &self.paths, &self.job, &mut |args| {
                launchd.run(args)
            })
        }

        fn host_up(&self) -> bool {
            host_up(&self.paths)()
        }
    }

    #[test]
    fn install_registers_the_login_job_while_a_started_host_runs() {
        // `cupola start`, then `cupola service install`.
        let mut fixture = ServiceFixture::new(false);
        fake_host(&fixture.paths.data);
        fixture.run(ServiceCommand::Install).unwrap();
        assert_eq!(fixture.launchd.calls, ["print", "bootstrap"]);
        assert!(fixture.launchd.loaded, "the job runs at the next login");
        assert_eq!(fixture.launchd.hosts_started, 0, "no second host");
        assert!(fixture.host_up());
        // Installing again changes nothing.
        fixture.run(ServiceCommand::Install).unwrap();
        assert_eq!(fixture.launchd.calls, ["print", "bootstrap", "print"]);
    }

    #[test]
    fn fresh_install_leaves_the_host_running() {
        let mut fixture = ServiceFixture::new(false);
        fixture.run(ServiceCommand::Install).unwrap();
        assert_eq!(
            std::fs::read_to_string(&fixture.job.plist).unwrap(),
            "fixture plist"
        );
        assert!(fixture.launchd.loaded);
        assert_eq!(fixture.launchd.hosts_started, 1);
        assert!(fixture.host_up());
    }

    #[test]
    fn restart_brings_a_running_host_back_every_time() {
        let mut fixture = ServiceFixture::new(false);
        fixture.run(ServiceCommand::Install).unwrap();
        for round in 1..=10 {
            fixture.run(ServiceCommand::Restart).unwrap();
            assert!(fixture.host_up(), "round {round}");
            assert_eq!(fixture.launchd.hosts_started, round + 1);
        }
    }

    #[test]
    fn restart_says_why_when_the_host_does_not_come_back() {
        let mut fixture = ServiceFixture::new(false);
        fixture.run(ServiceCommand::Install).unwrap();
        fixture.launchd.runs_start_host = false;
        let error = fixture
            .run(ServiceCommand::Restart)
            .unwrap_err()
            .to_string();
        assert!(!fixture.host_up());
        assert!(
            error.starts_with("The Cupola login service is installed, but the host did not start")
                && error.ends_with("Run `cupola logs` for more."),
            "{error}"
        );
    }

    #[test]
    fn service_named_by_the_old_directory_still_manages_the_job_after_the_move() {
        // A login service installed for rollback sets `~/.wonder`; the first
        // start moves the data to `~/.cupola` and rewrites the job's files.
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let legacy = home.join(wonderd::data_home::LEGACY_DATA_DIR);
        let app = Path::new("/Applications/Cupola.app");
        let id = "com.saimun.wonder.cli";
        let setting = || Some(legacy.clone().into_os_string());
        let paths = Paths::resolve_in(Some(home), setting()).unwrap();
        assert_eq!(paths.data, legacy, "before the move");
        std::fs::create_dir_all(&paths.state).unwrap();
        write_private(
            &paths.config(),
            json!({"tunnel": "tailscale"}).to_string().as_bytes(),
        )
        .unwrap();
        let mut launchd = FakeLaunchd::new(false);
        launchd.data = Some(paths.data.clone());
        let service = |command, paths: &Paths, launchd: &mut FakeLaunchd| {
            let job = ServiceJob::new(app, home, paths, id);
            run_service(command, paths, &job, &mut |args| launchd.run(args))
        };
        service(ServiceCommand::Install, &paths, &mut launchd).unwrap();
        service(ServiceCommand::Restart, &paths, &mut launchd).unwrap();
        stop_host(&paths).unwrap();

        let moved = wonderd::data_home::move_legacy(home).unwrap();
        relocate_service_file(&service_path(home, id), &legacy, &moved).unwrap();
        let paths = Paths::resolve_in(Some(home), setting()).unwrap();
        assert_eq!(paths.data, home.join(wonderd::data_home::DATA_DIR));
        launchd.data = Some(paths.data.clone());
        for command in [
            ServiceCommand::Install,
            ServiceCommand::Restart,
            ServiceCommand::Status,
            ServiceCommand::Uninstall,
        ] {
            service(command, &paths, &mut launchd).unwrap();
        }
        assert!(!service_path(home, id).exists());
        assert!(!host_up(&paths)());
        // The previous release still finds its data through the link.
        assert!(legacy.join("headless/config.json").is_file());
    }

    /// A home with a configured data directory and the login job an
    /// earlier release's app installed.
    fn renamed_app_fixture(old_app: &Path) -> (tempfile::TempDir, Paths, ServiceJob, String) {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let paths = fixture_paths(&home.join(".cupola"));
        std::fs::create_dir_all(&paths.state).unwrap();
        write_private(
            &paths.config(),
            json!({"tunnel": "tailscale"}).to_string().as_bytes(),
        )
        .unwrap();
        let id = "com.saimun.wonder.cli";
        let old = ServiceJob::new(old_app, home, &paths, id).contents;
        std::fs::create_dir_all(home.join("Library/LaunchAgents")).unwrap();
        write_private(&service_path(home, id), old.as_bytes()).unwrap();
        let app = home.join("Applications/Cupola.app");
        let job = ServiceJob::new(&app, home, &paths, id);
        (directory, paths, job, old)
    }

    #[test]
    fn service_commands_from_cupola_app_take_over_the_cupola_cli_app_job() {
        let old_app = Path::new("/Applications/Cupola CLI.app");
        for command in [
            ServiceCommand::Install,
            ServiceCommand::Restart,
            ServiceCommand::Uninstall,
        ] {
            let (_directory, paths, job, _) = renamed_app_fixture(old_app);
            assert_eq!(
                job.moved_from(&std::fs::read_to_string(&job.plist).unwrap())
                    .as_deref(),
                Some(old_app)
            );
            let mut launchd = FakeLaunchd::new(true);
            launchd.data = Some(paths.data.clone());
            let (name, uninstall) = (
                format!("{command:?}"),
                matches!(command, ServiceCommand::Uninstall),
            );
            run_service(command, &paths, &job, &mut |args| launchd.run(args)).unwrap();
            if uninstall {
                assert!(!job.plist.exists());
                assert!(!launchd.loaded);
            } else {
                assert_eq!(std::fs::read_to_string(&job.plist).unwrap(), job.contents);
                // launchd drops the old app's job and loads the new one.
                assert!(launchd.calls.contains(&"bootout".to_string()), "{name}");
                assert_eq!(launchd.calls.last().map(String::as_str), Some("bootstrap"));
                assert!(host_up(&paths)());
                stop_host(&paths).unwrap();
            }
        }
    }

    #[test]
    fn a_job_for_another_data_directory_is_not_taken_over() {
        let (directory, paths, job, _) =
            renamed_app_fixture(Path::new("/Applications/Cupola CLI.app"));
        let other = fixture_paths(&directory.path().join("other"));
        let foreign = ServiceJob::new(
            Path::new("/Applications/Cupola CLI.app"),
            directory.path(),
            &other,
            "com.saimun.wonder.cli",
        )
        .contents;
        write_private(&job.plist, foreign.as_bytes()).unwrap();
        for command in [
            ServiceCommand::Install,
            ServiceCommand::Restart,
            ServiceCommand::Uninstall,
        ] {
            let mut launchd = FakeLaunchd::new(true);
            assert!(run_service(command, &paths, &job, &mut |args| launchd.run(args)).is_err());
            assert_eq!(std::fs::read_to_string(&job.plist).unwrap(), foreign);
        }
        job.follow_removed_app().unwrap();
        assert_eq!(std::fs::read_to_string(&job.plist).unwrap(), foreign);
    }

    #[test]
    fn host_start_points_the_job_at_this_app_only_once_the_old_one_is_gone() {
        let directory = tempfile::tempdir().unwrap();
        let old_app = directory.path().join("A & B/Cupola CLI.app");
        let (_home, _paths, job, old) = renamed_app_fixture(&old_app);
        assert_eq!(job.moved_from(&old).as_deref(), Some(old_app.as_path()));
        // A copy kept beside the new one still owns its job.
        std::fs::create_dir_all(&old_app).unwrap();
        job.follow_removed_app().unwrap();
        assert_eq!(std::fs::read_to_string(&job.plist).unwrap(), old);
        std::fs::remove_dir_all(&old_app).unwrap();
        job.follow_removed_app().unwrap();
        assert_eq!(std::fs::read_to_string(&job.plist).unwrap(), job.contents);
        assert_eq!(job.moved_from(&job.contents), None);
    }

    #[test]
    fn uninstall_stops_the_host_and_unloads_the_job() {
        let mut fixture = ServiceFixture::new(false);
        fixture.run(ServiceCommand::Install).unwrap();
        fixture.run(ServiceCommand::Uninstall).unwrap();
        assert!(!fixture.host_up());
        assert!(!fixture.launchd.loaded);
        assert!(!fixture.job.plist.exists());
    }
}
