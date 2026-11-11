use super::*;
use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};

type HelperInput = Box<dyn AsyncWrite + Unpin + Send>;
type HelperOutput = Box<dyn AsyncRead + Unpin + Send>;

struct LaunchedHelper {
    child: tokio::process::Child,
    stdin: HelperInput,
    stdout: HelperOutput,
    _codec_log: CodecLogTask,
    /// Independent input owner releases its own injected downs after the
    /// capture job closes. It exits within two seconds if its client wedges.
    #[cfg(windows)]
    _input_owner: Option<tokio::process::Child>,
    /// Windows: the job ends the helper and anything it starts (the Python
    /// backend's launcher tree) when the session ends or the daemon exits.
    #[cfg(windows)]
    _job: wonder_app_server::job::Job,
}

/// Own the bounded, non-content codec reader with the helper session.
struct CodecLogTask(tokio::task::JoinHandle<()>);
impl Drop for CodecLogTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Ignore arbitrary diagnostics, including overlong lines. Only a fixed codec
/// token can reach the daemon log; SDP, titles and input text never do.
async fn read_codec_summary(input: &mut (impl AsyncRead + Unpin)) -> Option<&'static str> {
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 512];
    let mut line = Vec::with_capacity(64);
    let mut overlong = false;
    loop {
        let count = input.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        for byte in &chunk[..count] {
            if *byte == b'\n' {
                if !overlong && line == b"WONDER_COMPUTER_CODEC video/VP8" {
                    return Some("VP8");
                }
                line.clear();
                overlong = false;
            } else if line.len() < 64 {
                line.push(*byte);
            } else {
                overlong = true;
            }
        }
    }
}

/// Windows: the helper is spawned suspended (`helper_command`) and runs only
/// once it and everything it starts belong to a kill-on-close job. Unix
/// helpers are single processes owned by `kill_on_drop`.
#[cfg(windows)]
type HelperOwner = fn(&tokio::process::Child) -> std::io::Result<wonder_app_server::job::Job>;

#[cfg(windows)]
fn own_helper(child: &tokio::process::Child) -> std::io::Result<wonder_app_server::job::Job> {
    let (Some(handle), Some(pid)) = (child.raw_handle(), child.id()) else {
        return Err(std::io::Error::other("helper exited before it was owned"));
    };
    let job = wonder_app_server::job::Job::kill_on_close()?;
    job.adopt_suspended(handle, pid)?;
    Ok(job)
}

/// The environment a computer helper (session, discovery worker, dialog
/// watcher, one-shot modes and the macOS LaunchServices proxy's `open`) may
/// see. Helpers run as the owner but parse peer media and screen content, so
/// they never receive the host's own authority: the loopback capability, the
/// relay and pairing settings, provider keys or anything else not listed.
/// Kept: what the OS, Python and the display stack need, and the helper
/// settings below. `WONDER_COMPUTER_USE_HANDSHAKE` is set per launch.
pub fn helper_environment(
    variables: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    variables
        .into_iter()
        .filter(|(name, _)| name.to_str().is_some_and(helper_may_see))
        .collect()
}

/// Who may talk to the computer-use helper. The macOS helper holds Cupola's
/// Screen Recording and Accessibility grants, and its LaunchServices launcher
/// is reached through a private Unix socket. Another process of the same
/// user must not be able to join that socket or drive the helper:
/// the session secret never travels in process arguments (`ps` shows those to
/// every local process), and each end checks the executable of the process on
/// the other end of the socket against the Cupola host's own executable.
/// Unix-only and platform-neutral so the Linux gate exercises it.
#[cfg(unix)]
pub mod helper_channel {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};

    /// File name of the session secret inside the private socket directory.
    pub const TOKEN_FILE: &str = "token";
    /// A session secret is a simple UUID: 32 hex digits.
    pub const TOKEN_LEN: usize = 32;

    /// Stores `token` as `<directory>/token`, readable by the owner only.
    /// Never replaces an existing file.
    pub fn write_token_file(directory: &Path, token: &str) -> std::io::Result<PathBuf> {
        use std::io::Write;
        let path = directory.join(TOKEN_FILE);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        file.write_all(token.as_bytes())?;
        Ok(path)
    }

    /// Reads the secret written by [`write_token_file`]. Refuses a symlink, a
    /// file another user owns, one readable by anyone else, or the wrong size.
    pub fn read_token_file(path: &Path) -> std::io::Result<String> {
        let refuse = |reason: &str| {
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, reason.to_owned())
        };
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(refuse("helper session secret is not a regular file"));
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(refuse("helper session secret belongs to another user"));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(refuse("helper session secret is readable by others"));
        }
        if metadata.len() != TOKEN_LEN as u64 {
            return Err(refuse("helper session secret has the wrong size"));
        }
        let mut token = String::new();
        file.read_to_string(&mut token)?;
        Ok(token)
    }

    /// Process ID and user ID of the process on the other end of `stream`.
    pub fn peer_identity(stream: &UnixStream) -> std::io::Result<(u32, u32)> {
        let fd = stream.as_raw_fd();
        #[cfg(target_os = "macos")]
        {
            // SOL_LOCAL / LOCAL_PEERPID, <sys/un.h>.
            const SOL_LOCAL: libc::c_int = 0;
            const LOCAL_PEERPID: libc::c_int = 0x002;
            let mut pid: libc::pid_t = 0;
            let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
            let status = unsafe {
                libc::getsockopt(
                    fd,
                    SOL_LOCAL,
                    LOCAL_PEERPID,
                    (&mut pid as *mut libc::pid_t).cast(),
                    &mut length,
                )
            };
            if status != 0 || pid <= 0 {
                return Err(std::io::Error::last_os_error());
            }
            let (mut uid, mut gid) = (0, 0);
            if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok((pid as u32, uid))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut credentials = libc::ucred {
                pid: 0,
                uid: 0,
                gid: 0,
            };
            let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let status = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut length,
                )
            };
            if status != 0 || credentials.pid <= 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok((credentials.pid as u32, credentials.uid))
        }
    }

    /// The executable image a running process was started from.
    pub fn process_executable(pid: u32) -> std::io::Result<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
            let mut buffer = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
            let length = unsafe {
                libc::proc_pidpath(
                    pid as libc::c_int,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                )
            };
            if length <= 0 {
                return Err(std::io::Error::last_os_error());
            }
            buffer.truncate(length as usize);
            Ok(PathBuf::from(
                <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(buffer),
            ))
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::fs::read_link(format!("/proc/{pid}/exe"))
        }
    }

    /// Accepts the other end of `stream` only when it is this user's process
    /// running `expected`, the Cupola host executable. Both sides compare
    /// canonical paths, so a symlink to the host is the host.
    pub fn require_peer_executable(stream: &UnixStream, expected: &Path) -> std::io::Result<()> {
        let refuse = |reason: &str| {
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, reason.to_owned())
        };
        let (pid, uid) = peer_identity(stream)?;
        if uid != unsafe { libc::geteuid() } {
            return Err(refuse("helper peer belongs to another user"));
        }
        let actual = crate::canonical_path::canonicalize(process_executable(pid)?)?;
        let expected = crate::canonical_path::canonicalize(expected)?;
        if actual != expected {
            return Err(refuse("helper peer is not the Cupola host"));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const TOKEN: &str = "0123456789abcdef0123456789abcdef";

        #[test]
        fn token_file_is_private_and_never_replaced() {
            let directory = tempfile::tempdir().unwrap();
            let path = write_token_file(directory.path(), TOKEN).unwrap();
            assert_eq!(path, directory.path().join(TOKEN_FILE));
            assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            assert_eq!(read_token_file(&path).unwrap(), TOKEN);
            assert!(write_token_file(directory.path(), "other").is_err());
            assert_eq!(read_token_file(&path).unwrap(), TOKEN);
        }

        #[test]
        fn token_file_that_others_can_read_or_a_symlink_is_refused() {
            let directory = tempfile::tempdir().unwrap();
            let path = write_token_file(directory.path(), TOKEN).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let error = read_token_file(&path).unwrap_err();
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{error}"
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let link = directory.path().join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(
                read_token_file(&link).is_err(),
                "a symlinked secret was read"
            );
            std::fs::write(&path, "short").unwrap();
            assert!(
                read_token_file(&path).is_err(),
                "a wrong-size secret was read"
            );
        }

        #[test]
        fn the_host_is_recognised_by_its_executable_and_anything_else_is_refused() {
            let (host, peer) = UnixStream::pair().unwrap();
            let me = std::env::current_exe().unwrap();
            // Both ends are this test process, which stands in for the host.
            require_peer_executable(&host, &me).unwrap();
            require_peer_executable(&peer, &me).unwrap();
            let other = Path::new("/bin/sh");
            let error = require_peer_executable(&host, other).unwrap_err();
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{error}"
            );
            assert!(error.to_string().contains("not the Cupola host"), "{error}");
        }

        #[test]
        fn a_different_program_on_the_socket_is_not_the_host() {
            use std::os::unix::net::UnixListener;
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("pipe");
            let listener = UnixListener::bind(&socket).unwrap();
            // Another program of the same user connecting, as a local intruder would.
            let Ok(mut intruder) = std::process::Command::new("python3")
                .args([
                    "-c",
                    "import socket,sys,time;s=socket.socket(socket.AF_UNIX);s.connect(sys.argv[1]);time.sleep(5)",
                ])
                .arg(&socket)
                .spawn()
            else {
                return; // no python3 here: the pair test above covers the rule
            };
            let (accepted, _) = listener.accept().unwrap();
            let verdict = require_peer_executable(&accepted, &std::env::current_exe().unwrap());
            let _ = intruder.kill();
            let _ = intruder.wait();
            assert!(
                verdict.is_err(),
                "a foreign program was accepted as the host"
            );
        }
    }
}

fn helper_may_see(name: &str) -> bool {
    const COMMON: &[&str] = &[
        "HOME",
        "PATH",
        "TMPDIR",
        "USER",
        "LOGNAME",
        "LANG",
        "TZ",
        // Helper settings: the Take control preference directory, the
        // Python backend's capture choice and opt-in local statistics.
        "WONDER_SERVICE_DIR",
        "WONDER_CAPTURE_BACKEND",
        "WONDER_COMPUTER_STATS",
        "WONDER_COMPUTER_USE_HANDSHAKE",
    ];
    // Display, session bus, portal and audio sockets for X11/Wayland capture.
    const LINUX: &[&str] = &[
        "DISPLAY",
        "XAUTHORITY",
        "WAYLAND_DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "PULSE_SERVER",
        "PIPEWIRE_RUNTIME_DIR",
        "LD_LIBRARY_PATH",
        "PYTHONPATH",
        "PYTHONHOME",
    ];
    const MACOS: &[&str] = &["__CF_USER_TEXT_ENCODING"];
    // cmd.exe (the Python launcher), Winsock, Python's site paths and the
    // capture libraries resolve these; Windows compares names without case.
    const WINDOWS: &[&str] = &[
        "SYSTEMROOT",
        "WINDIR",
        "SYSTEMDRIVE",
        "COMSPEC",
        "PATHEXT",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "USERNAME",
        "USERDOMAIN",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "COMMONPROGRAMFILES",
        "COMMONPROGRAMFILES(X86)",
        "COMPUTERNAME",
        "OS",
        "PROCESSOR_ARCHITECTURE",
        "NUMBER_OF_PROCESSORS",
        "PUBLIC",
        "PYTHONPATH",
        "PYTHONHOME",
    ];
    let listed = |names: &[&str]| {
        names.iter().any(|allowed| {
            if cfg!(windows) {
                allowed.eq_ignore_ascii_case(name)
            } else {
                *allowed == name
            }
        })
    };
    listed(COMMON)
        || name.starts_with("LC_")
        || if cfg!(target_os = "macos") {
            listed(MACOS)
        } else if cfg!(windows) {
            listed(WINDOWS)
        } else {
            listed(LINUX) || name.starts_with("XDG_")
        }
}

/// Every computer helper process starts here, with `helper_environment`.
pub(crate) fn helper_process(binary: &FsPath) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .envs(helper_environment(std::env::vars_os()))
        .kill_on_drop(true);
    command
}

async fn helper_command(binary: &FsPath) -> Command {
    let mut command = helper_process(binary);
    #[cfg(target_os = "linux")]
    {
        // A phone retry starts a new helper while the service can remain alive
        // across logout/login. Resolve that login again, without changing the
        // daemon's environment or leaking manager/provider settings.
        let environment = crate::linux_desktop::refresh(std::env::vars_os().collect()).await;
        command.env_clear().envs(helper_environment(environment));
    }
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    command.creation_flags(wonder_app_server::job::CREATE_SUSPENDED);
    command
}

/// Host status feature: sessions may use the reserved host-view conversation ID
/// (`wonder_store::HOST_VIEW_CONVERSATION_ID`) before any conversation exists.
/// Session, lease and event code only compares that ID for binding equality;
/// agent computer use resolves real conversations, so it rejects this label.
pub(crate) const HOST_VIEW_FEATURE: &str = "computer-host-view-v1";
const UNAVAILABLE_REASON: &str = "Live computer viewing is unavailable on this computer. Update Cupola when a configured media provider is available.";
const UNAVAILABLE_ACTION: &str = "update-host";
const CONTROL_UNAVAILABLE_REASON: &str =
    "This computer’s input helper is unavailable. Update or restart Cupola on the computer, then reopen this view.";
const CONTROL_AVAILABLE_REASON: &str = "Take control asks for approval on your computer.";
/// The owner chose view only on the Mac; the phone shows it after "View only."
const CONTROL_DISABLED_REASON: &str =
    "Control is turned off on this computer. To allow it, run cupola computer control on there.";
/// Windows' Python screen sharing cannot ask on the computer or show Stop.
const LOCAL_APPROVAL_UNAVAILABLE_REASON: &str =
    "This computer can be viewed but not controlled: its Python screen sharing can’t ask you on the computer first. Switch it to Cupola’s built-in screen sharing to take control.";
const WINDOW_VIEW_ONLY_REASON: &str =
    "This window is view only. Show the whole screen to control the Mac.";
/// Simulator input stays inside the simulator, so there is no approval step.
const SIMULATOR_CONTROL_AVAILABLE_REASON: &str = "Take control to use this simulator from here.";
const SIMULATOR_VIEW_ONLY_REASON: &str =
    "This simulator can be viewed but not controlled with this version of Xcode.";
const SIMULATOR_STOPPED_REASON: &str =
    "This simulator has stopped. Start it again, then reopen this view.";
const SIMULATOR_CONTROL_PENDING_REASON: &str =
    "Checking whether this simulator can be controlled. Try again in a moment.";
const SIMULATOR_TEXT_REASON: &str =
    "The simulator can only type letters, numbers and symbols from a US keyboard. Paste other text instead.";
const CONTROL_HEARTBEAT_SECONDS: u64 = 3;
const CONTROL_EXPIRY_SECONDS: i64 = 10;
const CONTROL_CONSENT_TIMEOUT_SECONDS: u64 = 120;
const CONTROL_CONSENT_RESERVATION_SECONDS: i64 =
    CONTROL_CONSENT_TIMEOUT_SECONDS as i64 + CONTROL_EXPIRY_SECONDS;
const CAPTURE_AVAILABLE_REASON: &str = "Live computer viewing is available on this computer.";
const MAX_INPUT_ACTIONS: usize = 32;
const MAX_INPUT_TEXT: usize = 4_096;
const MAX_CLIPBOARD_TEXT: usize = 8_192;
// Clipboard limit informed by Saffsanity/sill (Apache-2.0), github.com/Saffsanity/sill.
const MAX_SHARED_CLIPBOARD_BYTES: usize = 256 * 1024;
const MAX_INPUT_BATCH_BYTES: usize = 16 * 1024;
const MAX_SIGNALING_SDP_BYTES: usize = 64 * 1024;
const MAX_SIGNALING_CANDIDATE_BYTES: usize = 4 * 1024;
const MAX_SIGNALING_CANDIDATES: usize = 128;
const MAX_SIGNALING_CANDIDATE_BATCH: usize = 32;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StartRequest {
    client_request_id: String,
    conversation_id: String,
    host_installation_id: String,
    #[serde(default)]
    generation: Option<u64>,
    #[serde(default)]
    source: Option<SourceRequest>,
    #[serde(default)]
    video_quality: Option<String>,
    #[serde(default)]
    video_max_height: Option<u16>,
    /// The highest frame rate the viewer will take, 15...120. Absent from
    /// clients that predate the field, which get 15.
    #[serde(default)]
    video_max_frames_per_second: Option<u16>,
    /// An explicit mutation of an existing request's session. Ordinary start
    /// retries stay idempotent and can never implicitly change its source.
    #[serde(default)]
    rebind: Option<SourceRebindRequest>,
    /// The view this open replaces, as the person last saw it. When another
    /// open or close changed the host's view first, the open is refused.
    #[serde(default)]
    replaces: Option<ViewerReplacement>,
    /// Ends another device's view of this computer. Requires `replaces`.
    #[serde(default)]
    take_over: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ViewerReplacement {
    session_id: String,
    generation: u64,
}

/// Why an open cannot replace the host's current view. `viewerBusy` names the
/// device so the phone can offer Take over; `viewChanged` is a real mismatch.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ViewerConflictResponse {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_name: Option<String>,
    session_id: String,
    generation: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QualityRequest {
    generation: u64,
    video_quality: String,
    #[serde(default)]
    video_max_height: Option<u16>,
    /// Absent keeps the session's current ceiling.
    #[serde(default)]
    video_max_frames_per_second: Option<u16>,
}

const MIN_VIDEO_FPS: u16 = 15;
const MAX_VIDEO_FPS: u16 = 120;

/// The ceiling a request names. A request without one keeps `absent`: 15 when
/// opening a view, the session's current value when changing quality.
fn requested_video_fps(requested: Option<u16>, absent: u16) -> Result<u16, &'static str> {
    match requested {
        None => Ok(absent),
        Some(fps) if (MIN_VIDEO_FPS..=MAX_VIDEO_FPS).contains(&fps) => Ok(fps),
        Some(_) => Err("video frame rate is invalid"),
    }
}

fn requested_video_quality(
    quality: Option<&str>,
    max_height: Option<u16>,
) -> Result<&str, &'static str> {
    match (quality.unwrap_or("standard"), max_height) {
        ("auto", Some(1080)) => Ok("medium"),
        ("standard" | "medium" | "auto" | "high", None) => Ok(quality.unwrap_or("standard")),
        _ => Err("video quality is invalid"),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceRequest {
    id: Option<String>,
    name: Option<String>,
    kind: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    scale: Option<f64>,
    crop: Option<CropRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceRebindRequest {
    generation: u64,
    geometry_revision: u64,
    source_id: Option<String>,
    lease_id: Option<String>,
    source: Option<SourceRequest>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceRebindResponse {
    accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    session: SessionResponse,
    lease: Option<LeaseResponse>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CropRequest {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GenerationQuery {
    generation: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdmissionRequest {
    generation: u64,
    role: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignalingBinding {
    generation: u64,
    conversation_id: String,
    host_installation_id: String,
    #[serde(default)]
    peer_revision: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SignalingPollRequest {
    #[serde(flatten)]
    binding: SignalingBinding,
    #[serde(default)]
    cursor: u64,
    #[serde(default)]
    clipboard_subscription_id: Option<String>,
    #[serde(default)]
    clipboard_cursor: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SignalingAnswerRequest {
    #[serde(flatten)]
    binding: SignalingBinding,
    #[serde(rename = "type")]
    kind: String,
    sdp: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SignalingCandidateRequest {
    #[serde(flatten)]
    binding: SignalingBinding,
    sequence: u64,
    candidate: String,
    sdp_mid: Option<String>,
    sdp_m_line_index: u32,
    username_fragment: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SignalingCloseRequest {
    #[serde(flatten)]
    binding: SignalingBinding,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SignalingDescription {
    #[serde(rename = "type")]
    kind: String,
    sdp: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SignalingCandidate {
    sequence: u64,
    candidate: String,
    sdp_mid: Option<String>,
    sdp_m_line_index: u32,
    username_fragment: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SignalingIceServer {
    urls: Vec<String>,
    username: Option<String>,
    credential: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SignalingResponse {
    session_id: String,
    generation: u64,
    peer_revision: Option<String>,
    state: String,
    capture_state: String,
    peer_state: String,
    offer: Option<SignalingDescription>,
    candidates: Vec<SignalingCandidate>,
    cursor: u64,
    next_cursor: u64,
    failure_reason: Option<String>,
    ice_servers: Vec<SignalingIceServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    clipboard: Option<ClipboardResponse>,
    /// While the phone controls the Mac: whether typing focus is in a text
    /// field, so the phone can raise or lower its keyboard. Omitted when the
    /// computer does not report focus (Linux, Windows, older Cupola).
    #[serde(skip_serializing_if = "Option::is_none")]
    text_input_focused: Option<bool>,
}

// Ephemeral authenticated reply only: never published in durable session events.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardResponse {
    subscription_id: Option<String>,
    sequence: u64,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

impl ClipboardResponse {
    /// A helper timeout, error or malformed reply, or capability negotiation
    /// still in progress: transient, unlike an older helper that omits the field.
    fn unavailable(subscription: Option<&str>, cursor: u64) -> Self {
        Self {
            subscription_id: subscription.map(str::to_owned),
            sequence: cursor,
            status: "unavailable".into(),
            text: None,
        }
    }
}

fn clipboard_response(
    value: &serde_json::Value,
    subscription: Option<&str>,
) -> Option<ClipboardResponse> {
    let status = value.get("status")?.as_str()?;
    if !matches!(status, "off" | "watching" | "permissionRequired") {
        return None;
    }
    let received = value
        .get("subscriptionID")
        .and_then(serde_json::Value::as_str);
    if (received != subscription && !(status == "off" && received.is_none()))
        || (status != "watching" && value.get("text").is_some())
    {
        return None;
    }
    let text = match value.get("text") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text))
            if subscription.is_some()
                && !text.is_empty()
                && text.len() <= MAX_SHARED_CLIPBOARD_BYTES =>
        {
            Some(text.clone())
        }
        _ => return None,
    };
    Some(ClipboardResponse {
        subscription_id: received.map(str::to_owned),
        sequence: value.get("sequence")?.as_u64()?,
        status: status.to_owned(),
        text,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AcquireControlRequest {
    client_request_id: String,
    /// The card the owner accepted. Ordinary computer control answers no card.
    #[serde(default)]
    takeover_request_id: Option<String>,
    generation: u64,
    geometry_revision: u64,
    source_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LeaseBindingRequest {
    lease_id: String,
    generation: u64,
    geometry_revision: u64,
    source_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InputBatchRequest {
    lease_id: String,
    generation: u64,
    geometry_revision: u64,
    source_id: Option<String>,
    sequence: u64,
    actions: Vec<InputAction>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum InputAction {
    Pointer {
        x: f64,
        y: f64,
        phase: String,
        button: Option<String>,
    },
    Scroll {
        delta_x: f64,
        delta_y: f64,
    },
    Key {
        key: String,
        phase: String,
        modifiers: u32,
    },
    Text {
        text: String,
    },
    Clipboard {
        operation: String,
        text: Option<String>,
    },
    /// Simulator views only: one finger at a normalized point.
    Touch {
        x: f64,
        y: f64,
        phase: String,
        delay_ms: Option<u32>,
    },
    /// Simulator views only: a hardware button.
    Button {
        button: String,
        phase: String,
    },
    ReleaseAll,
}

const MAX_TOUCH_DELAY_MS: u32 = 1_000;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Capability {
    available: bool,
    action: &'static str,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlCapability {
    available: bool,
    action: &'static str,
    reason: &'static str,
    heartbeat_interval_seconds: u64,
    lease_expiry_seconds: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    id: Option<String>,
    name: Option<String>,
    kind: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    scale: Option<f64>,
    crop: Option<CropRequest>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse {
    id: String,
    client_request_id: String,
    owner_device_id: String,
    host_installation_id: String,
    conversation_id: String,
    generation: u64,
    state: String,
    video_quality: String,
    video_max_frames_per_second: u16,
    supports_live_quality_change: bool,
    supports_control_source_rebind: bool,
    source: Source,
    geometry_revision: u64,
    failure_reason: Option<String>,
    created_at: String,
    updated_at: String,
    last_state_at: String,
    ended_at: Option<String>,
    capability: Capability,
    control: ControlCapability,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdmissionResponse {
    session: SessionResponse,
    role: String,
    granted: bool,
    admission: Option<serde_json::Value>,
    capability: Capability,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaseResponse {
    id: String,
    session_id: String,
    owner_device_id: String,
    host_installation_id: String,
    conversation_id: String,
    generation: u64,
    source_id: Option<String>,
    geometry_revision: u64,
    status: String,
    last_sequence: u64,
    acquired_at: String,
    updated_at: String,
    expires_at: String,
    released_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlActionResponse {
    granted: bool,
    acknowledged: bool,
    status: String,
    reason: String,
    lease: Option<LeaseResponse>,
    control: ControlCapability,
    #[serde(skip_serializing_if = "Option::is_none")]
    clipboard_text: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct AcceptedInputRecovery {
    acknowledged: bool,
    status: &'static str,
    reason: String,
}

fn accepted_input_recovery() -> AcceptedInputRecovery {
    AcceptedInputRecovery {
        acknowledged: true,
        status: "stale",
        reason: "The input reached the computer, but control ended because Cupola could not save its receipt. Do not retry this action.".into(),
    }
}

pub(crate) fn provider_available(state: &AppState) -> bool {
    state.computer_use_enabled
        && state.computer_use_bin.as_ref().is_some_and(|path| {
            std::fs::metadata(path)
                .map(|metadata| metadata.is_file() && is_executable(&metadata))
                .unwrap_or(false)
        })
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
}

fn capability(state: &AppState) -> Capability {
    if provider_available(state) {
        Capability {
            available: true,
            action: "retry",
            reason: CAPTURE_AVAILABLE_REASON,
        }
    } else {
        Capability {
            available: false,
            action: UNAVAILABLE_ACTION,
            reason: UNAVAILABLE_REASON,
        }
    }
}

fn control_capability_for(available: bool) -> ControlCapability {
    ControlCapability {
        available,
        action: if available {
            "none"
        } else {
            UNAVAILABLE_ACTION
        },
        reason: if available {
            CONTROL_AVAILABLE_REASON
        } else {
            CONTROL_UNAVAILABLE_REASON
        },
        heartbeat_interval_seconds: CONTROL_HEARTBEAT_SECONDS,
        lease_expiry_seconds: CONTROL_EXPIRY_SECONDS as u64,
    }
}

fn control_capability(state: &AppState) -> ControlCapability {
    blocked_control_capability(
        state.computer_supervisor.control_capable(),
        state
            .computer_supervisor
            .control_block
            .load(Ordering::Acquire),
    )
}

/// Why the helper's capabilities said input is unavailable, as a
/// [`blocked_control_capability`] code.
fn helper_control_block(value: &serde_json::Value) -> u8 {
    match value["result"]["control"]["reason"].as_str() {
        Some("accessibility_permission_required") => 1,
        Some("wayland_input_unavailable") => 2,
        Some("xdotool_required") => 3,
        Some("x11_display_required") => 4,
        Some("control_disabled") => 5,
        Some("local_approval_unavailable") => 6,
        _ => 0,
    }
}

fn blocked_control_capability(available: bool, block: u8) -> ControlCapability {
    let mut capability = control_capability_for(available);
    if !capability.available {
        capability.reason = match block {
            1 => "On your Mac, allow Cupola in System Settings → Privacy & Security → Accessibility, then reopen this view.",
            2 => "Input is unavailable in this Wayland session. Sign in with an X11 desktop, then reopen this view.",
            3 => "Install xdotool on this computer to enable input, then reopen this view.",
            4 => "No input display is available. Start an X11 desktop session on this computer, then reopen this view.",
            5 => CONTROL_DISABLED_REASON,
            6 => LOCAL_APPROVAL_UNAVAILABLE_REASON,
            _ => CONTROL_UNAVAILABLE_REASON,
        };
        // The owner's choice, not something to update.
        if matches!(block, 5 | 6) {
            capability.action = "none";
        }
    }
    capability
}

/// Window control follows the helper's source capability. Mac windows remain
/// unavailable pending native acceptance; Windows/Linux use their verified input.
fn session_control_capability(
    state: &AppState,
    session: &StoredComputerSession,
) -> ControlCapability {
    if session.source_kind.as_deref() == Some("simulator") {
        return simulator_control_capability(
            state
                .computer_supervisor
                .simulator_control_for(session.source_id.as_deref()),
            state
                .computer_supervisor
                .control_block
                .load(Ordering::Acquire),
        );
    }
    let control = control_capability(state);
    if control.available
        && session.source_kind.as_deref() == Some("window")
        && state
            .computer_supervisor
            .window_control_blocked
            .load(Ordering::Acquire)
    {
        return ControlCapability {
            available: false,
            action: "none",
            reason: WINDOW_VIEW_ONLY_REASON,
            ..control_capability_for(false)
        };
    }
    control
}

/// What the helper reported for the session's exact simulator. Nil until its
/// capabilities answer arrives; the Mac's Accessibility grant does not apply.
fn simulator_control_capability(control: Option<SimulatorControl>, block: u8) -> ControlCapability {
    let mut capability = ControlCapability {
        action: "none",
        ..control_capability_for(false)
    };
    match control {
        None => capability.reason = SIMULATOR_CONTROL_PENDING_REASON,
        Some(control) if control.available && block == 5 => {
            capability.reason = CONTROL_DISABLED_REASON
        }
        Some(control) if control.available => {
            capability.available = true;
            capability.reason = SIMULATOR_CONTROL_AVAILABLE_REASON;
        }
        Some(control) if control.reason.as_deref() == Some("simulator_not_booted") => {
            capability.reason = SIMULATOR_STOPPED_REASON
        }
        Some(_) => capability.reason = SIMULATOR_VIEW_ONLY_REASON,
    }
    capability
}

fn lease_response(lease: StoredComputerControlLease) -> LeaseResponse {
    LeaseResponse {
        id: lease.id,
        session_id: lease.session_id,
        owner_device_id: lease.owner_device_id,
        host_installation_id: lease.host_installation_id,
        conversation_id: lease.conversation_id,
        generation: lease.session_generation,
        source_id: lease.source_id,
        geometry_revision: lease.geometry_revision,
        status: lease.status,
        last_sequence: lease.last_sequence,
        acquired_at: lease.acquired_at,
        updated_at: lease.updated_at,
        expires_at: lease.expires_at,
        released_at: lease.released_at,
    }
}

fn control_response(
    state: &AppState,
    granted: bool,
    acknowledged: bool,
    status: impl Into<String>,
    reason: impl Into<String>,
    lease: Option<StoredComputerControlLease>,
    clipboard_text: Option<String>,
) -> ControlActionResponse {
    ControlActionResponse {
        granted,
        acknowledged,
        status: status.into(),
        reason: reason.into(),
        lease: lease.map(lease_response),
        control: control_capability(state),
        clipboard_text,
    }
}

fn response(state: &AppState, session: StoredComputerSession) -> SessionResponse {
    let control = session_control_capability(state, &session);
    let failure_reason = session
        .failure_reason
        .as_deref()
        .map(|reason| failure_words(reason, session.source_kind.as_deref()));
    SessionResponse {
        id: session.id,
        client_request_id: session.client_request_id,
        owner_device_id: session.owner_device_id,
        host_installation_id: session.host_installation_id,
        conversation_id: session.conversation_id,
        generation: session.generation,
        state: session.state,
        video_quality: session.video_quality,
        video_max_frames_per_second: session.video_max_fps,
        supports_live_quality_change: state.computer_supervisor.quality_capable(),
        supports_control_source_rebind: true,
        source: Source {
            id: session.source_id,
            name: session.source_name,
            kind: session.source_kind,
            width: session.source_width,
            height: session.source_height,
            scale: session.source_scale,
            crop: session
                .crop_json
                .and_then(|value| serde_json::from_str(&value).ok()),
        },
        geometry_revision: session.geometry_revision,
        failure_reason,
        created_at: session.created_at,
        updated_at: session.updated_at,
        last_state_at: session.last_state_at,
        ended_at: session.ended_at,
        capability: capability(state),
        control,
    }
}

fn validate_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= max && !value.chars().any(char::is_control)
}

fn validate_input_action(action: &InputAction) -> bool {
    match action {
        InputAction::Pointer {
            x,
            y,
            phase,
            button,
        } => {
            x.is_finite()
                && y.is_finite()
                && (0.0..=1.0).contains(x)
                && (0.0..=1.0).contains(y)
                && matches!(phase.as_str(), "move" | "down" | "up")
                && button
                    .as_deref()
                    .is_none_or(|value| matches!(value, "left" | "right" | "middle"))
                && (phase == "move" || button.is_some())
        }
        InputAction::Scroll { delta_x, delta_y } => {
            delta_x.is_finite()
                && delta_y.is_finite()
                && delta_x.abs() <= 4_096.0
                && delta_y.abs() <= 4_096.0
        }
        InputAction::Key {
            key,
            phase,
            modifiers,
        } => {
            validate_text(key, 32)
                && matches!(phase.as_str(), "down" | "up" | "press")
                && (*modifiers & !0b1_1111) == 0
        }
        InputAction::Text { text } => {
            !text.is_empty()
                && text.chars().count() <= MAX_INPUT_TEXT
                && text
                    .chars()
                    .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        }
        InputAction::Clipboard { operation, text } => {
            matches!(operation.as_str(), "copyToPhone" | "pasteFromPhone")
                && text.as_ref().is_none_or(|value| {
                    value.chars().count() <= MAX_CLIPBOARD_TEXT
                        && value
                            .chars()
                            .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
                })
                && ((operation == "copyToPhone" && text.is_none())
                    || (operation == "pasteFromPhone" && text.is_some()))
        }
        InputAction::Touch {
            x,
            y,
            phase,
            delay_ms,
        } => {
            x.is_finite()
                && y.is_finite()
                && (0.0..=1.0).contains(x)
                && (0.0..=1.0).contains(y)
                && matches!(phase.as_str(), "down" | "move" | "up")
                && delay_ms.is_none_or(|delay| delay <= MAX_TOUCH_DELAY_MS)
        }
        InputAction::Button { button, phase } => {
            matches!(button.as_str(), "home" | "lock" | "volumeUp" | "volumeDown")
                && matches!(phase.as_str(), "press" | "down" | "up")
        }
        InputAction::ReleaseAll => true,
    }
}

/// Simulators take touch, buttons, keys and typed text; computers never take
/// touch or simulator buttons. Checked before the batch reserves a sequence.
fn actions_fit_source(source_kind: Option<&str>, actions: &[InputAction]) -> bool {
    let simulator = source_kind == Some("simulator");
    actions.iter().all(|action| match action {
        InputAction::Touch { .. } | InputAction::Button { .. } => simulator,
        InputAction::Key { .. } | InputAction::Text { .. } | InputAction::ReleaseAll => true,
        InputAction::Pointer { .. }
        | InputAction::Scroll { .. }
        | InputAction::Clipboard { .. } => !simulator,
    })
}

/// The simulator types through US keyboard keys; the phone offers paste for
/// anything else, so this refusal leaves control in place.
fn simulator_typeable(actions: &[InputAction]) -> bool {
    actions.iter().all(|action| match action {
        InputAction::Text { text } => text
            .chars()
            .all(|character| matches!(character, ' '..='~' | '\n' | '\t' | '\r')),
        _ => true,
    })
}

fn validate_input_batch(request: &InputBatchRequest) -> Result<(), &'static str> {
    if request.lease_id.trim().is_empty()
        || request.lease_id.chars().count() > 128
        || request.generation == 0
        || request.sequence == 0
        || request
            .source_id
            .as_deref()
            .is_some_and(|value| !validate_text(value, 160))
        || request.actions.is_empty()
        || request.actions.len() > MAX_INPUT_ACTIONS
        || (request.actions.len() != 1
            && request.actions.iter().any(|action| {
                matches!(
                    action,
                    InputAction::Clipboard { operation, .. } if operation == "pasteFromPhone"
                )
            }))
        || request
            .actions
            .iter()
            .any(|action| !validate_input_action(action))
        || serde_json::to_vec(request)
            .map(|bytes| bytes.len() > MAX_INPUT_BATCH_BYTES)
            .unwrap_or(true)
    {
        return Err("input batch is invalid");
    }
    Ok(())
}

fn validate_binding_request(request: &LeaseBindingRequest) -> bool {
    validate_text(&request.lease_id, 128)
        && request.generation > 0
        && request
            .source_id
            .as_deref()
            .is_none_or(|value| validate_text(value, 160))
}

fn session_binding_matches(
    session: &StoredComputerSession,
    generation: u64,
    geometry_revision: u64,
    source_id: Option<&str>,
) -> bool {
    session.generation == generation
        && session.geometry_revision == geometry_revision
        && session.source_id.as_deref() == source_id
}

fn lease_matches_request(
    lease: &StoredComputerControlLease,
    state: &AppState,
    session: &StoredComputerSession,
    request: &AcquireControlRequest,
) -> bool {
    lease.owner_device_id == session.owner_device_id
        && lease.host_installation_id == state.host_installation_id
        && lease.session_id == session.id
        && lease.conversation_id == session.conversation_id
        && lease.session_generation == request.generation
        && lease.geometry_revision == request.geometry_revision
        && lease.source_id.as_deref() == request.source_id.as_deref()
        && lease.client_request_id == request.client_request_id
}

fn control_identity_params(
    session: &StoredComputerSession,
    lease: &StoredComputerControlLease,
) -> serde_json::Value {
    serde_json::json!({
        "sessionID": session.id,
        "generation": lease.session_generation,
        "leaseID": lease.id,
        "requestID": lease.client_request_id,
        "geometryRevision": lease.geometry_revision,
        "sourceID": lease.source_id,
    })
}

fn control_unavailable(state: &AppState, session: StoredComputerSession) -> Response {
    let capability = session_control_capability(state, &session);
    let reason = if capability.available {
        session
            .failure_reason
            .as_deref()
            .map(|reason| failure_words(reason, session.source_kind.as_deref()))
            .unwrap_or_else(|| {
                "The computer view is not live. Wait for it to reconnect, then try again.".into()
            })
    } else {
        capability.reason.to_owned()
    };
    let mut reply = control_response(state, false, false, "unavailable", reason, None, None);
    reply.control = capability;
    (StatusCode::SERVICE_UNAVAILABLE, Json(reply)).into_response()
}

#[allow(clippy::type_complexity)]
fn source_fields(
    source: Option<SourceRequest>,
) -> Result<
    (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<u32>,
        Option<u32>,
        Option<f64>,
        Option<String>,
    ),
    &'static str,
> {
    let Some(source) = source else {
        return Ok((None, None, None, None, None, None, None));
    };
    for value in [&source.id, &source.name, &source.kind]
        .into_iter()
        .flatten()
    {
        if !validate_text(value, 160) {
            return Err("source metadata is invalid");
        }
    }
    if source
        .width
        .is_some_and(|value| !(1..=16_384).contains(&value))
        || source
            .height
            .is_some_and(|value| !(1..=16_384).contains(&value))
        || source
            .scale
            .is_some_and(|value| !value.is_finite() || !(0.1..=8.0).contains(&value))
    {
        return Err("source geometry is invalid");
    }
    let crop_json = source
        .crop
        .map(|value| {
            if [value.x, value.y, value.width, value.height]
                .into_iter()
                .any(|coordinate| {
                    !coordinate.is_finite() || !(0.0..=16_384.0).contains(&coordinate)
                })
                || value.width <= 0.0
                || value.height <= 0.0
            {
                return Err("source crop is invalid");
            }
            serde_json::to_string(&value).map_err(|_| "source crop is invalid")
        })
        .transpose()?;
    if crop_json.as_ref().is_some_and(|value| value.len() > 2048) {
        return Err("source crop is too large");
    }
    Ok((
        source.id,
        source.name,
        source.kind,
        source.width,
        source.height,
        source.scale,
        crop_json,
    ))
}

#[allow(clippy::result_large_err)]
async fn owner_device(
    authenticated: Option<Extension<AuthenticatedDevice>>,
) -> Result<AuthenticatedDevice, Response> {
    authenticated
        .map(|Extension(device)| device)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "paired device required").into_response())
}

#[allow(clippy::result_large_err)]
async fn owned_session(
    state: &AppState,
    id: &str,
    device_id: &str,
) -> Result<StoredComputerSession, Response> {
    let session = state
        .store
        .computer_session(id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "computer session unavailable",
            )
                .into_response()
        })?
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    if session.owner_device_id != device_id
        || session.host_installation_id != state.host_installation_id
    {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    Ok(session)
}

fn valid_signaling_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn valid_sdp(value: &str) -> bool {
    value.len() <= MAX_SIGNALING_SDP_BYTES
        && !value.is_empty()
        && value.starts_with("v=0")
        && value.contains("\na=")
        && value
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\r' | '\n' | '\t'))
}

fn valid_ice_candidate(request: &SignalingCandidateRequest) -> bool {
    request.sequence > 0
        && request.sequence <= MAX_SIGNALING_CANDIDATES as u64
        && request.sdp_m_line_index <= i32::MAX as u32
        && request.candidate.len() <= MAX_SIGNALING_CANDIDATE_BYTES
        && request.candidate.starts_with("candidate:")
        && !request.candidate.chars().any(char::is_control)
        && request
            .sdp_mid
            .as_deref()
            .is_none_or(|value| valid_signaling_text(value, 128))
        && request
            .username_fragment
            .as_deref()
            .is_none_or(|value| valid_signaling_text(value, 256))
}

/// The computer view tries the home network first and falls back to Tailscale.
/// Each peer gathers every interface but ranks them its own way (aiortc gives
/// every host candidate one priority, so whichever interface it lists first
/// wins), so the relay drops a Tailscale candidate's local preference to zero
/// in both directions. Its type and component stay, every home network
/// candidate outranks it, and it still connects when nothing else answers.
fn tailscale_last(candidate: &str) -> Cow<'_, str> {
    // candidate:<foundation> <component> <transport> <priority> <address> <port> typ ...
    let mut fields: Vec<&str> = candidate.split(' ').collect();
    let (Some(Ok(priority)), Some(Ok(address))) = (
        fields.get(3).map(|value| value.parse::<u32>()),
        fields.get(4).map(|value| value.parse::<IpAddr>()),
    ) else {
        return Cow::Borrowed(candidate);
    };
    let lowered = priority & 0xFF00_00FF;
    if !crate::lan::tailscale_address(address) || lowered == priority {
        return Cow::Borrowed(candidate);
    }
    let lowered = lowered.to_string();
    fields[3] = &lowered;
    Cow::Owned(fields.join(" "))
}

/// [`tailscale_last`] for the candidates an offer carries in its SDP (aiortc
/// gathers before it offers), keeping every other byte and line ending.
fn tailscale_last_in_sdp(sdp: &str) -> Cow<'_, str> {
    let mut changed = false;
    let rewritten: String = sdp
        .split_inclusive('\n')
        .map(|line| {
            let body = line.trim_end_matches(['\r', '\n']);
            let Some(candidate) = body
                .strip_prefix("a=")
                .filter(|value| value.starts_with("candidate:"))
            else {
                return Cow::Borrowed(line);
            };
            match tailscale_last(candidate) {
                Cow::Borrowed(_) => Cow::Borrowed(line),
                Cow::Owned(lowered) => {
                    changed = true;
                    Cow::Owned(format!("a={lowered}{}", &line[body.len()..]))
                }
            }
        })
        .collect();
    if changed {
        Cow::Owned(rewritten)
    } else {
        Cow::Borrowed(sdp)
    }
}

/// The candidate the helper receives for a phone request. A retry of the same
/// request reserves the same value.
fn forwarded_candidate(request: &SignalingCandidateRequest) -> SignalingCandidate {
    SignalingCandidate {
        sequence: request.sequence,
        candidate: tailscale_last(&request.candidate).into_owned(),
        sdp_mid: request.sdp_mid.clone(),
        sdp_m_line_index: request.sdp_m_line_index,
        username_fragment: request.username_fragment.clone(),
    }
}

fn signal_binding_error(
    session: &StoredComputerSession,
    binding: &SignalingBinding,
) -> Option<Response> {
    if binding.host_installation_id != session.host_installation_id
        || binding.conversation_id != session.conversation_id
    {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    if binding.generation != session.generation {
        return Some(
            (StatusCode::CONFLICT, "computer session generation mismatch").into_response(),
        );
    }
    if matches!(session.state.as_str(), "ended" | "failed" | "unavailable") {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    None
}

#[allow(clippy::result_large_err)]
async fn signaling_handle(
    state: &AppState,
    session: &StoredComputerSession,
    device_id: &str,
) -> Result<SignalingHandle, Response> {
    state
        .computer_supervisor
        .signaling_handle(&session.id, device_id)
        .await
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())
}

fn signaling_snapshot(
    session_id: &str,
    generation: u64,
    source_kind: Option<&str>,
    signaling: &SignalingState,
    cursor: u64,
) -> SignalingResponse {
    let candidates: Vec<_> = signaling
        .local_candidates
        .iter()
        .filter(|candidate| candidate.sequence > cursor)
        .take(MAX_SIGNALING_CANDIDATE_BATCH)
        .cloned()
        .collect();
    let next_cursor = cursor.max(
        candidates
            .last()
            .map(|candidate| candidate.sequence)
            .unwrap_or(cursor),
    );
    SignalingResponse {
        session_id: session_id.to_owned(),
        generation,
        peer_revision: signaling.peer_revision.clone(),
        state: signaling.public_state(),
        capture_state: signaling.capture_state.clone(),
        peer_state: signaling.peer_state.clone(),
        offer: signaling.offer.clone(),
        candidates,
        cursor,
        next_cursor,
        failure_reason: signaling
            .failure_reason
            .as_deref()
            .map(|reason| failure_words(reason, source_kind)),
        ice_servers: Vec::new(),
        clipboard: None,
        text_input_focused: signaling.text_input_focused,
    }
}

pub(crate) async fn signal_poll(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<SignalingPollRequest>,
) -> Response {
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if let Some(response) = signal_binding_error(&session, &request.binding) {
        return response;
    }
    let handle = match signaling_handle(&state, &session, &device.device_id).await {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    if request
        .clipboard_subscription_id
        .as_deref()
        .is_some_and(|id| !valid_signaling_text(id, 128))
    {
        return (StatusCode::BAD_REQUEST, "clipboard subscription is invalid").into_response();
    }
    handle.mark_viewer_active();
    // Old clients omit the subscription and never read clipboard content. Unknown
    // helper methods stay optional, so older/non-Mac helpers still provide video.
    let requested = request.clipboard_subscription_id.as_deref();
    let (capable, forward) = {
        let mut signaling = handle.signaling.lock().await;
        // Skip the helper unless a subscription is starting, active, or still
        // needs its stop delivered.
        let forward = signaling.clipboard_capable == Some(true)
            && (requested.is_some() || signaling.clipboard_subscribed);
        if forward && requested.is_some() {
            signaling.clipboard_subscribed = true;
        }
        (signaling.clipboard_capable, forward)
    };
    let clipboard = if forward {
        let reply = handle
            .request(
                "clipboard.poll",
                serde_json::json!({
                    "subscriptionID": requested,
                    "cursor": request.clipboard_cursor,
                }),
            )
            .await
            .ok()
            .and_then(|value| clipboard_response(&value, requested));
        if requested.is_none() && reply.as_ref().is_some_and(|reply| reply.status == "off") {
            handle.signaling.lock().await.clipboard_subscribed = false;
        }
        reply.or_else(|| {
            requested.map(|id| ClipboardResponse::unavailable(Some(id), request.clipboard_cursor))
        })
    } else if capable.is_none() {
        requested.map(|id| ClipboardResponse::unavailable(Some(id), request.clipboard_cursor))
    } else {
        None
    };
    let signaling = handle.signaling.lock().await;
    let mut snapshot = signaling_snapshot(
        &session.id,
        session.generation,
        session.source_kind.as_deref(),
        &signaling,
        request.cursor,
    );
    snapshot.clipboard = clipboard;
    Json(snapshot).into_response()
}

pub(crate) async fn signal_answer(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<SignalingAnswerRequest>,
) -> Response {
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    if request.kind != "answer" || !valid_sdp(&request.sdp) {
        return (StatusCode::BAD_REQUEST, "computer answer is invalid").into_response();
    }
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if let Some(response) = signal_binding_error(&session, &request.binding) {
        return response;
    }
    let handle = match signaling_handle(&state, &session, &device.device_id).await {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let peer_revision = request.binding.peer_revision.clone();
    let duplicate = {
        let mut signaling = handle.signaling.lock().await;
        let Some(peer_revision) = request
            .binding
            .peer_revision
            .as_deref()
            .filter(|revision| valid_signaling_text(revision, 128))
        else {
            return (StatusCode::BAD_REQUEST, "computer peer revision is invalid").into_response();
        };
        if signaling.peer_revision.as_deref() != Some(peer_revision) {
            return (StatusCode::CONFLICT, "computer peer revision mismatch").into_response();
        }
        match signaling.reserve_answer(&request.sdp) {
            Ok(true) => false,
            Ok(false) => true,
            Err(reason) => return (StatusCode::CONFLICT, reason).into_response(),
        }
    };
    handle.mark_viewer_active();
    if duplicate {
        return Json(serde_json::json!({"accepted": true, "duplicate": true})).into_response();
    }
    let (delivered_tx, delivered_rx) = tokio::sync::oneshot::channel();
    if handle
        .commands
        .try_send(HelperCommand::RemoteAnswer {
            peer_revision: peer_revision.unwrap_or_default(),
            sdp: request.sdp.clone(),
            delivered: delivered_tx,
        })
        .is_err()
    {
        handle.signaling.lock().await.rollback_answer(&request.sdp);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "computer publisher is unavailable",
        )
            .into_response();
    }
    if !matches!(
        timeout(HELPER_COMMAND_DELIVERY_TIMEOUT, delivered_rx).await,
        Ok(Ok(true))
    ) {
        handle.signaling.lock().await.rollback_answer(&request.sdp);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "computer publisher is unavailable",
        )
            .into_response();
    }
    Json(serde_json::json!({"accepted": true})).into_response()
}

pub(crate) async fn signal_candidate(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<SignalingCandidateRequest>,
) -> Response {
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    if !valid_ice_candidate(&request) {
        return (StatusCode::BAD_REQUEST, "computer ICE candidate is invalid").into_response();
    }
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if let Some(response) = signal_binding_error(&session, &request.binding) {
        return response;
    }
    let handle = match signaling_handle(&state, &session, &device.device_id).await {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let peer_revision = request.binding.peer_revision.clone();
    let candidate = forwarded_candidate(&request);
    let duplicate = {
        let mut signaling = handle.signaling.lock().await;
        let Some(peer_revision) = request
            .binding
            .peer_revision
            .as_deref()
            .filter(|revision| valid_signaling_text(revision, 128))
        else {
            return (StatusCode::BAD_REQUEST, "computer peer revision is invalid").into_response();
        };
        if signaling.peer_revision.as_deref() != Some(peer_revision) {
            return (StatusCode::CONFLICT, "computer peer revision mismatch").into_response();
        }
        match signaling.reserve_candidate(candidate.clone()) {
            Ok(true) => false,
            Ok(false) => true,
            Err("computer ICE candidate limit reached") => {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "computer ICE candidate limit reached",
                )
                    .into_response();
            }
            Err(reason) => return (StatusCode::CONFLICT, reason).into_response(),
        }
    };
    handle.mark_viewer_active();
    if duplicate {
        return Json(serde_json::json!({"accepted": true, "duplicate": true})).into_response();
    }
    let (delivered_tx, delivered_rx) = tokio::sync::oneshot::channel();
    if handle
        .commands
        .try_send(HelperCommand::RemoteCandidate {
            peer_revision: peer_revision.unwrap_or_default(),
            sequence: candidate.sequence,
            candidate: candidate.candidate,
            sdp_mid: candidate.sdp_mid,
            sdp_m_line_index: candidate.sdp_m_line_index,
            username_fragment: candidate.username_fragment,
            delivered: delivered_tx,
        })
        .is_err()
    {
        handle
            .signaling
            .lock()
            .await
            .rollback_candidate(request.sequence);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "computer publisher is unavailable",
        )
            .into_response();
    }
    if !matches!(
        timeout(HELPER_COMMAND_DELIVERY_TIMEOUT, delivered_rx).await,
        Ok(Ok(true))
    ) {
        handle
            .signaling
            .lock()
            .await
            .rollback_candidate(request.sequence);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "computer publisher is unavailable",
        )
            .into_response();
    }
    Json(serde_json::json!({"accepted": true, "duplicate": false})).into_response()
}

pub(crate) async fn signal_close(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<SignalingCloseRequest>,
) -> Response {
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if let Some(response) = signal_binding_error(&session, &request.binding) {
        return response;
    }
    let handle = match signaling_handle(&state, &session, &device.device_id).await {
        Ok(handle) => handle,
        Err(response) => return response,
    };
    let peer_revision = request.binding.peer_revision.clone();
    let reason = request.reason.as_deref().unwrap_or("closed_by_viewer");
    if !valid_signaling_text(reason, 128) {
        return (StatusCode::BAD_REQUEST, "computer close reason is invalid").into_response();
    }
    {
        let signaling = handle.signaling.lock().await;
        let Some(peer_revision) = request
            .binding
            .peer_revision
            .as_deref()
            .filter(|revision| valid_signaling_text(revision, 128))
        else {
            return (StatusCode::BAD_REQUEST, "computer peer revision is invalid").into_response();
        };
        if signaling.peer_revision.as_deref() != Some(peer_revision) {
            return (StatusCode::CONFLICT, "computer peer revision mismatch").into_response();
        }
    }
    handle.mark_viewer_active();
    let (delivered_tx, delivered_rx) = tokio::sync::oneshot::channel();
    if handle
        .commands
        .try_send(HelperCommand::CloseSignal {
            peer_revision: peer_revision.unwrap_or_default(),
            reason: reason.to_owned(),
            delivered: delivered_tx,
        })
        .is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "computer publisher is unavailable",
        )
            .into_response();
    }
    if !matches!(
        timeout(HELPER_COMMAND_DELIVERY_TIMEOUT, delivered_rx).await,
        Ok(Ok(true))
    ) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "computer publisher is unavailable",
        )
            .into_response();
    }
    reconcile_media_state(&state, &session, &handle.signaling)
        .await
        .ok();
    Json(serde_json::json!({
        "accepted": true,
        "closed": true,
        "sessionId": session.id,
        "generation": session.generation
    }))
    .into_response()
}

/// Native discovery shares one bounded helper process per host; it neither
/// starts capture nor acquires input control.
pub(crate) async fn simulator_sources(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    discover_sources(&state, &[], None).await
}

/// One Automation prompt at a time; a second tap while it is up changes nothing.
static BROWSER_PERMISSION_ASKING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The owner asked from the phone to let Wonder read Chrome's windows (to find
/// Claude in Chrome's tab). Shows the macOS Automation prompt on the Mac; only
/// the person there can answer it. Discovery itself never asks.
pub(crate) async fn ask_browser_permission(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    let Some(binary) = state
        .computer_use_bin
        .clone()
        .filter(|_| cfg!(target_os = "macos"))
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Computer use is not set up",
        )
            .into_response();
    };
    use std::sync::atomic::Ordering;
    if !BROWSER_PERMISSION_ASKING.swap(true, Ordering::SeqCst) {
        let child = helper_process(&binary)
            .args(["--automation", "com.google.Chrome", "--ask"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match child {
            Ok(mut child) => {
                // The prompt waits for the person at the Mac; stop waiting after two minutes.
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(120), child.wait()).await;
                    BROWSER_PERMISSION_ASKING.store(false, Ordering::SeqCst);
                });
            }
            Err(_) => {
                BROWSER_PERMISSION_ASKING.store(false, Ordering::SeqCst);
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Computer use is not set up",
                )
                    .into_response();
            }
        }
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"state": "asked"})),
    )
        .into_response()
}

/// Installed Mac apps for the phone's launcher, by name. Icons come separately
/// (`installed_app_icon`) so the list stays small.
pub(crate) async fn installed_apps(
    State(_state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    if !cfg!(target_os = "macos") && !cfg!(test) {
        return Json(serde_json::json!({
            "available": false,
            "apps": [],
            "reason": "Opening apps from your phone works with a Mac.",
        }))
        .into_response();
    }
    let apps = crate::computer_apps::catalog().await;
    let apps: Vec<_> = apps
        .iter()
        .map(|app| serde_json::json!({"id": app.id, "name": app.name}))
        .collect();
    Json(serde_json::json!({"available": true, "apps": apps})).into_response()
}

#[derive(Deserialize)]
pub(crate) struct InstalledAppQuery {
    id: String,
}

/// The host's dock for the phone's "Windows & apps" menu: pinned apps, then
/// apps in use. `available` is false when this computer has no dock to read.
pub(crate) async fn dock(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    let dock = crate::computer_apps::dock(state.computer_use_bin.as_deref()).await;
    Json(dock.to_json()).into_response()
}

pub(crate) async fn installed_app_icon(
    State(_state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Query(query): Query<InstalledAppQuery>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    if !crate::computer_apps::valid_bundle_id(&query.id) {
        return (StatusCode::BAD_REQUEST, "Invalid app").into_response();
    }
    let base64_png = |png: Vec<u8>| {
        (
            [(axum::http::header::CACHE_CONTROL, "private, max-age=86400")],
            Json(serde_json::json!({
                "iconPNG": base64::engine::general_purpose::STANDARD.encode(png),
            })),
        )
            .into_response()
    };
    // Dock icons are already normalised when the dock was read.
    if let Some(icon) = crate::computer_apps::last_dock()
        .and_then(|dock| dock.find(&query.id).and_then(|app| app.icon.clone()))
    {
        return base64_png(icon);
    }
    let apps = crate::computer_apps::catalog().await;
    let Some(app) = apps.iter().find(|app| app.id == query.id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::task::spawn_blocking(move || crate::computer_apps::icon_png(&app.path)).await {
        // Base64 in JSON like window icons, so the phone's signed JSON client reads it.
        Ok(Some(png)) => (
            [(axum::http::header::CACHE_CONTROL, "private, max-age=86400")],
            Json(serde_json::json!({
                "iconPNG": base64::engine::general_purpose::STANDARD.encode(png),
            })),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct WindowSourceQuery {
    apps: Option<String>,
    #[serde(default, rename = "macDialogs")]
    mac_dialogs: bool,
    /// Chrome's ID for the tab an agent's Claude in Chrome tools named. The
    /// answer then says which listed Chrome window holds it (`browserTab`).
    #[serde(rename = "browserTab")]
    browser_tab: Option<u64>,
}

pub(crate) async fn window_sources(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Query(query): Query<WindowSourceQuery>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    if query.mac_dialogs {
        return match crate::mac_dialogs::observe() {
            Some(snapshot) => Json(snapshot).into_response(),
            None => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Checking the computer's dialogs.",
            )
                .into_response(),
        };
    }
    let apps = match query.apps {
        None => vec![ALL_WINDOWS.to_owned()],
        Some(apps) => {
            let apps: Vec<_> = apps.split(',').map(str::to_owned).collect();
            if apps.len() > 8 || apps.iter().any(|app| !valid_source_app(app)) {
                return (StatusCode::BAD_REQUEST, "Invalid app selection").into_response();
            }
            apps
        }
    };
    discover_sources(&state, &apps, query.browser_tab).await
}

// Internal discovery key only. On the JSON-lines wire this is `allWindows`,
// keeping [] as the existing simulator query and filtered callers compatible.
const ALL_WINDOWS: &str = "*";
const MAX_WINDOW_ICON_BASE64: usize = 3072;

fn valid_source_app(app: &str) -> bool {
    !app.is_empty()
        && app.len() <= 150
        && app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
}

fn valid_discovered_source(source: &serde_json::Value, apps: &[String]) -> bool {
    let id = source
        .get("id")
        .and_then(|s| s.as_str())
        .unwrap_or_default();
    if !valid_signaling_text(id, 160)
        || source
            .get("name")
            .and_then(|s| s.as_str())
            .is_none_or(|s| !valid_signaling_text(s, 160))
    {
        return false;
    }
    if ["appName", "windowTitle"].iter().any(|key| {
        source.get(key).is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|text| !valid_signaling_text(text, 160))
        })
    }) || source.get("iconPNG").is_some_and(|value| {
        value.as_str().is_none_or(|text| {
            text.len() > MAX_WINDOW_ICON_BASE64
                || base64::Engine::decode(&base64::engine::general_purpose::STANDARD, text)
                    .map_or(true, |data| !data.starts_with(b"\x89PNG\r\n\x1a\n"))
        })
    }) {
        return false;
    }
    if apps.is_empty() {
        source.get("kind").and_then(|s| s.as_str()) == Some("simulator")
            && id
                .strip_prefix("simulator:")
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .is_some()
    } else {
        let Some(app) = source.get("appId").and_then(|s| s.as_str()).filter(|app| {
            valid_source_app(app) && apps.iter().any(|a| a == app || a == ALL_WINDOWS)
        }) else {
            return false;
        };
        source.get("kind").and_then(|s| s.as_str()) == Some("window")
            && id
                .strip_prefix(&format!("app:{app}#"))
                .is_some_and(|number| {
                    !number.is_empty()
                        && number.bytes().all(|b| b.is_ascii_digit())
                        && number.parse::<u32>().is_ok_and(|id| id > 0)
                })
    }
}

/// How the helper's Chrome tab lookup ended (`BrowserTabWindow` in the helper).
const BROWSER_TAB_STATES: [&str; 6] = [
    "exact",
    "ambiguous",
    "notFound",
    "automationNeeded",
    "automationDenied",
    "unavailable",
];

/// The helper's tab answer: a known state, and for `exact` a listed source.
fn valid_browser_tab(value: &serde_json::Value) -> bool {
    let state = value.get("browserTab").and_then(|s| s.as_str());
    let source = value.get("browserTabSource");
    match state {
        None => source.is_none(),
        Some("exact") => source.and_then(|s| s.as_str()).is_some_and(|id| {
            value["sources"]
                .as_array()
                .is_some_and(|sources| sources.iter().any(|s| s["id"] == id))
        }),
        Some(state) => BROWSER_TAB_STATES.contains(&state) && source.is_none(),
    }
}

const SOURCE_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SOURCE_MAX_STALENESS: Duration = Duration::from_secs(30);
const SOURCE_RETRY_MAX: Duration = Duration::from_secs(30);
const SOURCE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
const SOURCE_DISCOVERY_CACHE_LIMIT: usize = 16;
/// A discovery worker no query used for this long exits; the next query
/// starts another. An open picker polls every `SOURCE_POLL_INTERVAL`.
const SOURCE_DISCOVERY_IDLE: Duration = Duration::from_secs(120);

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SourceDiscoveryKey {
    binary: PathBuf,
    apps: Vec<String>,
    browser_tab: Option<u64>,
}

impl SourceDiscoveryKey {
    fn new(binary: &FsPath, apps: &[String], browser_tab: Option<u64>) -> Self {
        let mut apps = apps.to_vec();
        apps.sort();
        apps.dedup();
        Self {
            binary: binary.to_owned(),
            apps,
            browser_tab,
        }
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
enum SourceDiscoveryHealth {
    #[default]
    Unknown,
    Fresh,
    Retaining,
    Unavailable,
}

#[derive(Default)]
struct SourceDiscoveryEntry {
    environment: Option<Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>>,
    last_good: Option<(tokio::time::Instant, serde_json::Value)>,
    next_poll_at: Option<tokio::time::Instant>,
    failures: u32,
    health: SourceDiscoveryHealth,
}

impl SourceDiscoveryEntry {
    fn cached(&self, now: tokio::time::Instant) -> Option<&serde_json::Value> {
        self.last_good.as_ref().and_then(|(at, value)| {
            (now.saturating_duration_since(*at) < SOURCE_MAX_STALENESS).then_some(value)
        })
    }

    fn finish_poll(&mut self, result: Option<serde_json::Value>, now: tokio::time::Instant) {
        let delay = if let Some(value) = result {
            // Only validated helper results advance this timestamp. In particular,
            // returning cached data must never extend the staleness deadline.
            self.last_good = Some((now, value));
            self.failures = 0;
            SOURCE_POLL_INTERVAL
        } else {
            self.failures = self.failures.saturating_add(1);
            SOURCE_POLL_INTERVAL
                .saturating_mul(1 << self.failures.saturating_sub(1).min(3))
                .min(SOURCE_RETRY_MAX)
        };
        self.next_poll_at = Some(now + delay);
    }

    fn answer(
        &mut self,
        now: tokio::time::Instant,
        logger: &logging::JsonlLogger,
    ) -> Option<serde_json::Value> {
        let health = if self.failures == 0 {
            SourceDiscoveryHealth::Fresh
        } else if self.cached(now).is_some() {
            SourceDiscoveryHealth::Retaining
        } else {
            SourceDiscoveryHealth::Unavailable
        };
        if health != self.health {
            // One bounded operational record per outage/expiry/recovery, never
            // window titles, app IDs, raw helper output or one record per retry.
            if !(self.health == SourceDiscoveryHealth::Unknown
                && health == SourceDiscoveryHealth::Fresh)
            {
                let state = match health {
                    SourceDiscoveryHealth::Fresh => "recovered",
                    SourceDiscoveryHealth::Retaining => "retaining",
                    _ => "unavailable",
                };
                let _ = logger.record(
                    if health == SourceDiscoveryHealth::Fresh { "info" } else { "warn" },
                    "computer_source_discovery_state",
                    serde_json::json!({
                        "state": state,
                        "failures": self.failures,
                        "retryMs": self.next_poll_at.map(|at| at.saturating_duration_since(now).as_millis() as u64),
                    }),
                );
            }
            self.health = health;
        }
        self.cached(now).cloned()
    }
}

type SourceDiscoveryEntries = BTreeMap<
    SourceDiscoveryKey,
    (
        tokio::time::Instant,
        Arc<tokio::sync::Mutex<SourceDiscoveryEntry>>,
    ),
>;

/// Cache and retries belong to the complete discovery query, including a Chrome
/// tab binding. Unrelated queries must never borrow each other's last snapshot.
struct SourceDiscovery {
    entries: tokio::sync::Mutex<SourceDiscoveryEntries>,
    helper: Arc<tokio::sync::Mutex<Option<SourceDiscoveryHelper>>>,
    stopped: AtomicBool,
}

impl Default for SourceDiscovery {
    fn default() -> Self {
        Self {
            entries: tokio::sync::Mutex::new(BTreeMap::new()),
            helper: Arc::new(tokio::sync::Mutex::new(None)),
            stopped: AtomicBool::new(false),
        }
    }
}

impl SourceDiscovery {
    async fn entry(
        &self,
        key: SourceDiscoveryKey,
    ) -> Option<Arc<tokio::sync::Mutex<SourceDiscoveryEntry>>> {
        let mut entries = self.entries.lock().await;
        let now = tokio::time::Instant::now();
        if let Some((used, entry)) = entries.get_mut(&key) {
            *used = now;
            return Some(Arc::clone(entry));
        }
        if entries.len() >= SOURCE_DISCOVERY_CACHE_LIMIT {
            // An in-flight query retains its slot, so eviction cannot create a
            // second poll/retry state for the same query.
            let oldest = entries
                .iter()
                .filter(|(_, (_, entry))| Arc::strong_count(entry) == 1)
                .min_by_key(|(_, (used, _))| *used)
                .map(|(key, _)| key.clone())?;
            entries.remove(&oldest);
        }
        let entry = Arc::new(tokio::sync::Mutex::new(SourceDiscoveryEntry::default()));
        entries.insert(key, (now, Arc::clone(&entry)));
        Some(entry)
    }

    async fn discover(
        &self,
        binary: &FsPath,
        apps: &[String],
        browser_tab: Option<u64>,
        logger: &logging::JsonlLogger,
    ) -> Option<serde_json::Value> {
        let key = SourceDiscoveryKey::new(binary, apps, browser_tab);
        let environment = command_environment(&helper_command(binary).await);
        let entry = self.entry(key.clone()).await?;
        // Identical callers share one poll; cached/backing-off queries do not
        // wait behind another query's helper process.
        let mut entry = entry.lock().await;
        if entry.environment.as_ref() != Some(&environment) {
            // No snapshot or retry backoff from the previous login may mask a
            // newly usable desktop (or continue advertising a logged-out one).
            *entry = SourceDiscoveryEntry {
                environment: Some(environment),
                ..Default::default()
            };
        }
        let now = tokio::time::Instant::now();
        if entry.next_poll_at.is_none_or(|at| now >= at) {
            let result = self.poll_sources(binary, &key.apps, browser_tab).await;
            entry.finish_poll(result, tokio::time::Instant::now());
        }
        entry.answer(tokio::time::Instant::now(), logger)
    }
}

/// One observation-only JSONL process, owned by the discovery supervisor. Taking
/// it out of the shared slot during I/O also makes cancellation kill the child:
/// an abandoned reply can never satisfy a later query. Tokio reaps dropped
/// kill_on_drop children; normal errors/shutdown explicitly close and wait.
struct SourceDiscoveryHelper {
    binary: PathBuf,
    environment: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
    /// Tells this launch apart from a later one in the same slot.
    launch: u64,
    /// When a query last finished with this worker.
    used: tokio::time::Instant,
    child: tokio::process::Child,
    #[cfg(windows)]
    _job: wonder_app_server::job::Job,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: BufReader<tokio::process::ChildStdout>,
    buffer: Vec<u8>,
}

impl SourceDiscoveryHelper {
    fn launch(binary: &FsPath, mut command: Command) -> Option<Self> {
        let environment = command_environment(&command);
        let mut child = command.arg("--discover-sources").spawn().ok()?;
        // Unowned, the worker would never resume; `kill_on_drop` ends it.
        #[cfg(windows)]
        let job = own_helper(&child).ok()?;
        static LAUNCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Some(Self {
            binary: binary.to_owned(),
            environment,
            launch: LAUNCHES.fetch_add(1, Ordering::Relaxed),
            used: tokio::time::Instant::now(),
            #[cfg(windows)]
            _job: job,
            stdin: Some(child.stdin.take()?),
            stdout: BufReader::new(child.stdout.take()?),
            child,
            buffer: Vec::new(),
        })
    }

    async fn poll(
        &mut self,
        apps: &[String],
        browser_tab: Option<u64>,
    ) -> Option<serde_json::Value> {
        let all_windows = apps.iter().any(|app| app == ALL_WINDOWS);
        let selected: Vec<_> = apps
            .iter()
            .filter(|app| app.as_str() != ALL_WINDOWS)
            .collect();
        let mut request = serde_json::to_vec(&serde_json::json!({
            "apps": selected, "allWindows": all_windows, "browserTab": browser_tab
        }))
        .ok()?;
        request.push(b'\n');
        let stdin = self.stdin.as_mut()?;
        stdin.write_all(&request).await.ok()?;
        stdin.flush().await.ok()?;
        let line = read_bounded_line(&mut self.stdout, &mut self.buffer)
            .await
            .ok()??;
        validated_discovery_answer(serde_json::from_slice(&line).ok()?, apps)
    }

    async fn shutdown(mut self) {
        // Closing the pipe is the discovery worker's normal shutdown signal.
        drop(self.stdin.take());
        if timeout(Duration::from_secs(1), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

fn command_environment(command: &Command) -> Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> {
    command
        .as_std()
        .get_envs()
        .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
        .collect()
}

impl SourceDiscovery {
    async fn poll_sources(
        &self,
        binary: &FsPath,
        apps: &[String],
        browser_tab: Option<u64>,
    ) -> Option<serde_json::Value> {
        let mut slot = timeout(SOURCE_DISCOVERY_TIMEOUT, self.helper.lock())
            .await
            .ok()?;
        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        // Discovery is persistent too: retire a worker from the previous login
        // before it can keep reporting that capture is unavailable after a retry.
        let command = helper_command(binary).await;
        let environment = command_environment(&command);
        let mut helper = match slot.take() {
            Some(helper) if helper.binary == binary && helper.environment == environment => helper,
            previous => {
                if let Some(helper) = previous {
                    helper.shutdown().await;
                }
                let helper = SourceDiscoveryHelper::launch(binary, command)?;
                tokio::spawn(retire_when_idle(
                    Arc::downgrade(&self.helper),
                    helper.launch,
                ));
                helper
            }
        };
        let result = timeout(SOURCE_DISCOVERY_TIMEOUT, helper.poll(apps, browser_tab))
            .await
            .ok()
            .flatten();
        if result.is_some() {
            helper.used = tokio::time::Instant::now();
            *slot = Some(helper);
        } else {
            helper.shutdown().await;
        }
        result
    }

    async fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(helper) = self.helper.lock().await.take() {
            helper.shutdown().await;
        }
    }
}

/// Stops one launch of the discovery worker once no query has used it for
/// `SOURCE_DISCOVERY_IDLE`. A query holds the slot's lock for its whole poll,
/// and this holds it while the worker exits, so there is never a second one.
async fn retire_when_idle(
    slot: std::sync::Weak<tokio::sync::Mutex<Option<SourceDiscoveryHelper>>>,
    launch: u64,
) {
    let mut due = tokio::time::Instant::now() + SOURCE_DISCOVERY_IDLE;
    loop {
        tokio::time::sleep_until(due).await;
        let Some(slot) = slot.upgrade() else { return };
        let mut slot = slot.lock().await;
        match slot.as_ref() {
            Some(helper) if helper.launch == launch => {
                due = helper.used + SOURCE_DISCOVERY_IDLE;
                if tokio::time::Instant::now() >= due {
                    if let Some(helper) = slot.take() {
                        helper.shutdown().await;
                    }
                    return;
                }
            }
            // Already stopped, or replaced by a launch with its own timer.
            _ => return,
        }
    }
}

fn validated_discovery_answer(
    value: serde_json::Value,
    apps: &[String],
) -> Option<serde_json::Value> {
    let available = value.get("available")?.as_bool()?;
    let sources = value.get("sources")?.as_array()?;
    if sources
        .iter()
        .any(|source| !valid_discovered_source(source, apps))
        || !valid_browser_tab(&value)
    {
        return None;
    }
    // The native helper orders eligible windows by recent activation before
    // applying this same cap. Older/alternate helpers may return more: retain
    // that ordering rather than rejecting a perfectly usable snapshot.
    // No sources are selectable when the host says it cannot capture. Never
    // let stale metadata turn a headless/disconnected display into a choice.
    let sources: Vec<_> = sources
        .iter()
        .take(if available { 32 } else { 0 })
        .cloned()
        .collect();
    let mut answer = serde_json::json!({"available": available, "sources": sources});
    if let Some(capture) = value.get("capture") {
        // One host-reported capability owns picker eligibility. Only the
        // permission reason is recoverable; never infer it from host platform.
        if capture.get("available")?.as_bool()? != available {
            return None;
        }
        let mut capability = serde_json::json!({"available": available});
        if let Some(reason) = capture.get("reason") {
            if available
                || !matches!(
                    reason.as_str()?,
                    "screen_recording_permission_required"
                        | "no_display"
                        | "wayland_unsupported"
                        | "secure_desktop"
                        | "unavailable"
                )
            {
                return None;
            }
            capability["reason"] = reason.clone();
        }
        if let Some(live_quality) = capture.get("liveQualityChange") {
            capability["liveQualityChange"] = serde_json::json!(live_quality.as_bool()?);
        }
        answer["capture"] = capability;
    }
    for key in ["browserTab", "browserTabSource"] {
        if let Some(field) = value.get(key) {
            answer[key] = field.clone();
        }
    }
    if !valid_browser_tab(&answer) {
        // An exact tab outside the bounded list cannot identify a visible pill.
        answer["browserTab"] = serde_json::json!("notFound");
        answer.as_object_mut()?.remove("browserTabSource");
    }
    Some(answer)
}

async fn discover_sources(state: &AppState, apps: &[String], browser_tab: Option<u64>) -> Response {
    // Every helper reports the same source contract and actual capture
    // capability. Linux without a display reports unavailable itself.
    if !state.computer_use_enabled {
        return Json(serde_json::json!({"available": false, "sources": []})).into_response();
    }
    let Some(binary) = state.computer_use_bin.as_ref() else {
        return Json(serde_json::json!({"available": false, "sources": []})).into_response();
    };
    match state
        .computer_supervisor
        .source_discovery
        .discover(binary, apps, browser_tab, &state.logger)
        .await
    {
        Some(value) => Json(value).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Computer views could not be loaded",
        )
            .into_response(),
    }
}

pub(crate) async fn create(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Json(request): Json<StartRequest>,
) -> Response {
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    if !validate_text(&request.client_request_id, 160)
        || !validate_text(&request.conversation_id, 160)
        || request.host_installation_id != state.host_installation_id
        || request.generation.unwrap_or(1) != 1
    {
        return (
            StatusCode::BAD_REQUEST,
            "computer session request is invalid",
        )
            .into_response();
    }
    if let Some(rebind) = request.rebind {
        let existing = match state
            .store
            .computer_session_by_request(&device.device_id, &request.client_request_id)
            .await
        {
            Ok(Some(existing))
                if existing.conversation_id == request.conversation_id
                    && existing.host_installation_id == request.host_installation_id =>
            {
                existing
            }
            _ => return (StatusCode::NOT_FOUND, "computer session is unavailable").into_response(),
        };
        return set_source(
            State(state),
            Some(Extension(device)),
            Path(existing.id),
            Json(rebind),
        )
        .await;
    }
    let video_quality =
        match requested_video_quality(request.video_quality.as_deref(), request.video_max_height) {
            Ok(quality) => quality,
            Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
        };
    let video_max_fps =
        match requested_video_fps(request.video_max_frames_per_second, MIN_VIDEO_FPS) {
            Ok(fps) => fps,
            Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
        };
    // A reserved source intent stays within the existing session wire contract.
    // Only this explicit navigation raises the watcher's window; ordinary
    // discovery and window viewing remain read-only.
    let mac_dialog = request
        .source
        .as_ref()
        .is_some_and(|source| source.id.as_deref() == Some("mac-dialog"));
    let mut source = match source_fields(request.source) {
        Ok(source) => source,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    if !state
        .store
        .computer_conversation_allowed(&request.conversation_id)
        .await
        .unwrap_or(false)
    {
        return (StatusCode::NOT_FOUND, "conversation is unavailable").into_response();
    }
    match state
        .store
        .computer_session_by_request(&device.device_id, &request.client_request_id)
        .await
    {
        Ok(Some(existing)) => {
            if existing.conversation_id != request.conversation_id
                || existing.host_installation_id != state.host_installation_id
                || existing.video_quality != video_quality
                || existing.video_max_fps != video_max_fps
            {
                return (
                    StatusCode::CONFLICT,
                    "client request is already bound to another session",
                )
                    .into_response();
            }
            if provider_available(&state)
                && matches!(
                    existing.state.as_str(),
                    "preparing" | "awaitingSource" | "paused" | "stale" | "live"
                )
            {
                let _ = state
                    .computer_supervisor
                    .start(state.clone(), existing.clone())
                    .await;
            }
            return Json(response(&state, existing)).into_response();
        }
        Ok(None) => {}
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "computer session unavailable",
            )
                .into_response()
        }
    }
    if request.take_over && request.replaces.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "computer session request is invalid",
        )
            .into_response();
    }
    let replacing = if provider_available(&state) {
        match viewer_admission(
            &state,
            &device,
            request.replaces.as_ref(),
            request.take_over,
        )
        .await
        {
            Ok(replacing) => replacing,
            Err(response) => return response,
        }
    } else {
        None
    };
    if mac_dialog {
        let dialog_source = match raise_dialog_source(&state).await {
            Ok(source) => source,
            Err(response) => return response,
        };
        source = match source_fields(Some(dialog_source)) {
            Ok(source) => source,
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The dialog window is unavailable.",
                )
                    .into_response();
            }
        };
    }
    let capture_available = provider_available(&state);
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let session = ComputerSessionCreate {
        id: uuid::Uuid::new_v4().to_string(),
        client_request_id: request.client_request_id.clone(),
        owner_device_id: device.device_id.clone(),
        host_installation_id: state.host_installation_id.clone(),
        conversation_id: request.conversation_id.clone(),
        generation: 1,
        state: if capture_available {
            ComputerSessionState::Preparing
        } else {
            ComputerSessionState::Unavailable
        },
        video_quality: video_quality.to_owned(),
        video_max_fps,
        source_id: source.0,
        source_name: source.1,
        source_kind: source.2,
        source_width: source.3,
        source_height: source.4,
        source_scale: source.5,
        crop_json: source.6,
        geometry_revision: 0,
        failure_reason: (!capture_available).then(|| UNAVAILABLE_REASON.into()),
        now: now.clone(),
    };
    // Recheck and replace under the host's lifecycle owner, taking the old
    // view's input lock first (the order every handler uses).
    let replaced_control = replacing
        .as_ref()
        .map(|old| state.computer_supervisor.session_control(&old.id));
    let mut replaced_input = match replaced_control.as_ref() {
        Some(control) => Some(control.lock().await),
        None => None,
    };
    let lifecycle = state.computer_supervisor.lifecycle.lock().await;
    if capture_available {
        // Decide again under the owner: the device may have been revoked, or
        // the view closed or replaced, while this open waited for the locks.
        let current = match viewer_admission(
            &state,
            &device,
            request.replaces.as_ref(),
            request.take_over,
        )
        .await
        {
            Ok(current) => current,
            Err(response) => return response,
        };
        let expected = replacing.as_ref().map(|old| (&old.id, old.generation));
        // None: the device's own view closed meanwhile, so nothing is replaced.
        if let Some(current) = current.as_ref() {
            if Some((&current.id, current.generation)) != expected {
                return view_changed(&current.id, current.generation);
            }
        }
        if let (Some(old), Some(input)) = (current.as_ref(), replaced_input.as_mut()) {
            if !retire_viewer(&state, old, input).await {
                return view_changed(&old.id, old.generation);
            }
        }
    }
    drop(replaced_input);
    let stored = match state.store.insert_computer_session(&session).await {
        Ok(stored) => stored,
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => match state
            .store
            .computer_session_by_request(&device.device_id, &request.client_request_id)
            .await
        {
            Ok(Some(existing))
                if existing.conversation_id == request.conversation_id
                    && existing.host_installation_id == state.host_installation_id
                    && existing.video_quality == video_quality
                    && existing.video_max_fps == video_max_fps =>
            {
                existing
            }
            _ => {
                return (
                    StatusCode::CONFLICT,
                    "client request is already bound to another session",
                )
                    .into_response()
            }
        },
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "computer session could not be saved",
            )
                .into_response()
        }
    };
    if capture_available {
        if let Err(error) = state
            .computer_supervisor
            .start_reserved(state.clone(), stored.clone(), ViewTimings::DEFAULT)
            .await
        {
            if error == SupervisorStartError::Conflict {
                let _ = state
                    .store
                    .end_computer_session(
                        &stored.id,
                        &device.device_id,
                        Some(stored.generation),
                        &now,
                    )
                    .await;
                return (
                    StatusCode::CONFLICT,
                    "another computer session is already preparing on this computer",
                )
                    .into_response();
            }
            let _ = state
                .store
                .transition_computer_session(
                    &stored.id,
                    stored.generation,
                    ComputerSessionState::Failed,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    stored.geometry_revision,
                    Some(error.reason()),
                    &now,
                )
                .await;
            let closed_by = match error {
                SupervisorStartError::Unavailable => "computer_provider_unavailable",
                _ => "computer_provider_failed_to_start",
            };
            log_session_end(&state, &stored, "launch", closed_by, None, Duration::ZERO).await;
        }
    }
    // The new helper listens for revocations from here on; one sent before it
    // subscribed is visible in the store, so end the view now rather than miss it.
    if !device_is_paired(&state, &device.device_id).await {
        state
            .computer_supervisor
            .stop_reserved(&state, &stored.id, "owner_revoked")
            .await;
        if let Ok(Some(ended)) = state
            .store
            .end_computer_session(&stored.id, &device.device_id, None, &now)
            .await
        {
            publish_session_event(&state, &ended).await;
        }
        return (StatusCode::UNAUTHORIZED, "paired device required").into_response();
    }
    drop(lifecycle);
    let stored = state
        .store
        .computer_session(&stored.id)
        .await
        .ok()
        .flatten()
        .unwrap_or(stored);
    let _ = publish_event_with_context(
        &state,
        WonderEvent::ComputerSessionChanged {
            session_id: stored.id.clone(),
            generation: stored.generation,
            state: stored.state.clone(),
        },
        EventContext {
            request_id: Some(stored.client_request_id.clone()),
            conversation_id: Some(stored.conversation_id.clone()),
            ..EventContext::default()
        },
    )
    .await;
    (StatusCode::CREATED, Json(response(&state, stored))).into_response()
}

/// Decides whether this open may replace the host's current view: the same
/// device reopening (after a crash or a lost close) replaces its own view;
/// another device must ask to take over. Returns the view to replace.
async fn viewer_admission(
    state: &AppState,
    device: &AuthenticatedDevice,
    replaces: Option<&ViewerReplacement>,
    take_over: bool,
) -> Result<Option<StoredComputerSession>, Response> {
    let active = state
        .store
        .active_computer_session_for_host(&state.host_installation_id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "computer session availability could not be verified",
            )
                .into_response()
        })?;
    let devices = state.store.list_owner_devices().await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "computer session availability could not be verified",
        )
            .into_response()
    })?;
    // A revoked device can never open a view or end someone else's.
    if !devices
        .iter()
        .any(|value| value.id == device.device_id && value.revoked_at.is_none())
    {
        return Err((StatusCode::UNAUTHORIZED, "paired device required").into_response());
    }
    let Some(active) = active else {
        // Taking over a view that has since closed is a stale decision; the
        // person should see the computer is free before opening it. A device
        // reopening its own closed view simply opens a new one.
        return match replaces {
            Some(expected) if take_over => {
                Err(view_changed(&expected.session_id, expected.generation))
            }
            _ => Ok(None),
        };
    };
    if let Some(expected) = replaces {
        if expected.session_id != active.id || expected.generation != active.generation {
            return Err(view_changed(&active.id, active.generation));
        }
    }
    let viewer = devices
        .iter()
        .find(|value| value.id == active.owner_device_id && value.revoked_at.is_none());
    // A view held by a forgotten or revoked device has no one to ask.
    if active.owner_device_id == device.device_id || take_over || viewer.is_none() {
        return Ok(Some(active));
    }
    let device_name = viewer
        .map(|value| value.label.trim().to_owned())
        .filter(|label| !label.is_empty());
    let message = match device_name.as_deref() {
        Some(name) => format!("This computer is being viewed on {name}."),
        None => "This computer is being viewed on another device.".into(),
    };
    Err((
        StatusCode::CONFLICT,
        Json(ViewerConflictResponse {
            code: "viewerBusy",
            message,
            device_name,
            session_id: active.id,
            generation: active.generation,
        }),
    )
        .into_response())
}

fn view_changed(session_id: &str, generation: u64) -> Response {
    (
        StatusCode::CONFLICT,
        Json(ViewerConflictResponse {
            code: "viewChanged",
            message: "The computer view changed. Close this screen and try again.".into(),
            device_name: None,
            session_id: session_id.to_owned(),
            generation,
        }),
    )
        .into_response()
}

async fn device_is_paired(state: &AppState, device_id: &str) -> bool {
    state.store.list_owner_devices().await.is_ok_and(|devices| {
        devices
            .iter()
            .any(|value| value.id == device_id && value.revoked_at.is_none())
    })
}

/// Ends the replaced view: its pending consent, lease and helper go with it,
/// so the new view inherits no control. The caller holds `lifecycle` and the
/// old view's input lock. Returns false when the view changed meanwhile.
async fn retire_viewer(
    state: &AppState,
    old: &StoredComputerSession,
    input: &mut SessionControl,
) -> bool {
    input.cancel_consent(None);
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let Ok(Some(ended)) = state
        .store
        .end_computer_session(&old.id, &old.owner_device_id, Some(old.generation), &now)
        .await
    else {
        return false;
    };
    state
        .computer_supervisor
        .stop_reserved(state, &ended.id, "viewer_replaced")
        .await;
    publish_session_event(state, &ended).await;
    true
}

async fn raise_dialog_source(state: &AppState) -> Result<SourceRequest, Response> {
    let Some(window) = crate::mac_dialogs::dialog_window() else {
        crate::mac_dialogs::clear_gone(state).await;
        return Err((StatusCode::GONE, "That dialog has closed.").into_response());
    };
    let Some(binary) = state
        .computer_use_bin
        .as_ref()
        .filter(|_| cfg!(target_os = "macos"))
    else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "The dialog could not be brought forward.",
        )
            .into_response());
    };
    let result = async {
        let mut child = helper_process(binary)
            .args([
                "--raise-dialog",
                &window.window_id.to_string(),
                &window.owner,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = tokio::io::BufReader::new(child.stdout.take()?);
        let line = read_bounded_line(&mut stdout, &mut Vec::new())
            .await
            .ok()??;
        let value: serde_json::Value = serde_json::from_slice(&line).ok()?;
        if !child.wait().await.ok()?.success()
            || value["windowId"].as_u64() != Some(window.window_id.into())
        {
            return None;
        }
        value["state"].as_str().map(str::to_owned)
    };
    match timeout(Duration::from_secs(8), result).await.ok().flatten().as_deref() {
        Some("raised") => Ok(SourceRequest {
            id: Some(format!("app:{}#{}", window.owner, window.window_id)), name: Some("Mac dialog".into()),
            kind: Some("window".into()), width: None, height: None, scale: None, crop: None,
        }),
        Some("gone") => {
            crate::mac_dialogs::forget_window(state, &window).await;
            Err((StatusCode::GONE, "That dialog has closed.").into_response())
        }
        _ => Err((StatusCode::SERVICE_UNAVAILABLE, "The dialog could not be brought forward. Run cupola permissions grant on your Mac to allow Accessibility, then try again.").into_response()),
    }
}

pub(crate) async fn read(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Query(query): Query<GenerationQuery>,
) -> Response {
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if query
        .generation
        .is_some_and(|generation| generation != session.generation)
    {
        return (StatusCode::CONFLICT, "computer session generation mismatch").into_response();
    }
    Json(response(&state, session)).into_response()
}

pub(crate) async fn set_quality(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<QualityRequest>,
) -> Response {
    let control = state.computer_supervisor.session_control(&id);
    let _input_guard = control.lock().await;
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if session.generation != request.generation
        || !matches!(session.state.as_str(), "live" | "paused")
    {
        return (StatusCode::CONFLICT, "computer session is not live").into_response();
    }
    let quality =
        match requested_video_quality(Some(&request.video_quality), request.video_max_height) {
            Ok(quality) => quality,
            Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
        };
    let video_max_fps =
        match requested_video_fps(request.video_max_frames_per_second, session.video_max_fps) {
            Ok(fps) => fps,
            Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
        };
    if quality == session.video_quality && video_max_fps == session.video_max_fps {
        return Json(response(&state, session)).into_response();
    }
    if !state.computer_supervisor.quality_capable() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "live quality change is unavailable",
        )
            .into_response();
    }
    let handle = match state
        .computer_supervisor
        .signaling_handle(&session.id, &device.device_id)
        .await
    {
        Some(handle) => handle,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "computer publisher is unavailable",
            )
                .into_response()
        }
    };
    match handle
        .request(
            "capture.setQuality",
            serde_json::json!({ "videoQuality": quality, "videoMaxFramesPerSecond": video_max_fps }),
        )
        .await
    {
        Ok(value) if value.get("accepted").and_then(serde_json::Value::as_bool) == Some(true) => {}
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "computer quality could not be changed",
            )
                .into_response()
        }
    }
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    match state
        .store
        .update_computer_session_video_quality(
            &session.id,
            &device.device_id,
            session.generation,
            quality,
            video_max_fps,
            &now,
        )
        .await
    {
        Ok(Some(updated)) => Json(response(&state, updated)).into_response(),
        Ok(None) => (StatusCode::CONFLICT, "computer session changed").into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "computer quality was not saved",
        )
            .into_response(),
    }
}

/// The view is a binding of one session and lease, never a new control request.
/// Hold the delivery boundary through helper drain/rebind and the durable CAS.
pub(crate) async fn set_source(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<SourceRebindRequest>,
) -> Response {
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    let control = state.computer_supervisor.session_control(&id);
    let _input_guard = control.lock().await;
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if session.state != "live"
        || !session_binding_matches(
            &session,
            request.generation,
            request.geometry_revision,
            request.source_id.as_deref(),
        )
    {
        return (
            StatusCode::CONFLICT,
            "The computer view changed. Reopen the window menu and try again.",
        )
            .into_response();
    }
    let selected = match source_fields(request.source) {
        Ok(source)
            if source
                .2
                .as_deref()
                .is_none_or(|kind| matches!(kind, "window" | "display")) =>
        {
            source
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "Choose an application window or Whole screen.",
            )
                .into_response()
        }
    };
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let lease = match state
        .store
        .active_computer_control_lease_for_session(&session.id, &now)
        .await
    {
        Ok(lease)
            if lease.as_ref().map(|lease| lease.id.as_str()) == request.lease_id.as_deref()
                && lease
                    .as_ref()
                    .is_none_or(|lease| lease_binding_for_session(lease, &session))
                && _input_guard.pending_consent.is_none() =>
        {
            lease
        }
        Ok(_) => {
            return (
                StatusCode::CONFLICT,
                "Control changed. Refresh this view before choosing another window.",
            )
                .into_response()
        }
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Control could not be checked. Try choosing the window again.",
            )
                .into_response()
        }
    };
    if lease.is_some()
        && selected.2.as_deref() == Some("window")
        && state
            .computer_supervisor
            .window_control_blocked
            .load(Ordering::Acquire)
    {
        return Json(SourceRebindResponse {
            accepted: false,
            reason: Some("Give control back before choosing a Mac window. Show the whole screen to control the Mac."),
            session: response(&state, session),
            lease: lease.map(lease_response),
        })
        .into_response();
    }
    let Some(handle) = state
        .computer_supervisor
        .signaling_handle(&id, &device.device_id)
        .await
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The computer view is unavailable. Reopen it and try again.",
        )
            .into_response();
    };
    let mut params = lease
        .as_ref()
        .map(|lease| control_identity_params(&session, lease))
        .unwrap_or_else(|| {
            serde_json::json!({
                "sessionID": session.id,
                "generation": session.generation,
                "sourceID": session.source_id,
                "geometryRevision": session.geometry_revision,
            })
        });
    params["nextSourceID"] = serde_json::json!(selected.0);
    handle.signaling.lock().await.source_rebinding = true;
    let result = handle.request("capture.rebind", params).await;
    // Only an explicit refusal with the unchanged live binding proves that
    // native delivery did not move. Timeouts, protocol errors and changed
    // geometry remain ambiguous and must still revoke the view below.
    let refused_unchanged = result.as_ref().ok().is_some_and(|value| {
        value["accepted"].as_bool() == Some(false)
            && serde_json::from_value::<HelperStatus>(value["status"].clone()).is_ok_and(|status| {
                matches!(status.state.as_str(), "capturing" | "suspended")
                    && status.session_id.as_deref() == Some(session.id.as_str())
                    && status.generation == Some(session.generation)
                    && status.source_id == session.source_id
                    && status.geometry_revision == Some(session.geometry_revision)
            })
    });
    if refused_unchanged {
        handle.signaling.lock().await.source_rebinding = false;
        return Json(SourceRebindResponse {
            accepted: false,
            reason: Some(
                "That window could not be opened. It may have closed. Choose another window.",
            ),
            session: response(&state, session),
            lease: lease.map(lease_response),
        })
        .into_response();
    }
    let rebound = async {
        let value = result.map_err(|_| ())?;
        if value["accepted"].as_bool() != Some(true) {
            return Err(());
        }
        let status: HelperStatus =
            serde_json::from_value(value["status"].clone()).map_err(|_| ())?;
        if status.session_id.as_deref() != Some(session.id.as_str())
            || status.generation != Some(session.generation)
            || status.state != "capturing"
        {
            return Err(());
        }
        let source: HelperSource =
            serde_json::from_value(value["source"].clone()).map_err(|_| ())?;
        let mut next = session.clone();
        next.source_id = source.id;
        next.source_name = source.title;
        next.source_kind = source.kind;
        // The renderer needs the encoded frame's aspect as well as the native
        // window's crop. Mac keeps a fixed-size padded encoder across rebinds;
        // waiting for its next metadata event would briefly mis-map the video.
        let configuration = &value["status"]["configuration"];
        let encoded_size = |key: &str| {
            configuration[key]
                .as_u64()
                .filter(|size| (1..=16_384).contains(size))
                .map(|size| size as u32)
        };
        next.source_width = encoded_size("width").or(source.width);
        next.source_height = encoded_size("height").or(source.height);
        next.source_scale = source.scale;
        next.crop_json = source
            .content_rect
            .and_then(|rect| serde_json::to_string(&rect).ok());
        next.geometry_revision = status.geometry_revision.ok_or(())?;
        if next.source_id != status.source_id
            || !matches!(next.source_kind.as_deref(), Some("window" | "display"))
        {
            return Err(());
        }
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let expires = (Utc::now() + chrono::Duration::seconds(CONTROL_EXPIRY_SECONDS))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        state
            .store
            .rebind_computer_session_source(
                &session,
                &next,
                request.lease_id.as_deref(),
                &now,
                &expires,
            )
            .await
            .map_err(|_| ())?
            .ok_or(())
    }
    .await;
    {
        let mut media = handle.signaling.lock().await;
        if let Ok((updated, _)) = &rebound {
            media.source_binding_revision = updated.geometry_revision;
            media.capture_state = "capturing".into();
        }
        media.source_rebinding = false;
    }
    match rebound {
        Ok((updated, lease)) => {
            publish_session_event(&state, &updated).await;
            Json(SourceRebindResponse {
                accepted: true,
                reason: None,
                session: response(&state, updated),
                lease: lease.map(lease_response),
            })
            .into_response()
        }
        Err(()) => {
            // A timeout may follow native delivery. Never replay it or let the
            // previous view's lease remain usable against the new source.
            fail_session(&state, &session, "source_rebind_failed").await;
            state
                .computer_supervisor
                .stop(&state, &id, "source_rebind_failed")
                .await;
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The window could not be changed safely. Reopen computer view and try again.",
            )
                .into_response()
        }
    }
}

fn lease_binding_for_session(
    lease: &StoredComputerControlLease,
    session: &StoredComputerSession,
) -> bool {
    lease.owner_device_id == session.owner_device_id
        && lease.host_installation_id == session.host_installation_id
        && lease.session_id == session.id
        && lease.conversation_id == session.conversation_id
        && lease.session_generation == session.generation
        && lease.source_id == session.source_id
        && lease.geometry_revision == session.geometry_revision
}

pub(crate) async fn admit(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<AdmissionRequest>,
) -> Response {
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    if !matches!(request.role.as_str(), "viewer" | "publisher") {
        return (StatusCode::BAD_REQUEST, "computer session role is invalid").into_response();
    }
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if request.generation != session.generation {
        return (StatusCode::CONFLICT, "computer session generation mismatch").into_response();
    }
    let session_response = response(&state, session);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(AdmissionResponse {
            session: session_response,
            role: request.role,
            granted: false,
            admission: None,
            capability: capability(&state),
        }),
    )
        .into_response()
}

pub(crate) async fn acquire_control(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<AcquireControlRequest>,
) -> Response {
    let control = state.computer_supervisor.session_control(&id);
    let mut acquire_guard = control.lock().await;
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    if !validate_text(&request.client_request_id, 160)
        || request
            .source_id
            .as_deref()
            .is_some_and(|value| !validate_text(value, 160))
    {
        return (StatusCode::BAD_REQUEST, "control lease request is invalid").into_response();
    }
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !session_binding_matches(
        &session,
        request.generation,
        request.geometry_revision,
        request.source_id.as_deref(),
    ) {
        return (
            StatusCode::CONFLICT,
            Json(control_response(
                &state,
                false,
                false,
                "stale",
                "The computer view changed. Refresh before taking control.",
                None,
                None,
            )),
        )
            .into_response();
    }
    if let Some(takeover_id) = &request.takeover_request_id {
        if !state
            .computer_supervisor
            .takeover_accepts_control(takeover_id, &session, Utc::now())
        {
            return (
                StatusCode::CONFLICT,
                "This takeover request has ended or belongs to another chat. Open its chat and try again.",
            ).into_response();
        }
    }
    // A retry cannot turn a reservation into activation before its native
    // decision. Other operations, especially end/release, remain available.
    if acquire_guard.pending_consent.is_some() {
        return (
            StatusCode::CONFLICT,
            "Control is waiting for approval on the computer.",
        )
            .into_response();
    }
    // The durable single-owner index remains authoritative across sessions.
    let mut existing = match state
        .store
        .computer_control_lease_by_request(&device.device_id, &request.client_request_id)
        .await
    {
        Ok(existing) => existing,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "control lease is unavailable",
            )
                .into_response()
        }
    };
    let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    if existing
        .as_ref()
        .is_some_and(|lease| lease.status == "active" && lease.expires_at <= checked_at)
    {
        let expiry_probe = ComputerControlLeaseCreate {
            id: uuid::Uuid::new_v4().to_string(),
            client_request_id: request.client_request_id.clone(),
            session_id: session.id.clone(),
            owner_device_id: device.device_id.clone(),
            host_installation_id: state.host_installation_id.clone(),
            conversation_id: session.conversation_id.clone(),
            session_generation: request.generation,
            source_id: request.source_id.clone(),
            geometry_revision: request.geometry_revision,
            now: checked_at.clone(),
            expires_at: checked_at.clone(),
        };
        existing = match state
            .store
            .acquire_computer_control_lease(&expiry_probe)
            .await
        {
            Ok(ComputerLeaseAcquireResult::Existing(lease)) => Some(lease),
            Ok(_) | Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "control lease could not be reconciled",
                )
                    .into_response()
            }
        };
    }
    if let Some(existing) = existing.as_ref() {
        if !state
            .computer_supervisor
            .takeover_lease_matches(request.takeover_request_id.as_deref(), &existing.id)
        {
            return (
                StatusCode::CONFLICT,
                "This control request already answers a different takeover request.",
            )
                .into_response();
        }
        if !lease_matches_request(existing, &state, &session, &request) {
            return (
                StatusCode::CONFLICT,
                "client request is already bound to another control lease",
            )
                .into_response();
        }
        if existing.status != "active" {
            return (
                StatusCode::CONFLICT,
                Json(control_response(
                    &state,
                    false,
                    false,
                    if existing.status == "expired" {
                        "expired"
                    } else {
                        "released"
                    },
                    "This control request has already ended. Start a new control request.",
                    Some(existing.clone()),
                    None,
                )),
            )
                .into_response();
        }
    }
    if session.state != ComputerSessionState::Live.as_str()
        || !session_control_capability(&state, &session).available
    {
        return control_unavailable(&state, session);
    }
    let handle = match state
        .computer_supervisor
        .signaling_handle(&session.id, &device.device_id)
        .await
    {
        Some(handle) => handle,
        None => return control_unavailable(&state, session),
    };

    let was_existing = existing.is_some();
    let mut lease = if let Some(existing) = existing {
        existing
    } else {
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        // Reserve the single-owner slot for the full local-consent window.
        // Once consent returns, the lease is reduced to the normal heartbeat
        // lifetime before the helper is allowed to activate control.
        let expires_at = (Utc::now()
            + chrono::Duration::seconds(CONTROL_CONSENT_RESERVATION_SECONDS))
        .to_rfc3339_opts(SecondsFormat::Millis, true);
        let lease_request = ComputerControlLeaseCreate {
            id: uuid::Uuid::new_v4().to_string(),
            client_request_id: request.client_request_id.clone(),
            session_id: session.id.clone(),
            owner_device_id: device.device_id.clone(),
            host_installation_id: state.host_installation_id.clone(),
            conversation_id: session.conversation_id.clone(),
            session_generation: request.generation,
            source_id: request.source_id.clone(),
            geometry_revision: request.geometry_revision,
            now,
            expires_at,
        };
        match state
            .store
            .acquire_computer_control_lease(&lease_request)
            .await
        {
            Ok(ComputerLeaseAcquireResult::Granted(lease)) => lease,
            Ok(ComputerLeaseAcquireResult::Existing(lease)) if lease.status == "active" => lease,
            Ok(ComputerLeaseAcquireResult::Existing(lease)) => {
                return (
                    StatusCode::CONFLICT,
                    Json(control_response(
                        &state,
                        false,
                        false,
                        if lease.status == "expired" {
                            "expired"
                        } else {
                            "released"
                        },
                        "This control request has already ended. Start a new control request.",
                        Some(lease),
                        None,
                    )),
                )
                    .into_response()
            }
            Ok(ComputerLeaseAcquireResult::Busy(_)) => {
                return (
                    StatusCode::CONFLICT,
                    Json(control_response(
                        &state,
                        false,
                        false,
                        "busy",
                        "Another device is controlling the computer.",
                        None,
                        None,
                    )),
                )
                    .into_response()
            }
            Ok(ComputerLeaseAcquireResult::Conflict) => {
                return (
                    StatusCode::CONFLICT,
                    Json(control_response(
                        &state,
                        false,
                        false,
                        "rejected",
                        "This control request is already bound to another computer view.",
                        None,
                        None,
                    )),
                )
                    .into_response()
            }
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "control lease could not be saved",
                )
                    .into_response()
            }
        }
    };
    let consent_identity_params = control_identity_params(&session, &lease);
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    if !was_existing {
        acquire_guard.pending_consent = Some(PendingControlConsent {
            lease_id: lease.id.clone(),
            cancel,
        });
    }
    // Consent may wait for the owner for two minutes. Never hold the delivery
    // boundary while waiting: end/release must be able to cancel it promptly.
    drop(acquire_guard);
    let mut control_disabled = false;
    let consent_granted = if was_existing {
        true
    } else {
        let decision = tokio::select! {
            biased;
            _ = cancelled => Err("control request cancelled".into()),
            result = handle.request("control.consent", consent_identity_params) => result,
        };
        match decision {
            Ok(value) => {
                control_disabled = value.get("reason").and_then(serde_json::Value::as_str)
                    == Some("control_disabled");
                value.get("granted").and_then(serde_json::Value::as_bool) == Some(true)
            }
            _ => false,
        }
    };
    let mut acquire_guard = control.lock().await;
    if !was_existing {
        if acquire_guard
            .pending_consent
            .as_ref()
            .map(|pending| pending.lease_id.as_str())
            != Some(lease.id.as_str())
        {
            // End/release won, even if a late consent answer raced cancellation.
            return (
                StatusCode::CONFLICT,
                Json(control_response(
                    &state,
                    false,
                    false,
                    "released",
                    "This control request was cancelled.",
                    state
                        .store
                        .computer_control_lease(&lease.id)
                        .await
                        .ok()
                        .flatten(),
                    None,
                )),
            )
                .into_response();
        }
        acquire_guard.pending_consent.take();
    }
    let activated = if consent_granted {
        let activated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let expires_at = (Utc::now() + chrono::Duration::seconds(CONTROL_EXPIRY_SECONDS))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        match state
            .store
            .heartbeat_computer_control_lease(
                &lease.id,
                &device.device_id,
                &state.host_installation_id,
                &session.id,
                &session.conversation_id,
                request.generation,
                request.source_id.as_deref(),
                request.geometry_revision,
                &activated_at,
                &expires_at,
            )
            .await
        {
            Ok(Some(refreshed)) => {
                lease = refreshed;
                let activation = handle
                    .request(
                        "control.activate",
                        control_identity_params(&session, &lease),
                    )
                    .await;
                if let Ok(value) = &activation {
                    control_disabled |= value.get("reason").and_then(serde_json::Value::as_str)
                        == Some("control_disabled");
                }
                activation
                    .ok()
                    .and_then(|value| value.get("activated").and_then(serde_json::Value::as_bool))
                    == Some(true)
            }
            Ok(None) | Err(_) => false,
        }
    } else {
        false
    };
    if activated {
        if let Some(takeover_id) = &request.takeover_request_id {
            state
                .computer_supervisor
                .activate_takeover(takeover_id, &session, &lease, Utc::now());
        }
        let mut reply = control_response(
            &state,
            true,
            true,
            "active",
            if was_existing {
                "Control already granted."
            } else {
                "Control granted."
            },
            Some(lease),
            None,
        );
        reply.control = session_control_capability(&state, &session);
        return Json(reply).into_response();
    }
    // Consent, activation, and helper loss all roll back the durable
    // reservation. The released request ID is terminal and cannot reprompt.
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let _ = state
        .store
        .release_computer_control_lease(&lease.id, &device.device_id, &now)
        .await;
    let _ = handle
        .request("control.release", control_identity_params(&session, &lease))
        .await;
    (
        StatusCode::CONFLICT,
        Json(control_response(
            &state,
            false,
            true,
            "rejected",
            if control_disabled {
                CONTROL_DISABLED_REASON
            } else {
                "Control was not allowed or is no longer available on the computer."
            },
            state
                .store
                .computer_control_lease(&lease.id)
                .await
                .ok()
                .flatten(),
            None,
        )),
    )
        .into_response()
}

pub(crate) async fn heartbeat_control(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<LeaseBindingRequest>,
) -> Response {
    let control = state.computer_supervisor.session_control(&id);
    let _input_guard = control.lock().await;
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    if !validate_binding_request(&request) {
        return (StatusCode::BAD_REQUEST, "control lease request is invalid").into_response();
    }
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !session_binding_matches(
        &session,
        request.generation,
        request.geometry_revision,
        request.source_id.as_deref(),
    ) {
        return (
            StatusCode::CONFLICT,
            Json(control_response(
                &state,
                false,
                false,
                "stale",
                "The computer view changed. Control was not renewed.",
                None,
                None,
            )),
        )
            .into_response();
    }
    let session_control = session_control_capability(&state, &session);
    match renew_control(&state, &device, session, &request).await {
        Ok(lease) => {
            let mut reply = control_response(
                &state,
                false,
                true,
                "active",
                "Control renewed.",
                Some(lease),
                None,
            );
            reply.control = session_control;
            Json(reply).into_response()
        }
        Err(response) => response,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenAppRequest {
    #[serde(flatten)]
    binding: LeaseBindingRequest,
    bundle_id: String,
}

/// Opens an installed app on the computer. Launching is input, so it needs the
/// phone's current control lease for this view, renewed like a heartbeat.
pub(crate) async fn open_app(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<OpenAppRequest>,
) -> Response {
    let control = state.computer_supervisor.session_control(&id);
    let _input_guard = control.lock().await;
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    if !validate_binding_request(&request.binding)
        || !crate::computer_apps::valid_bundle_id(&request.bundle_id)
    {
        return (StatusCode::BAD_REQUEST, "open app request is invalid").into_response();
    }
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !session_binding_matches(
        &session,
        request.binding.generation,
        request.binding.geometry_revision,
        request.binding.source_id.as_deref(),
    ) {
        return (
            StatusCode::CONFLICT,
            Json(control_response(
                &state,
                false,
                false,
                "stale",
                "The computer view changed. The app was not opened.",
                None,
                None,
            )),
        )
            .into_response();
    }
    // Opening an app launches it on the Mac, never inside a simulator.
    if session.source_kind.as_deref() == Some("simulator") {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "opened": false,
                "reason": "Open apps from the simulator’s Home Screen.",
            })),
        )
            .into_response();
    }
    let apps = crate::computer_apps::catalog().await;
    let helper = state.computer_use_bin.clone();
    let catalog_app = apps.iter().find(|app| app.id == request.bundle_id).cloned();
    // Linux and Windows apps are known from the dock, not a folder scan.
    let dock_app = if catalog_app.is_none() {
        crate::computer_apps::dock(helper.as_deref())
            .await
            .find(&request.bundle_id)
            .map(|app| app.id.clone())
    } else {
        None
    };
    if catalog_app.is_none() && dock_app.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "opened": false,
                "reason": "That app is no longer installed on this computer.",
            })),
        )
            .into_response();
    };
    let lease = match renew_control(&state, &device, session, &request.binding).await {
        Ok(lease) => lease,
        Err(response) => return response,
    };
    let launched = tokio::task::spawn_blocking(move || match (catalog_app, dock_app) {
        (Some(app), _) => crate::computer_apps::launch(&app),
        (None, Some(id)) => crate::computer_apps::launch_dock_app(helper.as_deref(), &id),
        (None, None) => Err(std::io::Error::other("unknown app")),
    })
    .await;
    match launched {
        Ok(Ok(())) => Json(serde_json::json!({
            "opened": true,
            "lease": lease_response(lease),
        }))
        .into_response(),
        _ => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "opened": false,
                "reason": "The computer could not open that app.",
            })),
        )
            .into_response(),
    }
}

/// Renews the phone's control lease with the store and the helper, or says why
/// control ended. Launching an app proves control the same way as a heartbeat.
#[allow(clippy::result_large_err)]
async fn renew_control(
    state: &AppState,
    device: &AuthenticatedDevice,
    session: StoredComputerSession,
    request: &LeaseBindingRequest,
) -> Result<StoredComputerControlLease, Response> {
    if !session_control_capability(state, &session).available
        || session.state != ComputerSessionState::Live.as_str()
    {
        return Err(control_unavailable(state, session));
    }
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let expires_at = (Utc::now() + chrono::Duration::seconds(CONTROL_EXPIRY_SECONDS))
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    match state
        .store
        .heartbeat_computer_control_lease(
            &request.lease_id,
            &device.device_id,
            &state.host_installation_id,
            &session.id,
            &session.conversation_id,
            request.generation,
            request.source_id.as_deref(),
            request.geometry_revision,
            &now,
            &expires_at,
        )
        .await
    {
        Ok(Some(lease)) => {
            let Some(handle) = state
                .computer_supervisor
                .signaling_handle(&session.id, &device.device_id)
                .await
            else {
                let _ = state
                    .store
                    .release_computer_control_lease(&lease.id, &device.device_id, &now)
                    .await;
                return Err(control_unavailable(state, session));
            };
            let helper_response = handle
                .request(
                    "control.heartbeat",
                    control_identity_params(&session, &lease),
                )
                .await
                .ok();
            let helper_disabled = helper_response
                .as_ref()
                .and_then(|value| value.get("reason"))
                .and_then(serde_json::Value::as_str)
                == Some("control_disabled");
            let helper_ok = helper_response
                .as_ref()
                .and_then(|value| value.get("renewed").and_then(serde_json::Value::as_bool))
                == Some(true);
            if !helper_ok {
                let _ = state
                    .store
                    .release_computer_control_lease(&lease.id, &device.device_id, &now)
                    .await;
                return Err((
                    StatusCode::CONFLICT,
                    Json(control_response(
                        state,
                        false,
                        false,
                        "expired",
                        if helper_disabled {
                            CONTROL_DISABLED_REASON
                        } else {
                            "Control expired or is no longer available on the computer."
                        },
                        state
                            .store
                            .computer_control_lease(&lease.id)
                            .await
                            .ok()
                            .flatten(),
                        None,
                    )),
                )
                    .into_response());
            }
            Ok(lease)
        }
        Ok(None) => Err((
            StatusCode::CONFLICT,
            Json(control_response(
                state,
                false,
                false,
                "expired",
                "Control expired or no longer matches this computer view.",
                None,
                None,
            )),
        )
            .into_response()),
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "control lease unavailable",
        )
            .into_response()),
    }
}

/// A copy from the computer that returns no text is still delivered input, so
/// control stays. Mac, Linux and Windows helpers report these reasons.
fn clipboard_copy_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("clipboard_permission_denied") => "Your Mac blocked clipboard access. In System Settings → Privacy & Security → Paste from Other Apps, allow Cupola, then copy again.",
        Some("clipboard_permission_prompt") => "Your Mac is asking whether Cupola may paste. Choose Allow Paste on the Mac, then tap Copy again.",
        Some("clipboard_sensitive") => "That copy is marked private, such as a password, so it stays on the Mac.",
        Some("clipboard_no_text") => "The clipboard on the computer has no text to copy.",
        Some("clipboard_too_large") => "That copy is longer than 8,192 characters. Copy a shorter selection on the computer.",
        _ => "The computer did not return clipboard text. Try again.",
    }
}

/// Why an input batch was refused, in words the phone shows as it releases
/// control. Codes come from the Mac, Linux and Windows helpers, the lease
/// store and `input_refusal_code`.
fn input_failure_reason(reason: &str) -> &'static str {
    match reason {
        "window_covered" => "Another window is covering this one. Bring the selected window forward on the computer, then take control again.",
        "window_not_focused" => "This window could not be brought forward. Select it on the computer, then take control again.",
        "source_not_found" | "window_not_found" => "That window has closed. Choose another window from the computer menu.",
        "accessibility_permission_required" | "accessibility_required" => "On your Mac, allow Cupola in System Settings → Privacy & Security → Accessibility, then reopen this view.",
        "input_blocked" => "Windows is showing a protected screen. Finish that step on the computer, then take control again.",
        "input_desktop_unavailable" => "Windows is showing a protected screen, such as a sign-in or permission prompt. Finish that step on the computer, then take control again.",
        "xdotool_required" => "This computer needs xdotool installed before it can be controlled. Install it on the computer, then take control again.",
        "xclip_required" => "This computer needs xclip installed before it can copy or paste. Install it on the computer, then take control again.",
        "display_unavailable" => "The computer’s desktop could not be reached. Check that its desktop session is running, then take control again.",
        "clipboard_too_large" => "That copy is too long to send to the phone. Copy a shorter selection on the computer, then take control again.",
        "control_disabled" => CONTROL_DISABLED_REASON,
        "text_unsupported_characters" => SIMULATOR_TEXT_REASON,
        "simulator_button_unsupported" => "This simulator doesn’t have that button.",
        "simulator_action_unsupported" => "That input doesn’t work in a simulator. Tap, swipe or type instead.",
        "simulator_not_booted" => "This simulator has stopped. Start it again, then take control.",
        "local_stop" => "Control was stopped on the computer. Take control again to continue.",
        "lease_expired" => "Control timed out after a pause in the connection. Take control again to continue.",
        "lease_not_active" | "lease_released" => "Control ended on the computer. Take control again to continue.",
        "sequence_payload_conflict" | "input_sequence_must_increase_by_one" | "input_out_of_order" => "Some input arrived out of order, so it was not sent. Check the computer, then take control again.",
        "capture_not_live" => "This window is hidden or closed on the computer. Show it there or choose another window, then take control again.",
        "view_changed" | "invalid_input" | "capture_replaced" | "source_replaced" | "source_geometry_changed" => "The computer view changed, so this input was not sent. Take control again.",
        "input_unavailable" => "This view can’t be controlled. Choose a window or the whole screen, then take control again.",
        "input_delivery_failed" | "input_failed" => "The computer did not accept this input. Check what reached it, then take control again.",
        "helper_unavailable" | "input_timed_out" | "request_timed_out" => "The computer stopped responding, so control was released. Check what reached it, then take control again.",
        "lease_unavailable" => "Control could not be checked on the computer. Take control again.",
        _ => "Input stopped. Check what reached the computer before taking control again.",
    }
}

/// Turns a refusal into the code `input_failure_reason` explains. The store
/// reports a lease that no longer matches as one phrase; tell apart a lease
/// the computer ended (and why), one that timed out, and a changed view.
async fn input_refusal_code(
    state: &AppState,
    handle: &SignalingHandle,
    lease_id: &str,
    store_reason: &str,
) -> String {
    match store_reason {
        "lease binding is stale" | "lease not found" | "lease ended" | "lease_not_active" => {}
        "computer session is stale" | "session not found" => return "view_changed".into(),
        "input sequence must increase by one" | "input sequence was not accepted" => {
            return "input_out_of_order".into()
        }
        "lease unavailable" | "session unavailable" => return "lease_unavailable".into(),
        // Helper transport failures are plain sentences, not codes.
        reason if reason.starts_with("computer helper") || reason.starts_with("helper") => {
            return "helper_unavailable".into()
        }
        reason => return reason.into(),
    }
    let revoked = handle.signaling.lock().await.revoked_lease.clone();
    if let Some((revoked_id, reason)) = revoked {
        if revoked_id == lease_id {
            return reason;
        }
    }
    if store_reason == "lease_not_active" {
        return store_reason.into();
    }
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    match state.store.computer_control_lease(lease_id).await {
        Ok(Some(lease)) if lease.status == "expired" => "lease_expired".into(),
        Ok(Some(lease))
            if lease.status == "active" && lease.expires_at.as_str() <= now.as_str() =>
        {
            "lease_expired".into()
        }
        Ok(Some(lease)) if lease.status == "active" => "view_changed".into(),
        Ok(_) => "lease_released".into(),
        Err(_) => "lease_unavailable".into(),
    }
}

/// Plain words for why a computer view stopped. Sessions keep the helper's
/// code for diagnostics; every reply to the phone goes through here, so a
/// code such as ScreenCaptureKit's `-3805` never reaches the screen.
fn failure_words(reason: &str, source_kind: Option<&str>) -> String {
    if reason.contains(' ') {
        // Already a sentence written for the phone.
        return reason.to_owned();
    }
    let words = match reason {
        "source_removed" if source_kind == Some("simulator") => "This simulator has stopped.",
        "simulator_unavailable" => "This simulator is unavailable.",
        "source_removed" | "source_not_found" | "window_not_found" | "source_unavailable"
        | "source_inactive" | "capture_stream_stopped" => {
            "The shared window closed. Choose another window or the whole screen."
        }
        "screen_recording_permission_required" | "permission_denied" => {
            "On your computer, allow Cupola in System Settings → Privacy & Security → Screen & System Audio Recording, then try again."
        }
        "wayland_unsupported" => {
            "Computer view needs an X11 session, and this computer is signed in to Wayland. Sign out on the computer, choose an X11 session (on Ubuntu, Ubuntu on Xorg), then reopen Computer view. Cupola picks up the new session automatically."
        }
        "x11_display_required" | "display_unavailable" => {
            "Computer view couldn’t reach this computer’s X11 desktop. Check that its desktop session is running, then try again."
        }
        "system_picker_unavailable" | "shareable_content_unavailable" | "no_display"
        | "capture_unavailable" => "This computer has no screen available to share right now. Try again.",
        "peer_connection_disconnected" | "peer_connection_failed" | "peer_connection_closed"
        | "peer_disconnected" | "peer_closed" | "disconnected" | "closed" | "failed" => {
            "The connection to the computer dropped. Try again."
        }
        // The video connection was never set up.
        "peer_setup_failed" | "peer_setup_timeout" | "offer_failed" | "vp8_unavailable"
        | "peer_creation_failed" | "remote_description_failed" | "local_description_failed"
        | "candidate_rejected" | "sdp_too_large" => {
            "The video connection to the computer couldn’t be set up. Try again."
        }
        // The helper stopped answering, whether or not sharing had begun.
        "helper_exited" | "helper_output_closed" | "helper_read_failed" | "helper_write_failed"
        | "helper_invalid_json" | "helper_line_too_large" | "helper_protocol_error"
        | "helper_state_update_failed" | "helper_stopped" => {
            "Cupola’s screen sharing helper on the computer stopped. Try again."
        }
        // Sharing never started.
        "helper_startup_timeout" | "computer_provider_failed_to_start" | "capture_not_prepared"
        | "invalid_capture_state" | "source_not_selected" => {
            "The computer couldn’t start sharing its screen. Try again; if it keeps happening, restart Cupola on the computer."
        }
        "first_frame_timeout" if source_kind == Some("simulator") => {
            "This simulator didn’t show a picture in time. Try again."
        }
        "first_frame_timeout" => "The computer didn’t show a picture in time. Try again.",
        "viewer_timeout" => "The computer view closed while this device was away. Try again.",
        "source_rebind_failed" => "The window could not be changed. Try again.",
        reason if reason.starts_with("stream_error_") => stream_error_words(reason),
        _ => "The computer stopped sharing its screen. Try again.",
    };
    words.to_owned()
}

/// The Mac helper reports ScreenCaptureKit failures as
/// `stream_error_<domain>_<code>` (SCError.h).
fn stream_error_words(reason: &str) -> &'static str {
    let code = reason
        .strip_prefix("stream_error_com.apple.ScreenCaptureKit.SCStreamErrorDomain_")
        .and_then(|code| code.parse::<i64>().ok());
    match code {
        // User declined, missing entitlements.
        Some(-3801) | Some(-3803) => {
            "On your computer, allow Cupola in System Settings → Privacy & Security → Screen & System Audio Recording, then try again."
        }
        // No window list, no display list, no capture source.
        Some(-3815..=-3813) => "The shared window or display is no longer available. Choose another window or the whole screen.",
        Some(-3817) => "Sharing was stopped on your computer. Try again to share it.",
        // Includes -3805: the Mac's screen capture service interrupted the stream.
        _ => "Your computer interrupted screen sharing. Try again.",
    }
}

/// Diagnostics-only stage timer for `input_control`; prints one line per accepted batch.
#[cfg(feature = "diagnostics-input-timing")]
struct InputTiming {
    started: std::time::Instant,
    last: std::time::Instant,
    stages: Vec<(&'static str, u128)>,
    completed: bool,
}

#[cfg(feature = "diagnostics-input-timing")]
impl InputTiming {
    fn start() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            last: now,
            stages: Vec::new(),
            completed: false,
        }
    }

    fn stage(mut self, name: &'static str) -> Self {
        let now = std::time::Instant::now();
        self.stages
            .push((name, now.duration_since(self.last).as_micros()));
        self.last = now;
        self
    }

    fn finish(mut self) {
        self.completed = true;
    }
}

#[cfg(feature = "diagnostics-input-timing")]
impl Drop for InputTiming {
    /// Prints on every exit; early returns show `exit=<last finished stage>`.
    fn drop(&mut self) {
        let stages: Vec<String> = self
            .stages
            .iter()
            .map(|(n, us)| format!("{n}_us={us}"))
            .collect();
        let exit = if self.completed {
            "ok"
        } else {
            self.stages.last().map_or("early", |(n, _)| n)
        };
        eprintln!(
            "input_control_timing exit={exit} total_us={} {}",
            self.started.elapsed().as_micros(),
            stages.join(" ")
        );
    }
}

pub(crate) async fn input_control(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<InputBatchRequest>,
) -> Response {
    #[cfg(feature = "diagnostics-input-timing")]
    let timing = InputTiming::start();
    let control = state.computer_supervisor.session_control(&id);
    let batches = control.lock().await.input_batches.clone();
    let _batch = batches.lock().await;
    let input_guard = control.lock().await;
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    if let Err(message) = validate_input_batch(&request) {
        return (StatusCode::BAD_REQUEST, message).into_response();
    }
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if !session_binding_matches(
        &session,
        request.generation,
        request.geometry_revision,
        request.source_id.as_deref(),
    ) {
        return (
            StatusCode::CONFLICT,
            Json(control_response(
                &state,
                false,
                false,
                "stale",
                "The computer view changed. Input was discarded.",
                None,
                None,
            )),
        )
            .into_response();
    }
    if request
        .actions
        .iter()
        .any(|action| matches!(action, InputAction::ReleaseAll))
    {
        // Release-all is deliberately a separate authenticated release path. It
        // never waits behind or replays an uncertain input batch.
        return (
            StatusCode::BAD_REQUEST,
            "release-all must use the release endpoint",
        )
            .into_response();
    }
    if !session_control_capability(&state, &session).available
        || session.state != ComputerSessionState::Live.as_str()
    {
        return control_unavailable(&state, session);
    }
    if !actions_fit_source(session.source_kind.as_deref(), &request.actions) {
        return (
            StatusCode::BAD_REQUEST,
            "input batch does not fit this view",
        )
            .into_response();
    }
    if session.source_kind.as_deref() == Some("simulator") && !simulator_typeable(&request.actions)
    {
        let mut reply = control_response(
            &state,
            false,
            false,
            "unsupported",
            SIMULATOR_TEXT_REASON,
            None,
            None,
        );
        reply.control = session_control_capability(&state, &session);
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(reply)).into_response();
    }
    let Some(handle) = state
        .computer_supervisor
        .signaling_handle(&session.id, &device.device_id)
        .await
    else {
        return control_unavailable(&state, session);
    };
    #[cfg(feature = "diagnostics-input-timing")]
    let timing = timing.stage("validate");
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let lease = match state
        .store
        .prepare_computer_input_batch(
            &request.lease_id,
            &device.device_id,
            &state.host_installation_id,
            &session.id,
            &session.conversation_id,
            request.generation,
            request.source_id.as_deref(),
            request.geometry_revision,
            request.sequence,
            &now,
        )
        .await
    {
        Ok(lease) => lease,
        Err(reason) => {
            let code = input_refusal_code(&state, &handle, &request.lease_id, reason).await;
            return (
                StatusCode::CONFLICT,
                Json(control_response(
                    &state,
                    false,
                    false,
                    "rejected",
                    input_failure_reason(&code),
                    None,
                    None,
                )),
            )
                .into_response();
        }
    };
    #[cfg(feature = "diagnostics-input-timing")]
    let timing = timing.stage("prepare_write");
    let mut params = control_identity_params(&session, &lease);
    if let Some(object) = params.as_object_mut() {
        object.insert("sequence".into(), serde_json::json!(request.sequence));
        object.insert(
            "actions".into(),
            serde_json::to_value(&request.actions).unwrap_or(serde_json::Value::Null),
        );
    }
    // Input execution is serialized above, independently of heartbeat,
    // release and source changes. Those can invalidate the helper generation.
    drop(input_guard);
    let delivered = handle.request("control.input", params).await;
    #[cfg(feature = "diagnostics-input-timing")]
    let timing = timing.stage("helper_round_trip");
    let _input_guard = control.lock().await;
    let helper_result = match delivered {
        Ok(value) => value,
        Err(reason) => {
            let code = input_refusal_code(&state, &handle, &request.lease_id, &reason).await;
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(control_response(
                    &state,
                    false,
                    false,
                    "unavailable",
                    input_failure_reason(&code),
                    None,
                    None,
                )),
            )
                .into_response();
        }
    };
    if helper_result
        .get("accepted")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        let reason = helper_result
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("input_delivery_failed");
        let code = input_refusal_code(&state, &handle, &request.lease_id, reason).await;
        return (
            StatusCode::CONFLICT,
            Json(control_response(
                &state,
                false,
                false,
                "rejected",
                input_failure_reason(&code),
                None,
                None,
            )),
        )
            .into_response();
    }
    let completed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let refreshed_lease = match state
        .store
        .advance_computer_input_batch(
            &request.lease_id,
            &device.device_id,
            &state.host_installation_id,
            &session.id,
            &session.conversation_id,
            request.generation,
            request.source_id.as_deref(),
            request.geometry_revision,
            request.sequence,
            &completed_at,
        )
        .await
    {
        Ok(lease) => lease,
        Err(_reason) => {
            let recovery = accepted_input_recovery();
            let _ = state
                .store
                .release_computer_control_lease(&lease.id, &device.device_id, &completed_at)
                .await;
            let _ = handle
                .request("control.release", control_identity_params(&session, &lease))
                .await;
            return (
                StatusCode::OK,
                Json(control_response(
                    &state,
                    false,
                    recovery.acknowledged,
                    recovery.status,
                    recovery.reason,
                    None,
                    None,
                )),
            )
                .into_response();
        }
    };
    #[cfg(feature = "diagnostics-input-timing")]
    timing.stage("advance_write").finish();
    let clipboard_text = helper_result
        .get("clipboardText")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let copy_requested = request.actions.iter().any(|action| {
        matches!(action, InputAction::Clipboard { operation, .. } if operation == "copyToPhone")
    });
    let reason = if copy_requested && clipboard_text.is_none() {
        clipboard_copy_reason(
            helper_result
                .get("clipboardReason")
                .and_then(serde_json::Value::as_str),
        )
    } else {
        "Input accepted."
    };
    let mut reply = control_response(
        &state,
        false,
        true,
        "active",
        reason,
        Some(refreshed_lease),
        clipboard_text,
    );
    reply.control = session_control_capability(&state, &session);
    Json(reply).into_response()
}

pub(crate) async fn release_control(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<LeaseBindingRequest>,
) -> Response {
    let control = state.computer_supervisor.session_control(&id);
    let mut _input_guard = control.lock().await;
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    if !validate_binding_request(&request) {
        return (
            StatusCode::BAD_REQUEST,
            "control release request is invalid",
        )
            .into_response();
    }
    let session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    let lease = match state.store.computer_control_lease(&request.lease_id).await {
        Ok(Some(lease))
            if lease.owner_device_id == device.device_id
                && lease.host_installation_id == state.host_installation_id
                && lease.session_id == session.id
                && lease.conversation_id == session.conversation_id
                && lease.session_generation == request.generation
                && lease.geometry_revision == request.geometry_revision
                && lease.source_id.as_deref() == request.source_id.as_deref() =>
        {
            lease
        }
        Ok(Some(_)) | Ok(None) => return StatusCode::NO_CONTENT.into_response(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "control release unavailable",
            )
                .into_response()
        }
    };
    _input_guard.cancel_consent(Some(&lease.id));
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    if state.computer_supervisor.control_capable() {
        if let Some(handle) = state
            .computer_supervisor
            .signaling_handle(&session.id, &device.device_id)
            .await
        {
            let _ = handle
                .request("control.release", control_identity_params(&session, &lease))
                .await;
        }
    }
    match state
        .store
        .release_computer_control_lease(&request.lease_id, &device.device_id, &now)
        .await
    {
        Ok(Some(released)) => {
            // Only this authenticated hand-back route can complete a request.
            // Expired/lost leases and acquisition rollbacks cannot do so.
            state
                .computer_supervisor
                .return_takeover(&lease, Utc::now());
            Json(control_response(
                &state,
                false,
                true,
                "released",
                "Control released. The computer is ready for the Project when it reports a fresh state.",
                Some(released),
                None,
            )).into_response()
        }
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "control release unavailable",
        )
            .into_response(),
    }
}

pub(crate) async fn resume_control(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Json(request): Json<LeaseBindingRequest>,
) -> Response {
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    if !validate_binding_request(&request) {
        return (StatusCode::BAD_REQUEST, "control resume request is invalid").into_response();
    }
    let _session = match owned_session(&state, &id, &device.device_id).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    (
        StatusCode::CONFLICT,
        Json(control_response(
            &state,
            false,
            false,
            "unsupported",
            "Project pause/resume is not acknowledged by this runtime, so handback cannot claim success.",
            None,
            None,
        )),
    )
        .into_response()
}

pub(crate) async fn end(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Query(query): Query<GenerationQuery>,
) -> Response {
    let control = state.computer_supervisor.session_control(&id);
    let mut _input_guard = control.lock().await;
    let device = match owner_device(authenticated).await {
        Ok(device) => device,
        Err(response) => return response,
    };
    let current = match state.store.computer_session(&id).await {
        Ok(current) => current,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "computer session unavailable",
            )
                .into_response()
        }
    };
    let Some(current) = current else {
        return StatusCode::NO_CONTENT.into_response();
    };
    if current.owner_device_id != device.device_id
        || current.host_installation_id != state.host_installation_id
    {
        return StatusCode::NO_CONTENT.into_response();
    }
    if current.state == ComputerSessionState::Ended.as_str() {
        let _ = state
            .store
            .release_computer_control_lease_for_session(
                &current.id,
                &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            )
            .await;
        return StatusCode::NO_CONTENT.into_response();
    }
    if query
        .generation
        .is_some_and(|generation| generation != current.generation)
    {
        return (StatusCode::CONFLICT, "computer session generation mismatch").into_response();
    }
    _input_guard.cancel_consent(None);
    let ended = match state
        .store
        .end_computer_session(
            &id,
            &device.device_id,
            query.generation,
            &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        )
        .await
    {
        Ok(Some(session)) => session,
        Ok(None) => return StatusCode::NO_CONTENT.into_response(),
        Err(_) => {
            return (StatusCode::CONFLICT, "computer session generation mismatch").into_response()
        }
    };
    let _ = state
        .store
        .release_computer_control_lease_for_session(
            &ended.id,
            &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        )
        .await;
    state
        .computer_supervisor
        .stop(&state, &ended.id, "session_deleted")
        .await;
    if current.state != ComputerSessionState::Ended.as_str() {
        let _ = publish_event_with_context(
            &state,
            WonderEvent::ComputerSessionChanged {
                session_id: ended.id.clone(),
                generation: ended.generation,
                state: ended.state.clone(),
            },
            EventContext {
                conversation_id: Some(ended.conversation_id.clone()),
                ..EventContext::default()
            },
        )
        .await;
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_timeline_keeps_the_first_time_of_each_hop_and_completes_on_link_and_frame() {
        let mut timeline = OpenTimeline::default();
        let at = |ms| Duration::from_millis(ms);
        let capabilities = serde_json::json!({"id": 1, "result": {}});
        let selected = serde_json::json!({"event": "capture.sourceSelected"});
        let connecting = serde_json::json!({"event": "signal.peerState", "state": "checking"});
        let connected = serde_json::json!({"event": "signal.peerState", "state": "connected"});
        let frame = serde_json::json!({"event": "capture.frameMetadata"});
        assert!(!timeline.observe(&capabilities, at(400)));
        assert!(!timeline.observe(&selected, at(280)));
        assert!(!timeline.observe(&connecting, at(300)));
        // A frame alone is not an opened view; the link must be up too.
        assert!(!timeline.observe(&frame, at(320)));
        assert!(timeline.observe(&connected, at(360)));
        // Later frames neither move the recorded first time nor log again.
        assert!(!timeline.observe(&frame, at(1320)));
        assert_eq!(
            timeline.hops,
            BTreeMap::from([
                ("capabilities", 400),
                ("sourceSelected", 280),
                ("firstFrame", 320),
                ("peerConnected", 360),
            ])
        );
    }

    #[tokio::test]
    async fn codec_summary_ignores_content_and_overlong_diagnostics() {
        let mut bytes =
            Vec::from(&b"private title and input\nWONDER_COMPUTER_CODEC video/H264\n"[..]);
        bytes.extend(vec![b'x'; 4096]);
        bytes.extend(b"WONDER_COMPUTER_CODEC video/VP8\n");
        bytes.extend(b"WONDER_COMPUTER_CODEC video/VP8 extra\n");
        assert_eq!(read_codec_summary(&mut bytes.as_slice()).await, None);
        bytes.extend(b"WONDER_COMPUTER_CODEC video/VP8\n");
        assert_eq!(read_codec_summary(&mut bytes.as_slice()).await, Some("VP8"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_helper_that_cannot_join_its_job_is_refused_and_ended() {
        use std::sync::atomic::AtomicU32;
        use tokio::io::AsyncWriteExt;
        static REFUSED_PID: AtomicU32 = AtomicU32::new(0);
        fn refuse(child: &tokio::process::Child) -> std::io::Result<wonder_app_server::job::Job> {
            REFUSED_PID.store(child.id().unwrap_or(0), Ordering::SeqCst);
            Err(std::io::Error::other("assignment refused"))
        }
        let shell = PathBuf::from(std::env::var("ComSpec").unwrap());
        let refused = launch_owned_helper(shell.clone(), "handshake", refuse).await;
        assert!(matches!(refused, Err(SupervisorStartError::Spawn)));
        let pid = REFUSED_PID.load(Ordering::SeqCst);
        assert_ne!(pid, 0);
        // SAFETY: queries a process ID; the handle is closed before returning.
        let still_running = unsafe {
            use windows_sys::Win32::System::Threading::{
                OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
            };
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            !process.is_null() && {
                let running = WaitForSingleObject(process, 0) != 0;
                windows_sys::Win32::Foundation::CloseHandle(process);
                running
            }
        };
        assert!(!still_running, "refused helper kept running");

        // An owned helper resumes and runs: the shell answers its stdin.
        let Ok(mut owned) = launch_owned_helper(shell, "handshake", own_helper).await else {
            panic!("owned helper refused");
        };
        owned.stdin.write_all(b"exit 3\r\n").await.unwrap();
        let status = observe_helper(owned.child.wait()).await.unwrap();
        assert_eq!(status.code(), Some(3));
    }

    /// Drive real child/SQLite I/O without letting Tokio's paused clock skip
    /// ahead to a protocol deadline while the OS has not scheduled the fixture.
    /// The real-time bound is only a hang guard for a busy Mac.
    pub(super) async fn observe_helper<F: std::future::Future>(future: F) -> F::Output {
        observe_helper_within(future, Duration::from_secs(60)).await
    }

    async fn observe_helper_within<F: std::future::Future>(
        future: F,
        limit: Duration,
    ) -> F::Output {
        tokio::pin!(future);
        let deadline = std::time::Instant::now() + limit;
        loop {
            tokio::select! {
                biased;
                result = &mut future => return result,
                _ = tokio::task::yield_now() => {
                    assert!(std::time::Instant::now() < deadline, "helper fixture did not reach its observed state");
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    async fn discovery_request_received<F: std::future::Future>(
        mut poll: std::pin::Pin<&mut F>,
        directory: &FsPath,
        calls: usize,
    ) {
        observe_helper(async {
            loop {
                tokio::select! {
                    _ = poll.as_mut() => panic!("hung discovery request completed before cancellation or timeout"),
                    _ = tokio::task::yield_now() => {
                        if std::fs::read_to_string(directory.join("calls"))
                            .unwrap_or_default().lines().count() == calls
                        {
                            break;
                        }
                    }
                }
            }
        }).await;
    }

    fn window_snapshot() -> serde_json::Value {
        serde_json::json!({"available":true,"sources":[{
            "kind":"window", "id":"app:com.apple.TextEdit#41",
            "appId":"com.apple.TextEdit", "name":"TextEdit — Owned fixture"
        }]})
    }

    #[cfg(unix)]
    fn discovery_helper(directory: &FsPath, snapshot: &serde_json::Value) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let helper = directory.join("discovery-helper");
        std::fs::write(directory.join("snapshot.json"), snapshot.to_string()).unwrap();
        std::fs::write(
            &helper,
            concat!(
                "#!/bin/sh\n",
                "cd \"$(dirname \"$0\")\" || exit 1\n",
                "printf '%s\\n' \"$$\" >> starts\n",
                "reply() {\n",
                "  printf '%s\\n' \"$$\" >> calls\n",
                "  [ ! -e fail ] || exit 1\n",
                "  if [ -e hang ]; then exec sleep 30; fi\n",
                "  cat snapshot.json\n",
                "  printf '\\n'\n",
                "}\n",
                "if [ \"$1\" = --discover-sources ]; then\n",
                "  while IFS= read -r request; do printf '%s\\n' \"$request\" >> queries; reply; done\n",
                "else reply; fi\n",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        helper
    }

    fn starts(directory: &FsPath) -> Vec<i32> {
        std::fs::read_to_string(directory.join("starts"))
            .unwrap_or_default()
            .lines()
            .map(|pid| pid.parse().unwrap())
            .collect()
    }

    /// An unused discovery worker exits after `SOURCE_DISCOVERY_IDLE`, timed
    /// from its last query, and the next query starts a fresh one.
    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_worker_retires_when_idle_and_restarts_on_demand() {
        let directory = tempfile::tempdir().unwrap();
        let helper = discovery_helper(directory.path(), &window_snapshot());
        let discovery = SourceDiscovery::default();
        let apps = vec!["com.apple.TextEdit".to_owned()];
        tokio::time::pause();
        assert!(observe_helper(discovery.poll_sources(&helper, &apps, None))
            .await
            .is_some());
        let pid = starts(directory.path())[0];
        let alive = || unsafe { libc::kill(pid, 0) } == 0;
        let settle = || async {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::advance(SOURCE_DISCOVERY_IDLE - Duration::from_secs(1)).await;
        assert!(observe_helper(discovery.poll_sources(&helper, &apps, None))
            .await
            .is_some());
        // Past the first launch's deadline, but used one second before it.
        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        assert!(alive(), "a worker in use was retired");
        assert_eq!(starts(directory.path()).len(), 1);
        tokio::time::advance(SOURCE_DISCOVERY_IDLE).await;
        observe_helper(async {
            while alive() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(discovery.helper.lock().await.is_none());
        assert!(observe_helper(discovery.poll_sources(&helper, &apps, None))
            .await
            .is_some());
        assert_eq!(starts(directory.path()).len(), 2);
        discovery.shutdown().await;
    }

    // Contract: helpers never see the host's credentials. The pure table
    // covers each platform's needs; the spawn check proves the real launch
    // path clears what the daemon inherited.
    #[test]
    fn helper_environment_keeps_platform_needs_and_drops_host_authority() {
        let host: Vec<(std::ffi::OsString, std::ffi::OsString)> = [
            "WONDER_LOOPBACK_CAPABILITY",
            "WONDER_PUBLIC_ORIGIN",
            "WONDER_PUBLIC_ORIGIN_FILE",
            "WONDER_LISTEN_ADDR",
            "WONDER_DATA_DIR",
            "WONDER_LOG_DIR",
            "WONDER_LAN_PORT",
            "WONDER_COMPUTER_USE_BIN",
            "TS_AUTHKEY",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GITHUB_TOKEN",
            "DYLD_INSERT_LIBRARIES",
            "HOME",
            "PATH",
            "LC_CTYPE",
            "WONDER_SERVICE_DIR",
            "WONDER_COMPUTER_USE_HANDSHAKE",
            "DISPLAY",
            "XDG_RUNTIME_DIR",
            "SystemRoot",
            "ComSpec",
        ]
        .into_iter()
        .map(|name| (name.into(), "value".into()))
        .collect();
        let kept: Vec<String> = helper_environment(host)
            .into_iter()
            .map(|(name, _)| name.into_string().unwrap())
            .collect();
        let mut expected = vec![
            "HOME",
            "PATH",
            "LC_CTYPE",
            "WONDER_SERVICE_DIR",
            "WONDER_COMPUTER_USE_HANDSHAKE",
        ];
        if cfg!(windows) {
            expected.extend(["SystemRoot", "ComSpec"]);
        } else if !cfg!(target_os = "macos") {
            expected.extend(["DISPLAY", "XDG_RUNTIME_DIR"]);
        }
        assert_eq!(kept, expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_spawns_receive_only_the_helper_environment() {
        let inherited: Vec<String> = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .collect();
        // cargo runs tests with CARGO_* set: names a helper must not see.
        assert!(inherited.iter().any(|name| !helper_may_see(name)));
        let output = helper_process(FsPath::new("/usr/bin/env"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        // Report names only; values stay out of test output.
        let leaked: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name.to_owned()))
            .filter(|name| !helper_may_see(name))
            .collect();
        assert!(leaked.is_empty(), "helper inherited {leaked:?}");
    }

    /// A steady discovery stream must not create one native process per poll.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn discovery_reuses_one_process_across_polls() {
        let directory = tempfile::tempdir().unwrap();
        let logger = logging::JsonlLogger::new(directory.path(), "discovery.jsonl").unwrap();
        let snapshot = window_snapshot();
        let helper = discovery_helper(directory.path(), &snapshot);
        let discovery = SourceDiscovery::default();
        let apps = vec!["com.apple.TextEdit".to_owned()];
        for _ in 0..3 {
            assert_eq!(
                discovery.discover(&helper, &apps, None, &logger).await,
                Some(snapshot.clone())
            );
            discovery
                .entry(SourceDiscoveryKey::new(&helper, &apps, None))
                .await
                .unwrap()
                .lock()
                .await
                .next_poll_at = None;
        }
        let starts = std::fs::read_to_string(directory.path().join("starts")).unwrap();
        assert_eq!(starts.lines().count(), 1);
        let pid: i32 = starts.trim().parse().unwrap();
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "worker stays alive between polls"
        );
        discovery.shutdown().await;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "shutdown reaps the worker"
        );
        assert!(discovery.poll_sources(&helper, &apps, None).await.is_none());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("starts")).unwrap(),
            starts
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn discovery_reaps_and_restarts_on_timeout_cancellation_and_binary_change() {
        let directory = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let logger = logging::JsonlLogger::new(directory.path(), "discovery.jsonl").unwrap();
        let snapshot = window_snapshot();
        let helper = discovery_helper(directory.path(), &snapshot);
        let replacement = discovery_helper(other.path(), &snapshot);
        let discovery = SourceDiscovery::default();
        let apps = vec!["com.apple.TextEdit".to_owned()];
        tokio::time::pause();
        assert_eq!(
            observe_helper(discovery.poll_sources(&helper, &apps, None)).await,
            Some(snapshot.clone())
        );
        let first = discovery
            .helper
            .lock()
            .await
            .as_ref()
            .unwrap()
            .child
            .id()
            .unwrap() as i32;
        assert_eq!(
            observe_helper(discovery.poll_sources(&replacement, &apps, None)).await,
            Some(snapshot.clone())
        );
        assert_eq!(
            unsafe { libc::kill(first, 0) },
            -1,
            "replacement reaps the old binary"
        );

        std::fs::write(other.path().join("hang"), "").unwrap();
        let second = discovery
            .helper
            .lock()
            .await
            .as_ref()
            .unwrap()
            .child
            .id()
            .unwrap() as i32;
        let mut poll = Box::pin(discovery.poll_sources(&replacement, &apps, None));
        discovery_request_received(poll.as_mut(), other.path(), 2).await;
        // Tokio's timer wheel rounds deadlines up to a millisecond. Cross the
        // deadline before polling, so that poll really arms the shutdown timer.
        tokio::time::advance(SOURCE_DISCOVERY_TIMEOUT + Duration::from_millis(1)).await;
        // Poll the expired request once to enter the unchanged one-second
        // graceful shutdown window, then expire that window explicitly.
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                poll.as_mut(),
                cx
            )))
            .await
            .is_pending()
        );
        tokio::time::advance(Duration::from_secs(1) + Duration::from_millis(1)).await;
        assert_eq!(observe_helper(poll).await, None);
        assert_eq!(
            unsafe { libc::kill(second, 0) },
            -1,
            "timeout reaps the hung process"
        );
        std::fs::remove_file(other.path().join("hang")).unwrap();
        assert_eq!(
            observe_helper(discovery.discover(&replacement, &apps, None, &logger)).await,
            Some(snapshot.clone())
        );

        std::fs::write(other.path().join("hang"), "").unwrap();
        let third = discovery
            .helper
            .lock()
            .await
            .as_ref()
            .unwrap()
            .child
            .id()
            .unwrap() as i32;
        let mut poll = Box::pin(discovery.poll_sources(&replacement, &apps, None));
        discovery_request_received(poll.as_mut(), other.path(), 4).await;
        // Cancel while this query owns the helper and awaits its reply, rather
        // than assuming the OS child reached that point within 200 ms.
        drop(poll);
        observe_helper_within(
            async {
                while unsafe { libc::kill(third, 0) } == 0 {
                    tokio::task::yield_now().await;
                }
            },
            Duration::from_secs(60),
        )
        .await;
        std::fs::remove_file(other.path().join("hang")).unwrap();
        assert_eq!(
            observe_helper(discovery.poll_sources(&replacement, &apps, None)).await,
            Some(snapshot)
        );
        observe_helper(discovery.shutdown()).await;
    }

    #[test]
    fn discovery_cap_preserves_validation_and_drops_only_an_omitted_exact_tab_binding() {
        let apps = vec!["com.google.Chrome".to_owned()];
        let sources: Vec<_> = (1..=40)
            .map(|id| {
                serde_json::json!({
                    "kind":"window", "id":format!("app:com.google.Chrome#{id}"),
                    "appId":"com.google.Chrome", "name":format!("Fixture {id}")
                })
            })
            .collect();
        let mut snapshot = serde_json::json!({"available":true,"sources":sources,
            "browserTab":"exact","browserTabSource":"app:com.google.Chrome#40"});
        let answer = validated_discovery_answer(snapshot.clone(), &apps).unwrap();
        assert_eq!(answer["sources"].as_array().unwrap().len(), 32);
        assert_eq!(answer["browserTab"], "notFound");
        assert!(answer.get("browserTabSource").is_none());
        snapshot["sources"][39]["appId"] = serde_json::json!("com.apple.Notes");
        assert!(
            validated_discovery_answer(snapshot, &apps).is_none(),
            "validate even the omitted tail"
        );
    }

    /// An oversized eligible answer is a bounded selection, not an outage.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn discovery_cuts_an_oversized_recent_first_answer() {
        let directory = tempfile::tempdir().unwrap();
        let logger = logging::JsonlLogger::new(directory.path(), "discovery.jsonl").unwrap();
        let sources: Vec<_> = (1..=40)
            .rev()
            .map(|id| {
                serde_json::json!({
                    "kind":"window", "id":format!("app:com.apple.TextEdit#{id}"),
                    "appId":"com.apple.TextEdit", "name":format!("Fixture {id}")
                })
            })
            .collect();
        let snapshot = serde_json::json!({"available":true,"sources":sources});
        let helper = discovery_helper(directory.path(), &snapshot);
        let discovery = SourceDiscovery::default();
        let actual = discovery
            .discover(&helper, &["com.apple.TextEdit".to_owned()], None, &logger)
            .await;
        assert_eq!(
            actual,
            Some(serde_json::json!({"available":true,"sources":&sources[..32]}))
        );
    }

    /// Discovery transport failure must not make a known window disappear.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn all_window_discovery_and_simulators_use_distinct_jsonl_requests() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        state.computer_use_enabled = true;
        state.computer_use_bin = Some(discovery_helper(directory.path(), &window_snapshot()));
        assert_eq!(
            discover_sources(&state, &[ALL_WINDOWS.to_owned()], None)
                .await
                .status(),
            StatusCode::OK
        );
        std::fs::write(
            directory.path().join("snapshot.json"),
            r#"{"available":false,"sources":[]}"#,
        )
        .unwrap();
        assert_eq!(
            discover_sources(&state, &[], None).await.status(),
            StatusCode::OK
        );
        let requests = std::fs::read_to_string(directory.path().join("queries")).unwrap();
        let requests: Vec<serde_json::Value> = requests
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["apps"], serde_json::json!([]));
        assert_eq!(requests[0]["allWindows"], true);
        assert_eq!(requests[1]["apps"], serde_json::json!([]));
        assert_eq!(requests[1]["allWindows"], false);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("starts"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        state.computer_supervisor.source_discovery.shutdown().await;
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn disabled_capture_is_not_offered_even_when_a_helper_is_installed() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        state.computer_use_enabled = false;
        state.computer_use_bin = Some(discovery_helper(directory.path(), &window_snapshot()));
        let answer = discover_sources(&state, &[ALL_WINDOWS.to_owned()], None).await;
        let body = axum::body::to_bytes(answer.into_body(), 16_384)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"available":false,"sources":[]})
        );
        assert!(!directory.path().join("starts").exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn discovery_keeps_windows_through_a_transient_helper_failure() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        let failure = directory.path().join("fail");
        let snapshot = window_snapshot();
        state.computer_use_enabled = true;
        state.computer_use_bin = Some(discovery_helper(directory.path(), &snapshot));
        let apps = vec!["com.apple.TextEdit".to_owned()];
        let first = discover_sources(&state, &apps, None).await;
        assert_eq!(first.status(), StatusCode::OK);
        std::fs::write(&failure, "").unwrap();
        // Let the next five-second UI poll actually exercise the helper.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let retained = discover_sources(&state, &apps, None).await;
        assert_eq!(retained.status(), StatusCode::OK);
        let body = axum::body::to_bytes(retained.into_body(), 16_384)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            snapshot
        );

        std::fs::remove_file(failure).unwrap();
        let closed = serde_json::json!({"available":true,"sources":[]});
        std::fs::write(directory.path().join("snapshot.json"), closed.to_string()).unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        let recovered = discover_sources(&state, &apps, None).await;
        assert_eq!(recovered.status(), StatusCode::OK);
        let body = axum::body::to_bytes(recovered.into_body(), 16_384)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            closed
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("calls"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("starts"))
                .unwrap()
                .lines()
                .count(),
            2,
            "only the failed worker is restarted"
        );
        state.computer_supervisor.source_discovery.shutdown().await;
    }

    /// The retry schedule and stale deadline are independent: cached reads and
    /// repeated failures cannot keep an old window alive indefinitely.
    #[test]
    fn discovery_backoff_expires_snapshots_and_logs_only_health_changes() {
        let directory = tempfile::tempdir().unwrap();
        let logger = logging::JsonlLogger::new(directory.path(), "discovery.jsonl").unwrap();
        let mut entry = SourceDiscoveryEntry::default();
        let start = tokio::time::Instant::now();
        let snapshot = window_snapshot();
        entry.finish_poll(Some(snapshot.clone()), start);
        assert_eq!(entry.answer(start, &logger), Some(snapshot.clone()));
        assert!(!directory.path().join("discovery.jsonl").exists());
        for (seconds, delay) in [(5, 5), (10, 10), (20, 20)] {
            let now = start + Duration::from_secs(seconds);
            entry.finish_poll(None, now);
            assert_eq!(entry.next_poll_at, Some(now + Duration::from_secs(delay)));
            assert_eq!(entry.answer(now, &logger), Some(snapshot.clone()));
        }
        assert_eq!(
            entry.answer(start + Duration::from_secs(29), &logger),
            Some(snapshot)
        );
        assert_eq!(entry.answer(start + Duration::from_secs(30), &logger), None);
        for seconds in [40, 70, 100] {
            let now = start + Duration::from_secs(seconds);
            entry.finish_poll(None, now);
            assert_eq!(entry.next_poll_at, Some(now + Duration::from_secs(30)));
            assert_eq!(entry.answer(now, &logger), None);
        }
        let fresh = serde_json::json!({"available":true,"sources":[]});
        let now = start + Duration::from_secs(130);
        entry.finish_poll(Some(fresh.clone()), now);
        assert_eq!(entry.answer(now, &logger), Some(fresh));
        assert_eq!(entry.failures, 0);
        assert_eq!(entry.next_poll_at, Some(now + Duration::from_secs(5)));
        let records: Vec<serde_json::Value> =
            std::fs::read_to_string(directory.path().join("discovery.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(
            records
                .iter()
                .map(|record| record["state"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["retaining", "unavailable", "recovered"]
        );
    }

    #[tokio::test]
    async fn discovery_cache_is_bounded_and_never_evicts_an_inflight_query() {
        let discovery = SourceDiscovery::default();
        let apps = vec!["com.apple.TextEdit".to_owned()];
        let binary = FsPath::new("fixture-helper");
        let mut held = Vec::new();
        for tab in 0..16 {
            held.push(
                discovery
                    .entry(SourceDiscoveryKey::new(binary, &apps, Some(tab)))
                    .await
                    .unwrap(),
            );
        }
        let extra = SourceDiscoveryKey::new(binary, &apps, Some(16));
        assert!(discovery.entry(extra.clone()).await.is_none());
        drop(held.pop());
        assert!(discovery.entry(extra).await.is_some());
        assert_eq!(discovery.entries.lock().await.len(), 16);
        let original = discovery
            .entry(SourceDiscoveryKey::new(binary, &apps, Some(0)))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&original, &held[0]));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn discovery_coalesces_identical_polls_and_isolates_tab_bindings() {
        let directory = tempfile::tempdir().unwrap();
        let logger = logging::JsonlLogger::new(directory.path(), "discovery.jsonl").unwrap();
        let mut snapshot = window_snapshot();
        snapshot["sources"][0]["id"] = serde_json::json!("app:com.google.Chrome#41");
        snapshot["sources"][0]["appId"] = serde_json::json!("com.google.Chrome");
        snapshot["sources"][0]["name"] = serde_json::json!("Chrome — Owned fixture");
        snapshot["browserTab"] = serde_json::json!("exact");
        snapshot["browserTabSource"] = serde_json::json!("app:com.google.Chrome#41");
        let helper = discovery_helper(directory.path(), &snapshot);
        let discovery = SourceDiscovery::default();
        let apps = vec!["com.google.Chrome".to_owned(), "com.apple.Notes".to_owned()];
        let reversed = vec![apps[1].clone(), apps[0].clone(), apps[0].clone()];
        let (first, duplicate) = tokio::join!(
            discovery.discover(&helper, &apps, Some(7), &logger),
            discovery.discover(&helper, &reversed, Some(7), &logger),
        );
        assert_eq!(first, Some(snapshot.clone()));
        assert_eq!(duplicate, first);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("calls"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let other_tab = serde_json::json!({"available":true,"sources":[],"browserTab":"notFound"});
        std::fs::write(
            directory.path().join("snapshot.json"),
            other_tab.to_string(),
        )
        .unwrap();
        assert_eq!(
            discovery.discover(&helper, &apps, Some(8), &logger).await,
            Some(other_tab)
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("calls"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        // A missing app must not reuse an otherwise fresh snapshot, either.
        std::fs::write(directory.path().join("fail"), "").unwrap();
        assert!(discovery
            .discover(&helper, &["com.apple.Safari".to_owned()], Some(7), &logger)
            .await
            .is_none());
        assert!(discovery
            .discover(&helper, &["com.apple.Safari".to_owned()], Some(7), &logger)
            .await
            .is_none());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("calls"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        let logs = std::fs::read_to_string(directory.path().join("discovery.jsonl")).unwrap();
        assert_eq!(
            logs.lines().count(),
            1,
            "backing-off requests must not add logs"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&logs).unwrap()["state"],
            "unavailable"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn discovery_rejects_invalid_snapshots_and_honors_authoritative_unavailability() {
        let directory = tempfile::tempdir().unwrap();
        let logger = logging::JsonlLogger::new(directory.path(), "discovery.jsonl").unwrap();
        let snapshot = window_snapshot();
        let helper = discovery_helper(directory.path(), &snapshot);
        let discovery = SourceDiscovery::default();
        let apps = vec!["com.apple.TextEdit".to_owned()];
        assert_eq!(
            discovery.discover(&helper, &apps, None, &logger).await,
            Some(snapshot.clone())
        );
        let invalid = serde_json::json!({"available":true,"sources":[{
            "kind":"window", "id":"app:com.apple.Notes#41", "appId":"com.apple.Notes", "name":"Unrequested app"
        }]});
        std::fs::write(directory.path().join("snapshot.json"), invalid.to_string()).unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::time::resume();
        assert_eq!(
            discovery.discover(&helper, &apps, None, &logger).await,
            Some(snapshot)
        );
        let unavailable = serde_json::json!({"available":false,"sources":[]});
        std::fs::write(
            directory.path().join("snapshot.json"),
            unavailable.to_string(),
        )
        .unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::time::resume();
        assert_eq!(
            discovery.discover(&helper, &apps, None, &logger).await,
            Some(unavailable)
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("calls"))
                .unwrap()
                .lines()
                .count(),
            3
        );
    }

    /// The helper's answer for a Claude in Chrome tab may name only a window it
    /// listed, and only when the match is exact.
    #[test]
    fn browser_tab_answers_name_only_a_listed_window() {
        let sources = serde_json::json!([{"kind":"window","id":"app:com.google.Chrome#21220","appId":"com.google.Chrome","name":"Google Chrome — Docs"}]);
        let answer = |extra: serde_json::Value| {
            let mut value = serde_json::json!({"available": true, "sources": sources});
            for (key, field) in extra.as_object().unwrap() {
                value[key] = field.clone();
            }
            valid_browser_tab(&value)
        };
        assert!(answer(serde_json::json!({})));
        assert!(answer(
            serde_json::json!({"browserTab":"exact","browserTabSource":"app:com.google.Chrome#21220"})
        ));
        for state in [
            "ambiguous",
            "notFound",
            "automationNeeded",
            "automationDenied",
            "unavailable",
        ] {
            assert!(
                answer(serde_json::json!({ "browserTab": state })),
                "{state}"
            );
        }
        assert!(!answer(
            serde_json::json!({"browserTab":"exact","browserTabSource":"app:com.google.Chrome#9"})
        ));
        assert!(!answer(serde_json::json!({"browserTab":"exact"})));
        assert!(!answer(
            serde_json::json!({"browserTab":"ambiguous","browserTabSource":"app:com.google.Chrome#21220"})
        ));
        assert!(!answer(serde_json::json!({"browserTab":"focused"})));
        assert!(!answer(
            serde_json::json!({"browserTabSource":"app:com.google.Chrome#21220"})
        ));
    }

    #[test]
    fn discovery_preserves_capture_recovery_without_turning_no_display_into_a_choice() {
        let apps = vec![ALL_WINDOWS.to_owned()];
        for reason in [
            "screen_recording_permission_required",
            "no_display",
            "secure_desktop",
        ] {
            let capture =
                serde_json::json!({"available":false,"reason":reason,"liveQualityChange":true});
            let answer = validated_discovery_answer(
                serde_json::json!({"available":false,"sources":[],"capture":capture}),
                &apps,
            )
            .unwrap();
            assert_eq!(answer["capture"], capture);
            assert_eq!(answer["sources"], serde_json::json!([]));
        }
        for capture in [
            serde_json::json!({"available":true,"reason":"screen_recording_permission_required"}),
            serde_json::json!({"available":false,"reason":"invented"}),
            serde_json::json!({"available":false,"liveQualityChange":"true"}),
        ] {
            assert!(validated_discovery_answer(
                serde_json::json!({"available":false,"sources":[],"capture":capture}),
                &apps
            )
            .is_none());
        }
    }

    #[test]
    fn all_window_discovery_preserves_exact_cross_platform_sources_and_bounds_icons() {
        let apps = vec![ALL_WINDOWS.to_owned()];
        for app in ["com.apple.TextEdit", "notepad.exe", "org.gnome.TextEditor"] {
            let source = serde_json::json!({"kind":"window", "id":format!("app:{app}#41"),
                "appId":app, "name":"Editor — Notes", "appName":"Editor", "windowTitle":"Notes"});
            assert!(valid_discovered_source(&source, &apps));
            let answer = validated_discovery_answer(
                serde_json::json!({"available":true,"sources":[source.clone()]}),
                &apps,
            )
            .unwrap();
            assert_eq!(answer["sources"][0], source);
            let unavailable = validated_discovery_answer(
                serde_json::json!({"available":false,"sources":[source.clone()]}),
                &apps,
            )
            .unwrap();
            assert_eq!(unavailable["sources"], serde_json::json!([]));
            for field in ["appName", "windowTitle", "iconPNG"] {
                let mut invalid = source.clone();
                invalid[field] = serde_json::json!("x".repeat(3073));
                assert!(!valid_discovered_source(&invalid, &apps), "{field}");
            }
        }
        assert!(!valid_source_app("../bad#app"));
        assert!(!valid_source_app(""));
        let mut source = window_snapshot()["sources"][0].clone();
        source["iconPNG"] = serde_json::json!("bm90IGEgcG5n");
        assert!(!valid_discovered_source(&source, &apps));
    }

    #[test]
    fn discovered_windows_require_requested_app_and_exact_positive_id() {
        let apps = vec!["com.apple.TextEdit".to_owned()];
        for (id, app, valid) in [
            ("app:com.apple.TextEdit#41", "com.apple.TextEdit", true),
            ("app:com.apple.TextEdit", "com.apple.TextEdit", false),
            ("app:com.apple.TextEdit#0", "com.apple.TextEdit", false),
            ("app:com.apple.TextEdit#+41", "com.apple.TextEdit", false),
            (
                "app:com.apple.TextEdit#4294967296",
                "com.apple.TextEdit",
                false,
            ),
            ("app:com.apple.Notes#41", "com.apple.TextEdit", false),
            ("app:com.apple.Notes#41", "com.apple.Notes", false),
        ] {
            let source = serde_json::json!({"kind":"window", "id":id, "appId":app, "name":"TextEdit — Rear document"});
            assert_eq!(valid_discovered_source(&source, &apps), valid, "{id} {app}");
            assert!(!valid_discovered_source(&source, &[]));
        }
        let simulator = serde_json::json!({"kind":"simulator", "id":"simulator:00000000-0000-0000-0000-000000000001", "name":"Preview iPhone"});
        assert!(valid_discovered_source(&simulator, &[]));
        assert!(!valid_discovered_source(&simulator, &apps));
    }

    #[test]
    fn video_height_hint_preserves_older_mac_1080p_request() {
        let request: StartRequest = serde_json::from_value(serde_json::json!({
            "clientRequestId": "request",
            "conversationId": "chat",
            "hostInstallationId": "mac",
            "videoQuality": "auto",
            "videoMaxHeight": 1080
        }))
        .unwrap();
        assert_eq!(
            requested_video_quality(request.video_quality.as_deref(), request.video_max_height),
            Ok("medium")
        );

        let invalid: StartRequest = serde_json::from_value(serde_json::json!({
            "clientRequestId": "request",
            "conversationId": "chat",
            "hostInstallationId": "mac",
            "videoQuality": "high",
            "videoMaxHeight": 1080
        }))
        .unwrap();
        assert_eq!(
            requested_video_quality(invalid.video_quality.as_deref(), invalid.video_max_height),
            Err("video quality is invalid")
        );
    }

    #[test]
    fn frame_rate_ceiling_is_15_to_120_and_old_clients_get_15() {
        let old: StartRequest = serde_json::from_value(serde_json::json!({
            "clientRequestId": "request",
            "conversationId": "chat",
            "hostInstallationId": "mac"
        }))
        .unwrap();
        assert_eq!(old.video_max_frames_per_second, None);
        assert_eq!(
            requested_video_fps(old.video_max_frames_per_second, MIN_VIDEO_FPS),
            Ok(15)
        );
        // A quality change that omits it keeps the session's ceiling.
        assert_eq!(requested_video_fps(None, 60), Ok(60));
        for fps in [15, 30, 60, 120] {
            assert_eq!(requested_video_fps(Some(fps), 15), Ok(fps));
        }
        for fps in [0, 14, 121, 240] {
            assert_eq!(
                requested_video_fps(Some(fps), 15),
                Err("video frame rate is invalid")
            );
        }
        let new: StartRequest = serde_json::from_value(serde_json::json!({
            "clientRequestId": "request",
            "conversationId": "chat",
            "hostInstallationId": "mac",
            "videoMaxFramesPerSecond": 60
        }))
        .unwrap();
        assert_eq!(new.video_max_frames_per_second, Some(60));
        let quality: QualityRequest = serde_json::from_value(serde_json::json!({
            "generation": 1,
            "videoQuality": "high",
            "videoMaxFramesPerSecond": 120
        }))
        .unwrap();
        assert_eq!(quality.video_max_frames_per_second, Some(120));

        // The HTTP contract names the same range, and old wires stay valid.
        let start = |fps: Option<u16>| {
            let mut wire = serde_json::json!({
                "clientRequestId": "request", "conversationId": "chat", "hostInstallationId": "mac"
            });
            if let Some(fps) = fps {
                wire["videoMaxFramesPerSecond"] = serde_json::json!(fps);
            }
            wire
        };
        let adjust = |fps: Option<u16>| {
            let mut wire = serde_json::json!({ "generation": 1, "videoQuality": "high" });
            if let Some(fps) = fps {
                wire["videoMaxFramesPerSecond"] = serde_json::json!(fps);
            }
            wire
        };
        // The contract validator lives in the unix-only crate tests.
        #[cfg(unix)]
        for fps in [None, Some(15), Some(30), Some(60), Some(120)] {
            crate::tests::validate_http_contract("startComputerSessionRequest", &start(fps));
            crate::tests::validate_http_contract("computerSessionQualityRequest", &adjust(fps));
        }
        let authority: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/schemas/wonder-http-v1.json"
        ))
        .unwrap();
        for (definition, wire) in [
            (
                "startComputerSessionRequest",
                start as fn(Option<u16>) -> serde_json::Value,
            ),
            (
                "computerSessionQualityRequest",
                adjust as fn(Option<u16>) -> serde_json::Value,
            ),
        ] {
            let validator = jsonschema::validator_for(&serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$ref": format!("#/$defs/{definition}"),
                "$defs": authority["$defs"]
            }))
            .unwrap();
            for fps in [14, 121] {
                assert!(!validator.is_valid(&wire(Some(fps))), "{definition} {fps}");
            }
        }
    }

    #[test]
    fn source_crop_is_bounded_and_requires_a_positive_rectangle() {
        let valid = SourceRequest {
            id: Some("display-1".into()),
            name: Some("Mac display".into()),
            kind: Some("display".into()),
            width: Some(1280),
            height: Some(720),
            scale: Some(2.0),
            crop: Some(CropRequest {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 720.0,
            }),
        };
        assert!(source_fields(Some(valid)).is_ok());

        let invalid = SourceRequest {
            id: None,
            name: None,
            kind: None,
            width: None,
            height: None,
            scale: None,
            crop: Some(CropRequest {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 720.0,
            }),
        };
        assert_eq!(source_fields(Some(invalid)), Err("source crop is invalid"));
    }

    #[test]
    fn capability_is_explicitly_unavailable_until_a_provider_exists() {
        let value = serde_json::to_value(serde_json::json!({
            "available": false,
            "action": UNAVAILABLE_ACTION,
            "reason": UNAVAILABLE_REASON,
        }))
        .unwrap();
        assert_eq!(value["available"], false);
        assert_eq!(value["action"], "update-host");
        assert!(value["reason"].as_str().unwrap().contains("media provider"));
    }

    #[test]
    fn control_contract_rejects_unbounded_actions_and_reserves_release_all() {
        let valid = InputBatchRequest {
            lease_id: "lease-1".into(),
            generation: 1,
            geometry_revision: 2,
            source_id: Some("display-1".into()),
            sequence: 1,
            actions: vec![InputAction::Text {
                text: "hello".into(),
            }],
        };
        assert!(validate_input_batch(&valid).is_ok());

        let invalid = InputBatchRequest {
            actions: vec![InputAction::Pointer {
                x: 2.0,
                y: 0.5,
                phase: "move".into(),
                button: None,
            }],
            ..valid
        };
        assert_eq!(
            validate_input_batch(&invalid),
            Err("input batch is invalid")
        );

        let release_all = InputBatchRequest {
            actions: vec![InputAction::ReleaseAll],
            ..invalid
        };
        assert!(validate_input_batch(&release_all).is_ok());

        assert!(validate_input_action(&InputAction::Key {
            key: "escape".into(),
            phase: "up".into(),
            modifiers: 0,
        }));
        assert!(!validate_input_action(&InputAction::Key {
            key: "escape".into(),
            phase: "press".into(),
            modifiers: 1 << 5,
        }));
        assert!(validate_input_action(&InputAction::Clipboard {
            operation: "copyToPhone".into(),
            text: None,
        }));
        assert!(!validate_input_action(&InputAction::Clipboard {
            operation: "pasteFromPhone".into(),
            text: None,
        }));
        assert!(!validate_input_action(&InputAction::Pointer {
            x: 0.5,
            y: 0.5,
            phase: "up".into(),
            button: None,
        }));

        let paste_with_another_action = InputBatchRequest {
            actions: vec![
                InputAction::Clipboard {
                    operation: "pasteFromPhone".into(),
                    text: Some("hello".into()),
                },
                InputAction::Key {
                    key: "return".into(),
                    phase: "press".into(),
                    modifiers: 0,
                },
            ],
            ..release_all
        };
        assert_eq!(
            validate_input_batch(&paste_with_another_action),
            Err("input batch is invalid")
        );

        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/schemas/wonder-http-v1.json"
        ))
        .unwrap();
        let wrapper = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "#/$defs/computerInputBatchRequest",
            "$defs": schema["$defs"].clone(),
        });
        let validator = jsonschema::validator_for(&wrapper).unwrap();
        let single_paste = serde_json::json!({
            "leaseId": "lease-1",
            "generation": 1,
            "geometryRevision": 2,
            "sourceId": "display-1",
            "sequence": 1,
            "actions": [{ "type": "clipboard", "operation": "pasteFromPhone", "text": "hello" }],
        });
        assert!(validator.is_valid(&single_paste));
        let mixed_paste = serde_json::json!({
            "leaseId": "lease-1",
            "generation": 1,
            "geometryRevision": 2,
            "sourceId": "display-1",
            "sequence": 1,
            "actions": [
                { "type": "clipboard", "operation": "pasteFromPhone", "text": "hello" },
                { "type": "key", "key": "return", "phase": "press", "modifiers": 0 }
            ],
        });
        assert!(!validator.is_valid(&mixed_paste));
    }

    #[test]
    fn protocol_scroll_reaches_the_helper_with_its_wire_field_names() {
        let wire = serde_json::json!({
            "leaseId": "lease-1",
            "generation": 1,
            "geometryRevision": 2,
            "sourceId": "display-1",
            "sequence": 1,
            "actions": [{ "type": "scroll", "deltaX": 0.0, "deltaY": -40.0 }],
        });
        let request: InputBatchRequest = serde_json::from_value(wire.clone()).unwrap();
        assert!(validate_input_action(&request.actions[0]));
        assert_eq!(
            serde_json::to_value(&request.actions).unwrap(),
            wire["actions"]
        );
    }

    #[test]
    fn phone_paste_accepts_multiline_unicode_but_rejects_unsafe_controls_and_oversize() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/fixtures/computer-clipboard-v1.json"
        ))
        .unwrap();
        for text in fixture["valid"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
        {
            assert!(validate_input_action(&InputAction::Clipboard {
                operation: "pasteFromPhone".into(),
                text: Some(text.into()),
            }));
        }
        let mut invalid: Vec<String> = fixture["invalid"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect();
        invalid.push("x".repeat(MAX_CLIPBOARD_TEXT + 1));
        invalid.push("e\u{301}".repeat(MAX_CLIPBOARD_TEXT / 2 + 1));
        for text in invalid {
            assert!(!validate_input_action(&InputAction::Clipboard {
                operation: "pasteFromPhone".into(),
                text: Some(text),
            }));
        }
    }

    #[test]
    fn accepted_input_recovery_cannot_be_reported_as_retryable() {
        let recovery = accepted_input_recovery();

        assert!(recovery.acknowledged);
        assert_eq!(recovery.status, "stale");
        assert!(recovery.reason.contains("reached the computer"));
        assert!(recovery.reason.contains("Do not retry"));
    }

    #[test]
    fn helper_control_capability_requires_the_exact_protocol_and_provider() {
        let valid = serde_json::json!({
            "result": {
                "protocolVersion": 1,
                "control": {
                    "available": true,
                    "provider": "core-graphics-v1",
                    "requiresAccessibility": true
                }
            }
        });
        assert!(verified_helper_control_capability(&valid));
        // Legacy Mac helpers are gated too; an explicit field advertises the
        // current bundled helper policy without disabling whole-screen input.
        assert!(!verified_helper_window_control_capability(&valid));
        let mut window_unavailable = valid.clone();
        window_unavailable["result"]["control"]["windowControlAvailable"] =
            serde_json::json!(false);
        assert!(verified_helper_control_capability(&window_unavailable));
        assert!(!verified_helper_window_control_capability(
            &window_unavailable
        ));

        let wrong_provider = serde_json::json!({
            "result": {
                "protocolVersion": 1,
                "control": {
                    "available": true,
                    "provider": "unknown",
                    "requiresAccessibility": true
                }
            }
        });
        assert!(!verified_helper_control_capability(&wrong_provider));

        let no_accessibility = serde_json::json!({
            "result": {
                "protocolVersion": 1,
                "control": {
                    "available": true,
                    "provider": "core-graphics-v1",
                    "requiresAccessibility": false
                }
            }
        });
        assert!(!verified_helper_control_capability(&no_accessibility));

        let input = |provider: &str, requires_accessibility: bool| {
            serde_json::json!({
                "result": {
                    "protocolVersion": 1,
                    "control": {
                        "available": true,
                        "provider": provider,
                        "requiresAccessibility": requires_accessibility
                    }
                }
            })
        };
        assert!(verified_helper_control_capability(&input(
            "xtest-x11-v1",
            false
        )));
        assert!(!verified_helper_control_capability(&input(
            "xtest-x11-v1",
            true
        )));
        assert!(verified_helper_control_capability(&input(
            "sendinput-win32-v1",
            false
        )));
        assert!(!verified_helper_control_capability(&input(
            "sendinput-win32-v1",
            true
        )));
        let mut unavailable = input("sendinput-win32-v1", false);
        unavailable["result"]["control"]["available"] = serde_json::json!(false);
        assert!(!verified_helper_control_capability(&unavailable));
        let mut wrong_version = input("sendinput-win32-v1", false);
        wrong_version["result"]["protocolVersion"] = serde_json::json!(2);
        assert!(!verified_helper_control_capability(&wrong_version));
        for provider in ["xtest-x11-v1", "sendinput-win32-v1"] {
            let mut capability = input(provider, false);
            assert!(verified_helper_window_control_capability(&capability));
            capability["result"]["control"]["windowControlAvailable"] = serde_json::json!(false);
            assert!(!verified_helper_window_control_capability(&capability));
        }

        let quality = serde_json::json!({
            "result": {
                "protocolVersion": 1,
                "capture": { "available": true, "liveQualityChange": true }
            }
        });
        assert!(verified_helper_quality_capability(&quality));
        for (reason, expected) in [
            ("screen_recording_permission_required", true),
            ("no_display", false),
        ] {
            let mut blocked = quality.clone();
            blocked["result"]["capture"]["available"] = serde_json::json!(false);
            blocked["result"]["capture"]["reason"] = serde_json::json!(reason);
            assert_eq!(verified_helper_quality_capability(&blocked), expected);
        }
        assert!(!verified_helper_quality_capability(&valid));
        assert!(!verified_helper_quality_capability(&serde_json::json!({
            "result": { "protocolVersion": 2, "capture": { "available": true, "liveQualityChange": true } }
        })));
    }

    /// With control off on the Mac the phone says "View only." and why, with
    /// no update or doctor advice; once on, the helper's next answer offers it.
    #[test]
    fn helper_control_answers_become_the_phone_reason() {
        let answer = |available: bool, reason: Option<&str>| {
            serde_json::json!({"id": 1, "result": {"protocolVersion": 1, "control": {
                "available": available, "provider": "core-graphics-v1",
                "requiresAccessibility": true, "windowControlAvailable": false,
                "reason": reason,
            }}})
        };
        let phone = |value: &serde_json::Value| {
            blocked_control_capability(
                verified_helper_control_capability(value),
                helper_control_block(value),
            )
        };
        let off = phone(&answer(false, Some("control_disabled")));
        assert!(!off.available);
        assert_eq!(off.action, "none");
        assert_eq!(
            off.reason,
            "Control is turned off on this computer. To allow it, run cupola computer control on there."
        );
        assert!(!off.reason.contains("doctor"));
        let accessibility = phone(&answer(false, Some("accessibility_permission_required")));
        assert!(!accessibility.available);
        assert!(accessibility.reason.contains("Accessibility"));
        // Windows' Python helper cannot ask locally: view only, said plainly.
        let unasked = phone(&answer(false, Some("local_approval_unavailable")));
        assert!(!unasked.available);
        assert_eq!(unasked.action, "none");
        assert_eq!(unasked.reason, LOCAL_APPROVAL_UNAVAILABLE_REASON);
        let on = phone(&answer(true, None));
        assert!(on.available);
        assert_eq!(on.reason, CONTROL_AVAILABLE_REASON);
        assert_eq!(phone(&answer(false, None)).action, UNAVAILABLE_ACTION);
    }

    #[test]
    fn control_capability_never_claims_control_without_a_provider() {
        let value = serde_json::to_value(control_capability_for(false)).unwrap();
        assert_eq!(value["available"], false);
        assert_eq!(value["heartbeatIntervalSeconds"], 3);
        assert_eq!(value["leaseExpirySeconds"], 10);
        assert!(value["reason"].as_str().unwrap().contains("unavailable"));
    }
}

const MAX_HELPER_LINE_BYTES: usize = 128 * 1024;
// Only the session helper's output carries shared Mac clipboard text: 256 KiB
// can expand sixfold under JSON escaping. Every other helper keeps the default.
const MAX_SESSION_HELPER_LINE_BYTES: usize = 2 * 1024 * 1024;
/// The first replies after launch. The Python helper imports aiortc and numpy
/// first, which took 2.8 s cold on a 2-vCPU Ubuntu 24.04 VM and more on a busy
/// one (#514: a 5 s budget failed sessions that would have started), so
/// Linux gets a longer budget.
const HELPER_STARTUP_TIMEOUT: Duration =
    Duration::from_secs(if cfg!(target_os = "linux") { 20 } else { 5 });
/// How long the host keeps reading after a helper exits, so its last report
/// (often why it stopped) is applied before the exit is.
const HELPER_EXIT_DRAIN: Duration = Duration::from_millis(500);
/// A helper normally exits right after closing its output. One that stays up
/// longer is stopped, so it cannot hold the only computer view.
const HELPER_OUTPUT_CLOSED_GRACE: Duration = Duration::from_millis(500);
/// A helper that no longer reads its input cannot hold up its own stop.
const HELPER_STOP_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const HELPER_COMMAND_DELIVERY_TIMEOUT: Duration = Duration::from_secs(2);
const HELPER_CONSENT_TIMEOUT: Duration = Duration::from_secs(CONTROL_CONSENT_TIMEOUT_SECONDS);
const VIEWER_ACTIVITY_LEASE: Duration = Duration::from_secs(30);
/// How long a dropped connection (a Wi-Fi roam, a few seconds out of range)
/// may take to come back before the view ends. Shorter than the viewer lease,
/// so the view still ends this way when the phone has gone for good.
const PEER_RECOVERY_GRACE: Duration = Duration::from_secs(15);

/// How long the view may take, from asking for it until the first picture is
/// showing. Only the helper and the phone's own link move it forward, so it
/// is its own clock: the phone's polls renew the idle lease below and never
/// this one. (SM-03: a view that never got a frame sat "preparing" for as long
/// as the phone kept polling.)
const OPENING_DEADLINE: Duration = Duration::from_secs(20);

/// The host's clocks for one view, so tests can shorten them.
#[derive(Clone, Copy)]
struct ViewTimings {
    viewer_activity_lease: Duration,
    opening_deadline: Duration,
}

impl ViewTimings {
    const DEFAULT: Self = Self {
        viewer_activity_lease: VIEWER_ACTIVITY_LEASE,
        opening_deadline: OPENING_DEADLINE,
    };
}

struct ViewerActivityLease {
    timer: std::pin::Pin<Box<tokio::time::Sleep>>,
    duration: Duration,
}

impl ViewerActivityLease {
    fn with_duration(duration: Duration) -> Self {
        Self {
            timer: Box::pin(tokio::time::sleep(duration)),
            duration,
        }
    }

    fn reset(&mut self) {
        self.timer
            .as_mut()
            .reset(tokio::time::Instant::now() + self.duration);
    }
}

/// Ends a view that is still opening when its budget runs out. Armed while the
/// view is in an opening phase (`ViewPhase::is_opening`), disarmed in every
/// other phase, and restarted each time the view re-enters one.
struct OpeningDeadline {
    timer: std::pin::Pin<Box<tokio::time::Sleep>>,
    armed: bool,
    budget: Duration,
}

impl OpeningDeadline {
    fn with_budget(budget: Duration) -> Self {
        Self {
            timer: Box::pin(tokio::time::sleep(budget)),
            armed: false,
            budget,
        }
    }

    fn sync(&mut self, phase: ViewPhase) {
        let opening = phase.is_opening();
        if opening && !self.armed {
            self.timer
                .as_mut()
                .reset(tokio::time::Instant::now() + self.budget);
        }
        self.armed = opening;
    }
}

/// Ends a view whose connection dropped and did not come back in time. It runs
/// on its own clock: the phone's polls stop exactly when its network is down.
struct PeerRecoveryTimer {
    timer: std::pin::Pin<Box<tokio::time::Sleep>>,
    deadline: Option<tokio::time::Instant>,
    grace: Duration,
}

impl PeerRecoveryTimer {
    fn with_grace(grace: Duration) -> Self {
        Self {
            timer: Box::pin(tokio::time::sleep(grace)),
            deadline: None,
            grace,
        }
    }

    /// Follows the signaling state: armed from the first drop, disarmed once
    /// the connection is back.
    fn sync(&mut self, interrupted_since: Option<tokio::time::Instant>) {
        let deadline = interrupted_since.map(|since| since + self.grace);
        if deadline == self.deadline {
            return;
        }
        if let Some(deadline) = deadline {
            self.timer.as_mut().reset(deadline);
        }
        self.deadline = deadline;
    }

    fn armed(&self) -> bool {
        self.deadline.is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorStartError {
    Conflict,
    Unavailable,
    Spawn,
}

impl SupervisorStartError {
    fn reason(self) -> &'static str {
        match self {
            Self::Conflict => "computer_provider_conflict",
            Self::Unavailable => UNAVAILABLE_REASON,
            Self::Spawn => "computer_provider_failed_to_start",
        }
    }
}

struct StopCommand {
    reason: String,
    end_session: bool,
}

#[derive(Clone)]
struct SignalingHandle {
    signaling: Arc<tokio::sync::Mutex<SignalingState>>,
    commands: tokio::sync::mpsc::Sender<HelperCommand>,
    viewer_activity: tokio::sync::mpsc::Sender<()>,
}

impl SignalingHandle {
    fn mark_viewer_active(&self) {
        // The channel is deliberately coalescing: one pending wakeup is
        // enough to move the lease forward after a burst of valid requests.
        let _ = self.viewer_activity.try_send(());
    }

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.commands
            .send(HelperCommand::Control {
                method: method.to_owned(),
                params,
                response: response_tx,
            })
            .await
            .map_err(|_| "computer helper is unavailable".to_owned())?;
        let response_timeout = if method == "control.consent" {
            HELPER_CONSENT_TIMEOUT
        } else if method == "control.input" {
            // Input has an eight-second execution budget. Heartbeats and release
            // still have the independent two-second admission deadline.
            Duration::from_secs(10)
        } else if matches!(method, "capture.setQuality" | "capture.rebind") {
            Duration::from_secs(8)
        } else {
            HELPER_COMMAND_DELIVERY_TIMEOUT
        };
        match timeout(response_timeout, response_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("computer helper is unavailable".to_owned()),
            Err(_) => Err("computer helper did not acknowledge the request".to_owned()),
        }
    }
}

enum HelperCommand {
    RemoteAnswer {
        peer_revision: String,
        sdp: String,
        delivered: tokio::sync::oneshot::Sender<bool>,
    },
    RemoteCandidate {
        peer_revision: String,
        sequence: u64,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_m_line_index: u32,
        username_fragment: Option<String>,
        delivered: tokio::sync::oneshot::Sender<bool>,
    },
    CloseSignal {
        peer_revision: String,
        reason: String,
        delivered: tokio::sync::oneshot::Sender<bool>,
    },
    Control {
        method: String,
        params: serde_json::Value,
        response: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    },
}

struct SignalingState {
    /// None until the helper answers capability negotiation.
    clipboard_capable: Option<bool>,
    /// The helper may hold a clipboard subscription for this session.
    clipboard_subscribed: bool,
    peer_revision: Option<String>,
    offer: Option<SignalingDescription>,
    answer: Option<String>,
    answer_pending: Option<String>,
    local_candidates: VecDeque<SignalingCandidate>,
    remote_candidates: BTreeMap<u64, SignalingCandidate>,
    remote_candidates_pending: BTreeMap<u64, SignalingCandidate>,
    peer_state: String,
    /// When the connection to the phone dropped and has not come back. The
    /// view is not live meanwhile (no input), and ends after the grace.
    peer_interrupted_since: Option<tokio::time::Instant>,
    capture_state: String,
    source_rebinding: bool,
    source_binding_revision: u64,
    first_frame_observed: bool,
    frame_geometry_revision: Option<u64>,
    /// The first terminal cause; later failures never replace it.
    failure_reason: Option<String>,
    closed: bool,
    /// The view went live at least once, so a failure stopped sharing
    /// rather than starting it.
    shared: bool,
    /// The helper's latest report for the active control lease and view.
    text_input_focused: Option<bool>,
    /// The lease the computer last ended and why (stopped there, control
    /// turned off, timed out), so the next refused input can say which.
    revoked_lease: Option<(String, String)>,
}

impl Default for SignalingState {
    fn default() -> Self {
        Self {
            clipboard_capable: None,
            clipboard_subscribed: false,
            peer_revision: None,
            offer: None,
            answer: None,
            answer_pending: None,
            local_candidates: VecDeque::new(),
            remote_candidates: BTreeMap::new(),
            remote_candidates_pending: BTreeMap::new(),
            peer_state: "new".into(),
            peer_interrupted_since: None,
            capture_state: "preparing".into(),
            source_rebinding: false,
            source_binding_revision: 0,
            first_frame_observed: false,
            frame_geometry_revision: None,
            failure_reason: None,
            closed: false,
            shared: false,
            text_input_focused: None,
            revoked_lease: None,
        }
    }
}

/// Where one computer view is on its way to showing pictures. The only place
/// the signaling facts (capture, peer, frame, failure) are read together:
/// `public_state`, the durable state and the opening deadline all follow it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ViewPhase {
    Closed,
    Failed,
    Reconnecting,
    Live,
    /// The helper answered the offer; the phone's link is coming up.
    Connecting,
    OfferReady,
    /// Waiting for the person to choose what to share, which has no deadline.
    AwaitingSource,
    /// The helper is starting capture or no frame has arrived yet.
    Preparing,
}

impl ViewPhase {
    fn public_name(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Failed => "failed",
            Self::Reconnecting => "reconnecting",
            Self::Live => "live",
            Self::Connecting => "connecting",
            Self::OfferReady => "offerReady",
            Self::AwaitingSource => "awaitingSource",
            Self::Preparing => "preparing",
        }
    }

    /// The view was asked for and no picture is showing yet. The opening
    /// deadline runs in exactly these phases.
    fn is_opening(self) -> bool {
        matches!(self, Self::Preparing | Self::OfferReady | Self::Connecting)
    }
}

impl SignalingState {
    fn phase(&self) -> ViewPhase {
        if self.closed {
            return ViewPhase::Closed;
        }
        if self.failure_reason.is_some()
            || matches!(self.peer_state.as_str(), "failed" | "closed")
            || matches!(
                self.capture_state.as_str(),
                "permissionDenied" | "sourceRemoved" | "failed" | "stopped" | "helperRestarted"
            )
        {
            return ViewPhase::Failed;
        }
        // A dropped connection that may still come back is not live (input is
        // refused) but not failed either, so signaling keeps answering.
        if self.peer_interrupted_since.is_some() {
            return ViewPhase::Reconnecting;
        }
        if shows_live_view(self) {
            return ViewPhase::Live;
        }
        if self.answer.is_some() {
            return ViewPhase::Connecting;
        }
        if self.offer.is_some() {
            return ViewPhase::OfferReady;
        }
        if self.capture_state == "awaitingSource" {
            return ViewPhase::AwaitingSource;
        }
        ViewPhase::Preparing
    }

    fn public_state(&self) -> String {
        self.phase().public_name().into()
    }

    fn clear_ephemeral(&mut self, reason: Option<String>) {
        self.peer_revision = None;
        self.offer = None;
        self.answer = None;
        self.answer_pending = None;
        self.local_candidates.clear();
        self.remote_candidates.clear();
        self.remote_candidates_pending.clear();
        self.first_frame_observed = false;
        self.frame_geometry_revision = None;
        match reason {
            Some(reason) => self.fail(reason),
            None => self.failure_reason = None,
        }
        self.text_input_focused = None;
        self.peer_interrupted_since = None;
    }

    /// Record why the view failed unless an earlier cause is already known.
    fn fail(&mut self, reason: String) {
        self.failure_reason.get_or_insert(reason);
    }

    /// The durable state this signaling state asks for, noting that the
    /// view went live.
    fn desired_state(&mut self) -> (ComputerSessionState, Option<String>) {
        let desired = desired_media_state(self);
        if desired.0 == ComputerSessionState::Live {
            self.shared = true;
        }
        desired
    }

    /// Returns `true` when a new command must be sent and `false` for an
    /// identical already-pending/applied command.
    fn reserve_answer(&mut self, sdp: &str) -> Result<bool, &'static str> {
        if self.offer.is_none() {
            return Err("computer offer is not ready");
        }
        if self.answer.as_deref() == Some(sdp) {
            return Ok(false);
        }
        if self.answer_pending.as_deref() == Some(sdp) {
            return Err("computer answer is still being applied");
        }
        if self.answer.is_some() || self.answer_pending.is_some() {
            return Err("computer answer is already bound");
        }
        self.answer_pending = Some(sdp.to_owned());
        Ok(true)
    }

    fn commit_answer(&mut self, sdp: &str) {
        if self.answer_pending.as_deref() == Some(sdp) {
            self.answer = self.answer_pending.take();
        }
    }

    fn rollback_answer(&mut self, sdp: &str) {
        if self.answer_pending.as_deref() == Some(sdp) {
            self.answer_pending = None;
        }
    }

    fn reserve_candidate(&mut self, candidate: SignalingCandidate) -> Result<bool, &'static str> {
        let sequence = candidate.sequence;
        if let Some(existing) = self.remote_candidates.get(&sequence) {
            if existing == &candidate {
                return Ok(false);
            }
            return Err("computer ICE sequence is already bound");
        }
        if let Some(existing) = self.remote_candidates_pending.get(&sequence) {
            if existing == &candidate {
                return Err("computer ICE candidate is still being applied");
            }
            return Err("computer ICE sequence is already bound");
        }
        if self.remote_candidates.len() + self.remote_candidates_pending.len()
            >= MAX_SIGNALING_CANDIDATES
        {
            return Err("computer ICE candidate limit reached");
        }
        self.remote_candidates_pending.insert(sequence, candidate);
        Ok(true)
    }

    fn commit_candidate(&mut self, sequence: u64) {
        if let Some(candidate) = self.remote_candidates_pending.remove(&sequence) {
            self.remote_candidates.insert(sequence, candidate);
        }
    }

    fn rollback_candidate(&mut self, sequence: u64) {
        self.remote_candidates_pending.remove(&sequence);
    }
}

struct ManagedHelper {
    session_id: String,
    owner_device_id: String,
    stop: Option<tokio::sync::oneshot::Sender<StopCommand>>,
    join: tokio::task::JoinHandle<()>,
    signaling: Arc<tokio::sync::Mutex<SignalingState>>,
    commands: tokio::sync::mpsc::Sender<HelperCommand>,
    viewer_activity: tokio::sync::mpsc::Sender<()>,
}

struct PendingControlConsent {
    lease_id: String,
    cancel: tokio::sync::oneshot::Sender<()>,
}

#[derive(Default)]
struct SessionControl {
    input_batches: Arc<tokio::sync::Mutex<()>>,
    pending_consent: Option<PendingControlConsent>,
}

impl SessionControl {
    fn cancel_consent(&mut self, lease_id: Option<&str>) {
        if lease_id.is_some_and(|id| {
            self.pending_consent
                .as_ref()
                .is_none_or(|pending| pending.lease_id != id)
        }) {
            return;
        }
        if let Some(pending) = self.pending_consent.take() {
            let _ = pending.cancel.send(());
        }
    }
}

type SessionControlLock = tokio::sync::Mutex<SessionControl>;

pub struct ComputerSessionSupervisor {
    active: tokio::sync::Mutex<Option<ManagedHelper>>,
    /// The one owner of the host's viewer lifecycle: active-row lookup,
    /// replacement, helper stop/reap, retirement and the next reservation run
    /// under it, so two opens cannot both pass the one-viewer check. Take a
    /// session's control lock before this one, never after.
    lifecycle: tokio::sync::Mutex<()>,
    /// The session whose helper reported the capabilities below. A late stop
    /// or status from another session cannot change the live viewer's.
    capability_session: std::sync::Mutex<Option<String>>,
    source_discovery: SourceDiscovery,
    session_controls: std::sync::Mutex<BTreeMap<String, std::sync::Weak<SessionControlLock>>>,
    control_capable: AtomicBool,
    window_control_blocked: AtomicBool,
    control_block: AtomicU8,
    quality_capable: AtomicBool,
    simulator_control: std::sync::Mutex<Option<SimulatorControl>>,
    takeovers: std::sync::Mutex<BTreeMap<String, TakeoverRequest>>,
    /// Remote screens agents added over SSH (Experimental): `thread_tools::stream`.
    pub(crate) streams: crate::thread_tools::stream::Streams,
}

// ---------------------------------------------------------------------------
// Agent takeover requests
// ---------------------------------------------------------------------------

/// Push key prefix for a takeover request (`push::delivery_preview`).
pub(crate) const TAKEOVER_PUSH_PREFIX: &str = "external:computer-takeover:";
/// Ended requests stay this long, so a retried call gets the same answer.
const TAKEOVER_RETENTION_SECONDS: i64 = 3600;
const MAX_TAKEOVERS: usize = 64;

/// One agent's request that the owner take over the computer (for example to
/// sign in). Kept in memory: a request lives only as long as the reply that
/// waits on it, and that reply does not survive a daemon restart either.
#[derive(Clone, Debug)]
pub(crate) struct TakeoverRequest {
    pub(crate) conversation_id: String,
    pub(crate) agent: String,
    pub(crate) reason: String,
    pub(crate) app: Option<String>,
    created_at: chrono::DateTime<Utc>,
    expires_at: chrono::DateTime<Utc>,
    pub(crate) state: TakeoverState,
    /// Set only after native consent and activation both succeeded.
    control: Option<TakeoverControl>,
    ended_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Clone, Debug)]
struct TakeoverControl {
    session_id: String,
    lease_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TakeoverState {
    /// Waiting for the owner to take control.
    Waiting,
    /// The owner holds control; the agent waits for it to be given back.
    InControl,
    Returned,
    Declined,
    Expired,
    /// The reply that asked ended first.
    Cancelled,
}

impl TakeoverState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::InControl => "in_control",
            Self::Returned => "returned",
            Self::Declined => "declined",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn ended(self) -> bool {
        !matches!(self, Self::Waiting | Self::InControl)
    }
}

pub(crate) struct TakeoverOpen<'a> {
    pub(crate) id: &'a str,
    pub(crate) conversation_id: &'a str,
    pub(crate) agent: &'a str,
    pub(crate) reason: &'a str,
    pub(crate) app: Option<&'a str>,
    pub(crate) timeout: Duration,
}

impl ComputerSessionSupervisor {
    /// Records a request; true when it is new. A retry with the same id
    /// keeps the original request, deadline included.
    pub(crate) fn open_takeover(&self, open: TakeoverOpen<'_>, now: chrono::DateTime<Utc>) -> bool {
        let mut takeovers = self.takeovers.lock().unwrap();
        takeovers.retain(|_, request| {
            request.ended_at.is_none_or(|at| {
                now.signed_duration_since(at).num_seconds() < TAKEOVER_RETENTION_SECONDS
            })
        });
        if takeovers.contains_key(open.id) {
            return false;
        }
        // Bounded: the oldest ended requests go first.
        while takeovers.len() >= MAX_TAKEOVERS {
            let Some(oldest) = takeovers
                .iter()
                .min_by_key(|(_, r)| (!r.state.ended(), r.created_at))
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            takeovers.remove(&oldest);
        }
        takeovers.insert(
            open.id.to_owned(),
            TakeoverRequest {
                conversation_id: open.conversation_id.to_owned(),
                agent: open.agent.to_owned(),
                reason: open.reason.to_owned(),
                app: open.app.map(str::to_owned),
                created_at: now,
                expires_at: now + chrono::Duration::from_std(open.timeout).unwrap_or_default(),
                state: TakeoverState::Waiting,
                control: None,
                ended_at: None,
            },
        );
        true
    }

    /// Deadlines are independent of leases: losing a reservation/heartbeat is
    /// never evidence that the owner finished their private interaction.
    pub(crate) fn advance_takeover(
        &self,
        id: &str,
        now: chrono::DateTime<Utc>,
    ) -> Option<TakeoverState> {
        let mut takeovers = self.takeovers.lock().unwrap();
        let request = takeovers.get_mut(id)?;
        if !request.state.ended() && now >= request.expires_at {
            request.state = TakeoverState::Expired;
            request.ended_at = Some(now);
        }
        Some(request.state)
    }

    fn takeover_accepts_control(
        &self,
        id: &str,
        session: &StoredComputerSession,
        now: chrono::DateTime<Utc>,
    ) -> bool {
        self.takeovers
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|request| {
                !request.state.ended()
                    && now < request.expires_at
                    && request.conversation_id == session.conversation_id
                    && !session
                        .source_id
                        .as_deref()
                        .is_some_and(|source| source.starts_with("simulator:"))
            })
    }

    fn takeover_lease_matches(&self, id: Option<&str>, lease_id: &str) -> bool {
        self.takeovers
            .lock()
            .unwrap()
            .iter()
            .all(|(request_id, request)| {
                request.control.as_ref().is_none_or(|control| {
                    control.lease_id != lease_id || Some(request_id.as_str()) == id
                })
            })
    }

    /// Called inside the session's acquisition boundary, after native success.
    pub(crate) fn activate_takeover(
        &self,
        id: &str,
        session: &StoredComputerSession,
        lease: &StoredComputerControlLease,
        now: chrono::DateTime<Utc>,
    ) {
        let mut takeovers = self.takeovers.lock().unwrap();
        if takeovers.iter().any(|(other, request)| {
            other != id
                && request
                    .control
                    .as_ref()
                    .is_some_and(|control| control.lease_id == lease.id)
        }) {
            return;
        }
        let Some(request) = takeovers.get_mut(id) else {
            return;
        };
        if request.state.ended()
            || now >= request.expires_at
            || request.conversation_id != session.conversation_id
            || !lease_binding_for_session(lease, session)
            || lease.status != "active"
            || lease.expires_at <= now.to_rfc3339_opts(SecondsFormat::Millis, true)
            || lease
                .source_id
                .as_deref()
                .is_some_and(|source| source.starts_with("simulator:"))
        {
            return;
        }
        request.control = Some(TakeoverControl {
            session_id: session.id.clone(),
            lease_id: lease.id.clone(),
        });
        request.state = TakeoverState::InControl;
    }

    /// A lost connection is not hand-back. End only the binding we checked,
    /// so a late poll cannot erase a new acquisition or an explicit answer.
    fn expire_takeover_control(&self, id: &str, lease_id: &str, now: chrono::DateTime<Utc>) {
        let mut takeovers = self.takeovers.lock().unwrap();
        if let Some(request) = takeovers.get_mut(id) {
            if request.state == TakeoverState::InControl
                && request
                    .control
                    .as_ref()
                    .is_some_and(|control| control.lease_id == lease_id)
            {
                request.state = TakeoverState::Expired;
                request.control = None;
                request.ended_at = Some(now);
            }
        }
    }

    /// Called only after the owner explicitly releases this exact active lease.
    fn return_takeover(&self, lease: &StoredComputerControlLease, now: chrono::DateTime<Utc>) {
        if lease.status != "active"
            || lease.expires_at <= now.to_rfc3339_opts(SecondsFormat::Millis, true)
        {
            return;
        }
        for request in self.takeovers.lock().unwrap().values_mut() {
            if request.state == TakeoverState::InControl
                && now < request.expires_at
                && request.control.as_ref().is_some_and(|control| {
                    control.session_id == lease.session_id && control.lease_id == lease.id
                })
            {
                request.state = TakeoverState::Returned;
                request.ended_at = Some(now);
            }
        }
    }

    /// The owner said not now. Only an open request can be declined.
    pub(crate) fn decline_takeover(&self, id: &str, now: chrono::DateTime<Utc>) -> bool {
        self.end_takeover(id, TakeoverState::Declined, now)
    }

    pub(crate) fn cancel_takeover(&self, id: &str, now: chrono::DateTime<Utc>) -> bool {
        self.end_takeover(id, TakeoverState::Cancelled, now)
    }

    fn end_takeover(&self, id: &str, state: TakeoverState, now: chrono::DateTime<Utc>) -> bool {
        let mut takeovers = self.takeovers.lock().unwrap();
        match takeovers.get_mut(id) {
            Some(request) if !request.state.ended() => {
                request.state = state;
                request.ended_at = Some(now);
                true
            }
            _ => false,
        }
    }

    fn takeover(&self, id: &str) -> Option<TakeoverRequest> {
        self.takeovers.lock().unwrap().get(id).cloned()
    }

    pub(crate) fn open_takeovers(&self) -> Vec<(String, TakeoverRequest)> {
        let mut open: Vec<_> = self
            .takeovers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, request)| !request.state.ended())
            .map(|(id, request)| (id.clone(), request.clone()))
            .collect();
        open.sort_by_key(|(_, request)| request.created_at);
        open
    }
}

/// Whether the owner holds phone control of this computer's screen or a
/// window now. Simulator control stays inside the simulator, so it leaves the
/// agent's computer use alone.
pub(crate) async fn owner_has_control(state: &AppState) -> Result<bool, sqlx::Error> {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    for session in state
        .store
        .active_computer_sessions_for_host(&state.host_installation_id)
        .await?
    {
        if let Some(lease) = state
            .store
            .active_computer_control_lease_for_session(&session.id, &now)
            .await?
        {
            if !lease
                .source_id
                .as_deref()
                .is_some_and(|source| source.starts_with("simulator:"))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Opens a request and tells the owner's phones. Idempotent per id.
pub(crate) async fn request_takeover(state: &AppState, open: TakeoverOpen<'_>) {
    let (id, conversation) = (open.id.to_owned(), open.conversation_id.to_owned());
    if state.computer_supervisor.open_takeover(open, Utc::now()) {
        let key = format!("computer-takeover:{id}");
        let _ = state
            .store
            .enqueue_external_push(&key, &conversation, "attention", None)
            .await;
    }
}

/// Returns the recorded request state, applying its deadline, or None when unknown.
pub(crate) async fn takeover_state(state: &AppState, id: &str) -> Option<TakeoverState> {
    let supervisor = &state.computer_supervisor;
    if let Some(request) = supervisor.takeover(id) {
        if let Some(control) = request
            .control
            .filter(|_| request.state == TakeoverState::InControl)
        {
            // Serialize with activation and explicit hand-back. In particular,
            // never expire the request between releasing its lease and recording
            // Returned. Database errors preserve the current state.
            let boundary = supervisor.session_control(&control.session_id);
            let _guard = boundary.lock().await;
            if let (Ok(lease), Ok(session)) = (
                state.store.computer_control_lease(&control.lease_id).await,
                state.store.computer_session(&control.session_id).await,
            ) {
                let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
                let active = lease.zip(session).is_some_and(|(lease, session)| {
                    lease.status == "active"
                        && lease.expires_at > now
                        && lease_binding_for_session(&lease, &session)
                        && !matches!(session.state.as_str(), "ended" | "failed" | "unavailable")
                });
                if !active {
                    supervisor.expire_takeover_control(id, &control.lease_id, Utc::now());
                }
            }
        }
    }
    let next = supervisor.advance_takeover(id, Utc::now())?;
    if next.ended() {
        withdraw_takeover_push(state, id).await;
    }
    Some(next)
}

pub(crate) async fn withdraw_takeover_push(state: &AppState, id: &str) {
    let _ = state
        .store
        .withdraw_external_push(&format!("computer-takeover:{id}"))
        .await;
}

/// The alert for a takeover request, or None once it has ended (a request
/// that ended before its push went out never alerts).
pub(crate) async fn takeover_push_preview(
    state: &AppState,
    source_key: &str,
) -> Option<(String, String)> {
    let id = source_key.strip_prefix(TAKEOVER_PUSH_PREFIX)?;
    let request = state
        .computer_supervisor
        .takeover(id)
        .filter(|request| !request.state.ended())?;
    let computer = crate::host_name::current(state).await;
    let stop = if request.reason.ends_with(['.', '!', '?']) {
        ""
    } else {
        "."
    };
    Some((
        format!("{} needs you on {computer}", request.agent),
        format!("{}{stop} Open Cupola to take control.", request.reason),
    ))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TakeoverResponse {
    id: String,
    conversation_id: String,
    agent: String,
    reason: String,
    app: Option<String>,
    state: &'static str,
    requested_at: String,
    expires_at: String,
}

fn takeover_response(id: String, request: TakeoverRequest) -> TakeoverResponse {
    let at = |time: chrono::DateTime<Utc>| time.to_rfc3339_opts(SecondsFormat::Millis, true);
    TakeoverResponse {
        id,
        conversation_id: request.conversation_id,
        agent: request.agent,
        reason: request.reason,
        app: request.app,
        state: request.state.as_str(),
        requested_at: at(request.created_at),
        expires_at: at(request.expires_at),
    }
}

/// Open takeover requests, oldest first, for the phone's Take control card.
pub(crate) async fn takeover_requests(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    // Finish expired/lost bindings even when the waiting reply is no longer
    // polling, so a disconnected request cannot leave a stale control card.
    for (id, _) in state.computer_supervisor.open_takeovers() {
        takeover_state(&state, &id).await;
    }
    let requests: Vec<_> = state
        .computer_supervisor
        .open_takeovers()
        .into_iter()
        .map(|(id, request)| takeover_response(id, request))
        .collect();
    Json(serde_json::json!({ "requests": requests })).into_response()
}

/// The owner's Not now: the agent's request returns `declined`.
pub(crate) async fn decline_takeover(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    if state.computer_supervisor.decline_takeover(&id, Utc::now()) {
        withdraw_takeover_push(&state, &id).await;
        return StatusCode::NO_CONTENT.into_response();
    }
    match state.computer_supervisor.takeover(&id) {
        // Already answered (returned, expired or declined): nothing to do.
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => (StatusCode::NOT_FOUND, "This request has ended.").into_response(),
    }
}

// ---------------------------------------------------------------------------
// Remote screens (Experimental): the owner's side of `wonder_stream_screen`
// ---------------------------------------------------------------------------

/// Remote screens agents added or asked to add, oldest first.
pub(crate) async fn remote_streams(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    Json(serde_json::json!({ "streams": state.computer_supervisor.streams.views() }))
        .into_response()
}

fn stream_answer(view: Option<crate::thread_tools::stream::StreamView>) -> Response {
    match view {
        Some(view) => {
            Json(serde_json::json!({ "state": view.state, "stream": view })).into_response()
        }
        None => (StatusCode::NOT_FOUND, "This remote screen has ended.").into_response(),
    }
}

/// The owner's Allow: the agent's waiting call connects.
pub(crate) async fn allow_remote_stream(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    stream_answer(state.computer_supervisor.streams.allow(&id))
}

/// The owner's Decline: the agent's call returns `declined`.
pub(crate) async fn decline_remote_stream(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    stream_answer(state.computer_supervisor.streams.decline(&id))
}

/// The owner's Stop: answers once the connection to the other computer is closed.
pub(crate) async fn stop_remote_stream(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    stream_answer(state.computer_supervisor.streams.stop(&id).await)
}

#[derive(Deserialize)]
pub(crate) struct RemoteFrameQuery {
    pub(crate) after: Option<u64>,
}

/// The latest picture as JSON like a simulator screenshot (`mimeType`, size, SHA-256, base64), or only its
/// sequence number when the viewer already has it (`?after=N`). `404` until the first picture exists.
pub(crate) async fn remote_stream_frame(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(id): Path<String>,
    Query(query): Query<RemoteFrameQuery>,
) -> Response {
    if let Err(response) = owner_device(authenticated).await {
        return response;
    }
    let Some(frame) = state.computer_supervisor.streams.frame(&id) else {
        return (StatusCode::NOT_FOUND, "No picture yet.").into_response();
    };
    if query.after == Some(frame.sequence) {
        return Json(
            serde_json::json!({ "id": id, "sequence": frame.sequence, "imageBase64": null }),
        )
        .into_response();
    }
    use base64::Engine as _;
    Json(serde_json::json!({
        "id": id,
        "sequence": frame.sequence,
        "mimeType": "image/jpeg",
        "byteCount": frame.jpeg.len(),
        "width": frame.width,
        "height": frame.height,
        "sha256": frame.sha256,
        "imageBase64": base64::engine::general_purpose::STANDARD.encode(&frame.jpeg),
    }))
    .into_response()
}

/// The helper's answer for one simulator (`simulator:<UDID>`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct SimulatorControl {
    source_id: String,
    available: bool,
    reason: Option<String>,
}

impl Default for ComputerSessionSupervisor {
    fn default() -> Self {
        Self {
            active: tokio::sync::Mutex::new(None),
            lifecycle: tokio::sync::Mutex::new(()),
            capability_session: std::sync::Mutex::new(None),
            source_discovery: SourceDiscovery::default(),
            session_controls: std::sync::Mutex::new(BTreeMap::new()),
            control_capable: AtomicBool::new(false),
            window_control_blocked: AtomicBool::new(false),
            control_block: AtomicU8::new(0),
            quality_capable: AtomicBool::new(false),
            simulator_control: std::sync::Mutex::new(None),
            takeovers: std::sync::Mutex::new(BTreeMap::new()),
            streams: Default::default(),
        }
    }
}

/// The helper is a plain executable inside Wonder.app, spawned as the daemon's
/// child so macOS attributes Screen Recording and Accessibility to Wonder.
async fn launch_helper(
    binary: PathBuf,
    handshake: &str,
) -> Result<LaunchedHelper, SupervisorStartError> {
    #[cfg(windows)]
    return launch_owned_helper(binary, handshake, own_helper).await;
    #[cfg(not(windows))]
    launch_owned_helper(binary, handshake).await
}

/// This small input-only process deliberately does not join the capture job:
/// it must outlive abrupt host/job termination long enough to release input.
/// Only the native helper can control Windows; the Python fallback refuses it.
#[cfg(windows)]
async fn launch_input_owner(
    binary: &FsPath,
    handshake: &str,
) -> Result<(Option<tokio::process::Child>, Option<String>), SupervisorStartError> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    if binary.file_name().and_then(|n| n.to_str()) != Some("wonder-computer-helper.exe") {
        return Ok((None, None));
    }
    let mut command = Command::new(binary);
    command
        .env_clear()
        .envs(helper_environment(std::env::vars_os()))
        .env("WONDER_COMPUTER_USE_HANDSHAKE", handshake)
        .arg("--input-owner")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // CREATE_NO_WINDOW; no kill_on_drop: EOF and its independent timeout own cleanup.
    command.creation_flags(0x08000000 | 0x01000000); // CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB
    let mut child = command.spawn().map_err(|_| SupervisorStartError::Spawn)?;
    let stdout = child.stdout.take().ok_or(SupervisorStartError::Spawn)?;
    let mut reader = tokio::io::BufReader::new(stdout.take(128));
    let mut address = String::new();
    if !matches!(timeout(Duration::from_secs(3),reader.read_line(&mut address)).await,Ok(Ok(n)) if n>0)
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(SupervisorStartError::Spawn);
    }
    let address = address.trim().to_string();
    if !address
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|a| a.ip().is_loopback())
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(SupervisorStartError::Spawn);
    }
    Ok((Some(child), Some(address)))
}

async fn launch_owned_helper(
    binary: PathBuf,
    handshake: &str,
    #[cfg(windows)] owner: HelperOwner,
) -> Result<LaunchedHelper, SupervisorStartError> {
    #[cfg(windows)]
    let (input_owner, input_address) = launch_input_owner(&binary, handshake).await?;
    let mut command = helper_command(&binary).await;
    #[cfg(windows)]
    if let Some(address) = input_address {
        command.env("WONDER_INPUT_OWNER", address);
    }
    let mut child = command
        .stderr(std::process::Stdio::piped())
        .env("WONDER_COMPUTER_USE_HANDSHAKE", handshake)
        .spawn()
        .map_err(|_| SupervisorStartError::Spawn)?;
    // A helper this session cannot end must not capture: refuse the launch.
    #[cfg(windows)]
    let job = match owner(&child) {
        Ok(job) => job,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(SupervisorStartError::Spawn);
        }
    };
    let Some(stdin) = child.stdin.take() else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(SupervisorStartError::Spawn);
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(SupervisorStartError::Spawn);
    };
    let stderr = child.stderr.take();
    let codec_log = CodecLogTask(tokio::spawn(async move {
        if let Some(mut stderr) = stderr {
            if let Some(codec) = read_codec_summary(&mut stderr).await {
                eprintln!("computer video codec: {codec}");
                // Drain after the one summary so libwebrtc diagnostics cannot
                // fill its pipe. No further diagnostics are forwarded.
                let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            }
        }
    }));
    Ok(LaunchedHelper {
        child,
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        _codec_log: codec_log,
        #[cfg(windows)]
        _input_owner: input_owner,
        #[cfg(windows)]
        _job: job,
    })
}

impl ComputerSessionSupervisor {
    fn session_control(&self, id: &str) -> Arc<SessionControlLock> {
        let mut controls = self.session_controls.lock().unwrap();
        controls.retain(|_, control| control.strong_count() > 0);
        if let Some(control) = controls.get(id).and_then(std::sync::Weak::upgrade) {
            return control;
        }
        let control = Arc::new(SessionControlLock::default());
        controls.insert(id.into(), Arc::downgrade(&control));
        control
    }

    fn control_capable(&self) -> bool {
        self.control_capable.load(Ordering::Acquire)
    }

    fn set_control_capable(&self, value: bool) {
        self.control_capable.store(value, Ordering::Release);
        if !value {
            self.quality_capable.store(false, Ordering::Release);
            *self.simulator_control.lock().unwrap() = None;
        }
    }

    /// Only an answer for this exact simulator counts.
    fn simulator_control_for(&self, source_id: Option<&str>) -> Option<SimulatorControl> {
        self.simulator_control
            .lock()
            .unwrap()
            .clone()
            .filter(|control| Some(control.source_id.as_str()) == source_id)
    }

    fn set_simulator_control(&self, value: Option<SimulatorControl>) {
        *self.simulator_control.lock().unwrap() = value;
    }

    fn quality_capable(&self) -> bool {
        self.quality_capable.load(Ordering::Acquire)
    }

    /// A starting helper owns the capabilities until it reports its own.
    fn claim_capabilities(&self, session_id: &str) {
        let mut owner = self.capability_session.lock().unwrap();
        *owner = Some(session_id.into());
        self.set_control_capable(false);
    }

    /// Applies a helper's reported capabilities only while its session owns them.
    /// Returns whether they were applied.
    fn set_session_capabilities(&self, session_id: &str, value: &serde_json::Value) -> bool {
        let owner = self.capability_session.lock().unwrap();
        if owner.as_deref() != Some(session_id) {
            return false;
        }
        self.control_block
            .store(helper_control_block(value), Ordering::Release);
        self.window_control_blocked.store(
            !verified_helper_window_control_capability(value),
            Ordering::Release,
        );
        self.set_control_capable(verified_helper_control_capability(value));
        self.set_quality_capable(verified_helper_quality_capability(value));
        true
    }

    /// Ending one session clears only the capabilities it owns.
    fn release_capabilities(&self, session_id: &str) {
        let mut owner = self.capability_session.lock().unwrap();
        if owner.as_deref() == Some(session_id) {
            *owner = None;
            self.set_control_capable(false);
        }
    }

    fn set_quality_capable(&self, value: bool) {
        self.quality_capable.store(value, Ordering::Release);
    }

    pub async fn start(
        self: &Arc<Self>,
        state: AppState,
        session: StoredComputerSession,
    ) -> Result<(), SupervisorStartError> {
        self.start_with_timings(state, session, ViewTimings::DEFAULT)
            .await
    }

    async fn start_with_timings(
        self: &Arc<Self>,
        state: AppState,
        session: StoredComputerSession,
        timings: ViewTimings,
    ) -> Result<(), SupervisorStartError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.start_reserved(state, session, timings).await
    }

    /// Starts the durable active session's helper. The caller holds `lifecycle`.
    async fn start_reserved(
        self: &Arc<Self>,
        state: AppState,
        session: StoredComputerSession,
        timings: ViewTimings,
    ) -> Result<(), SupervisorStartError> {
        if !provider_available(&state) {
            return Err(SupervisorStartError::Unavailable);
        }

        if let Some(active) = state
            .store
            .active_computer_session_for_host(&state.host_installation_id)
            .await
            .map_err(|_| SupervisorStartError::Conflict)?
        {
            if active.id != session.id {
                return Err(SupervisorStartError::Conflict);
            }
        }

        let leftover = {
            let mut active = self.active.lock().await;
            if let Some(current) = active.as_ref() {
                if current.session_id == session.id
                    && current.owner_device_id == session.owner_device_id
                {
                    return Ok(());
                }
                if current.session_id == session.id {
                    return Err(SupervisorStartError::Conflict);
                }
            }
            // The durable row names this session as the host's viewer, so a
            // helper still held for another one (a failed capture, a replaced
            // view) is left over: retire it before reserving the next.
            let leftover = active.take();
            if let Some(leftover) = leftover.as_ref() {
                self.release_capabilities(&leftover.session_id);
            }
            leftover
        };
        if let Some(leftover) = leftover {
            self.retire_helper(leftover, "viewer_replaced").await;
        }

        let binary = state
            .computer_use_bin
            .clone()
            .ok_or(SupervisorStartError::Unavailable)?;
        self.claim_capabilities(&session.id);
        let handshake = uuid::Uuid::new_v4().to_string();
        let helper = launch_helper(binary, &handshake).await?;
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(64);
        let (viewer_activity_tx, viewer_activity_rx) = tokio::sync::mpsc::channel(1);
        let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
        let supervisor = Arc::clone(self);
        let session_id = session.id.clone();
        let owner_device_id = session.owner_device_id.clone();
        let revocations = state.revocations.subscribe();
        let join = tokio::spawn(supervise_helper(
            supervisor,
            state,
            session,
            handshake,
            helper,
            stop_rx,
            revocations,
            command_rx,
            viewer_activity_rx,
            timings,
            Arc::clone(&signaling),
        ));
        let mut active = self.active.lock().await;
        if active.is_some() {
            drop(active);
            self.release_capabilities(&session_id);
            let _ = stop_tx.send(StopCommand {
                reason: "provider_conflict".into(),
                end_session: false,
            });
            reap_join(join, Duration::from_secs(1)).await;
            return Err(SupervisorStartError::Conflict);
        }
        *active = Some(ManagedHelper {
            session_id,
            owner_device_id,
            stop: Some(stop_tx),
            join,
            signaling,
            commands: command_tx,
            viewer_activity: viewer_activity_tx,
        });
        Ok(())
    }

    pub async fn stop(&self, state: &AppState, session_id: &str, reason: &str) {
        let _lifecycle = self.lifecycle.lock().await;
        self.stop_reserved(state, session_id, reason).await;
    }

    /// Stops only this session's helper and input. A late stop for a session
    /// that is no longer the viewer leaves the live one's control untouched.
    /// The caller holds `lifecycle`.
    async fn stop_reserved(&self, state: &AppState, session_id: &str, reason: &str) {
        let managed = {
            let mut active = self.active.lock().await;
            if active
                .as_ref()
                .is_some_and(|value| value.session_id == session_id)
            {
                self.release_capabilities(session_id);
                active.take()
            } else {
                None
            }
        };
        if let Some(managed) = managed {
            self.retire_helper(managed, reason).await;
        }
        let _ = state
            .store
            .release_computer_control_lease_for_session(
                session_id,
                &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            )
            .await;
    }

    /// Ends a helper already taken out of the slot, within the reap bound.
    async fn retire_helper(&self, mut managed: ManagedHelper, reason: &str) {
        self.release_capabilities(&managed.session_id);
        if let Some(stop) = managed.stop.take() {
            let _ = stop.send(StopCommand {
                reason: reason.into(),
                end_session: false,
            });
        }
        reap_join(managed.join, Duration::from_secs(3)).await;
    }

    pub async fn shutdown(&self, state: &AppState) {
        // A hung discovery query must not delay releasing capture/input control.
        tokio::join!(
            self.source_discovery.shutdown(),
            self.shutdown_capture(state),
            // Remote screens end with the daemon: their ssh children are killed and reaped.
            self.streams.shutdown()
        );
    }

    async fn shutdown_capture(&self, state: &AppState) {
        let managed = self.active.lock().await.take();
        let Some(mut managed) = managed else {
            self.set_control_capable(false);
            return;
        };
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        if let Ok(Some(session)) = state.store.computer_session(&managed.session_id).await {
            if session.state != ComputerSessionState::Ended.as_str() {
                if let Ok(Some(ended)) = state
                    .store
                    .end_computer_session(
                        &session.id,
                        &session.owner_device_id,
                        Some(session.generation),
                        &now,
                    )
                    .await
                {
                    let _ = state
                        .store
                        .release_computer_control_lease_for_session(&ended.id, &now)
                        .await;
                    publish_session_event(state, &ended).await;
                }
            }
        }
        if let Some(stop) = managed.stop.take() {
            let _ = stop.send(StopCommand {
                reason: "daemon_shutdown".into(),
                end_session: false,
            });
        }
        reap_join(managed.join, Duration::from_secs(3)).await;
        self.set_control_capable(false);
    }

    pub async fn retire_durable_sessions(&self, state: &AppState) {
        let Ok(sessions) = state
            .store
            .active_computer_sessions_for_host(&state.host_installation_id)
            .await
        else {
            return;
        };
        for session in sessions {
            let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            if let Ok(Some(ended)) = state
                .store
                .end_computer_session(
                    &session.id,
                    &session.owner_device_id,
                    Some(session.generation),
                    &now,
                )
                .await
            {
                let _ = state
                    .store
                    .release_computer_control_lease_for_session(&ended.id, &now)
                    .await;
                publish_session_event(state, &ended).await;
            }
        }
    }

    async fn clear_if(&self, session_id: &str) {
        let mut active = self.active.lock().await;
        if active
            .as_ref()
            .is_some_and(|value| value.session_id == session_id)
        {
            self.release_capabilities(session_id);
            active.take();
        }
    }

    async fn signaling_handle(
        &self,
        session_id: &str,
        owner_device_id: &str,
    ) -> Option<SignalingHandle> {
        let active = self.active.lock().await;
        let managed = active.as_ref().filter(|value| {
            value.session_id == session_id && value.owner_device_id == owner_device_id
        })?;
        Some(SignalingHandle {
            signaling: Arc::clone(&managed.signaling),
            commands: managed.commands.clone(),
            viewer_activity: managed.viewer_activity.clone(),
        })
    }
}

async fn reap_join(mut join: tokio::task::JoinHandle<()>, wait: Duration) {
    if timeout(wait, &mut join).await.is_err() {
        join.abort();
        let _ = join.await;
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelperStatus {
    state: String,
    #[serde(rename = "sessionID", alias = "sessionId")]
    session_id: Option<String>,
    generation: Option<u64>,
    #[serde(rename = "sourceID", alias = "sourceId")]
    source_id: Option<String>,
    #[serde(rename = "geometryRevision", alias = "geometry_revision")]
    geometry_revision: Option<u64>,
    reason: Option<String>,
    message: Option<String>,
    last_frame: Option<HelperFrameMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelperControlCapability {
    available: bool,
    provider: String,
    requires_accessibility: bool,
    #[serde(default)]
    window_control_available: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelperCapabilities {
    protocol_version: u64,
    control: HelperControlCapability,
}

fn verified_helper_control_capability(value: &serde_json::Value) -> bool {
    serde_json::from_value::<HelperCapabilities>(
        value
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map(|capability| {
        capability.protocol_version == 1
            && capability.control.available
            && match capability.control.provider.as_str() {
                // macOS helper: input is posted through Core Graphics and
                // needs the Accessibility grant.
                "core-graphics-v1" => capability.control.requires_accessibility,
                // Experimental Linux helper: input is injected into its own
                // X11 display (XTEST), which has no permission prompt.
                "xtest-x11-v1" => !capability.control.requires_accessibility,
                // Windows uses SendInput in the signed-in desktop session,
                // with per-session consent rather than macOS Accessibility.
                "sendinput-win32-v1" => !capability.control.requires_accessibility,
                _ => false,
            }
    })
    .unwrap_or(false)
}

/// The helper's simulator input answer for `source_id`. A helper that answers
/// capabilities without it (an older Cupola) cannot control simulators.
fn helper_simulator_control(value: &serde_json::Value, source_id: &str) -> SimulatorControl {
    let control = &value["result"]["simulatorControl"];
    let verified = value["result"]["protocolVersion"].as_u64() == Some(1)
        && control["sourceID"].as_str() == Some(source_id)
        && control["provider"].as_str() == Some("simulator-hid-v1")
        && control["requiresAccessibility"].as_bool() == Some(false);
    SimulatorControl {
        source_id: source_id.to_owned(),
        available: verified && control["available"].as_bool() == Some(true),
        reason: control["reason"]
            .as_str()
            .filter(|reason| reason.len() <= 64)
            .map(str::to_owned)
            .or_else(|| (!verified).then(|| "simulator_input_unsupported".to_owned())),
    }
}

fn verified_helper_quality_capability(value: &serde_json::Value) -> bool {
    let result = &value["result"];
    result["protocolVersion"].as_u64() == Some(1)
        && (result["capture"]["available"].as_bool() == Some(true)
            || (result["capture"]["available"].as_bool() == Some(false)
                && result["capture"]["reason"].as_str()
                    == Some("screen_recording_permission_required")))
        && result["capture"]["liveQualityChange"].as_bool() == Some(true)
}

fn verified_helper_window_control_capability(value: &serde_json::Value) -> bool {
    verified_helper_control_capability(value)
        && serde_json::from_value::<HelperCapabilities>(value["result"].clone()).is_ok_and(
            |capability| {
                capability
                    .control
                    .window_control_available
                    .unwrap_or(capability.control.provider != "core-graphics-v1")
            },
        )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelperSource {
    id: Option<String>,
    title: Option<String>,
    kind: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    scale: Option<f64>,
    content_rect: Option<HelperRect>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct HelperRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelperFrameMetadata {
    #[serde(rename = "sessionID", alias = "sessionId")]
    session_id: Option<String>,
    generation: Option<u64>,
    source_width: Option<u32>,
    source_height: Option<u32>,
    scale: Option<f64>,
}

fn command_line(id: u64, method: &str, params: serde_json::Value) -> Vec<u8> {
    let mut request = serde_json::json!({
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
    .into_bytes();
    request.push(b'\n');
    request
}

async fn write_helper_request(
    stdin: &mut HelperInput,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> std::io::Result<()> {
    let line = command_line(id, method, params);
    if line.len() > MAX_HELPER_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "helper request is too large",
        ));
    }
    stdin.write_all(&line).await?;
    stdin.flush().await
}

pub(crate) async fn read_bounded_line<R>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
) -> std::io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    read_line_within(reader, buffer, MAX_HELPER_LINE_BYTES).await
}

/// Cancel-safe: bytes of a partial line stay in `buffer` when the future is
/// dropped (the supervisor's select drops it whenever another branch wins),
/// and the next call continues that line. Errors discard the partial line.
async fn read_line_within<R>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let result = read_line_continuing(reader, buffer, limit).await;
    if result.is_err() {
        buffer.clear();
    }
    result
}

async fn read_line_continuing<R>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if buffer.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "helper emitted an unterminated line",
                ))
            };
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(chunk.len(), |index| index + 1);
        if buffer.len() + take > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "helper line is too large",
            ));
        }
        buffer.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if newline.is_some() {
            if buffer.last() == Some(&b'\n') {
                buffer.pop();
            }
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
            return Ok(Some(std::mem::take(buffer)));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn supervise_helper(
    supervisor: Arc<ComputerSessionSupervisor>,
    state: AppState,
    session: StoredComputerSession,
    handshake: String,
    helper: LaunchedHelper,
    mut stop_rx: tokio::sync::oneshot::Receiver<StopCommand>,
    mut revocations: tokio::sync::broadcast::Receiver<String>,
    mut command_rx: tokio::sync::mpsc::Receiver<HelperCommand>,
    mut viewer_activity_rx: tokio::sync::mpsc::Receiver<()>,
    timings: ViewTimings,
    signaling: Arc<tokio::sync::Mutex<SignalingState>>,
) {
    let LaunchedHelper {
        mut child,
        mut stdin,
        stdout,
        _codec_log,
        // Windows keeps the job in `helper` until supervision returns.
        ..
    } = helper;
    let common = serde_json::json!({
        "sessionID": session.id,
        "generation": session.generation,
        "handshake": handshake,
    });
    let prepare_params = common
        .clone()
        .as_object()
        .cloned()
        .map(|mut params| {
            params.insert("viewerCount".into(), serde_json::json!(1));
            params.insert("sourceID".into(), serde_json::json!(session.source_id));
            params.insert(
                "videoQuality".into(),
                serde_json::json!(session.video_quality),
            );
            params.insert(
                "videoMaxFramesPerSecond".into(),
                serde_json::json!(session.video_max_fps),
            );
            serde_json::Value::Object(params)
        })
        .unwrap_or_default();
    let pick_params = common.clone();
    let pick_params = if let Some(source_id) = session.source_id.as_deref() {
        pick_params
            .as_object()
            .cloned()
            .map(|mut params| {
                params.insert("sourceID".into(), serde_json::json!(source_id));
                serde_json::Value::Object(params)
            })
            .unwrap_or_default()
    } else {
        pick_params
    };
    let mut startup_pending = std::collections::BTreeSet::from(["2".to_owned(), "3".to_owned()]);
    let mut pending_control: BTreeMap<
        u64,
        tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    > = BTreeMap::new();
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::with_capacity(4096);
    let mut next_command_id = 4_u64;
    let startup_timer = tokio::time::sleep(HELPER_STARTUP_TIMEOUT);
    tokio::pin!(startup_timer);
    let mut viewer_activity_lease =
        ViewerActivityLease::with_duration(timings.viewer_activity_lease);
    let mut opening_deadline = OpeningDeadline::with_budget(timings.opening_deadline);
    let mut viewer_activity_open = true;
    let mut peer_recovery = PeerRecoveryTimer::with_grace(PEER_RECOVERY_GRACE);
    let mut intentional_stop = None;
    let started = std::time::Instant::now();
    let mut open_timeline = OpenTimeline::default();
    // Set once the helper process has exited; reading then continues until
    // its output ends or the drain deadline passes.
    let mut helper_exit: Option<Option<std::process::ExitStatus>> = None;
    let exit_drain = tokio::time::sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(exit_drain);

    // A simulator session asks about its exact device; other sessions send
    // the same request as before.
    let simulator_source = session
        .source_id
        .clone()
        .filter(|_| session.source_kind.as_deref() == Some("simulator"));
    let capabilities_params = match simulator_source.as_deref() {
        Some(source_id) => serde_json::json!({ "handshake": handshake, "sourceID": source_id }),
        None => serde_json::json!({ "handshake": handshake }),
    };
    if write_helper_request(&mut stdin, 1, "capabilities", capabilities_params)
        .await
        .is_err()
        || write_helper_request(&mut stdin, 2, "capture.prepare", prepare_params)
            .await
            .is_err()
        || write_helper_request(&mut stdin, 3, "capture.pick", pick_params)
            .await
            .is_err()
    {
        fail_session(&state, &session, "helper_write_failed").await;
        intentional_stop = Some(StopCommand {
            reason: "helper_write_failed".into(),
            end_session: false,
        });
    }

    while intentional_stop.is_none() {
        let (interrupted_since, phase) = {
            let current = signaling.lock().await;
            (current.peer_interrupted_since, current.phase())
        };
        opening_deadline.sync(phase);
        let was_interrupted = peer_recovery.armed();
        peer_recovery.sync(interrupted_since);
        if !was_interrupted && peer_recovery.armed() {
            // Nothing the phone sends can arrive now: end control and let go
            // of held keys and buttons instead of waiting for the lease.
            let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            let _ = state
                .store
                .release_computer_control_lease_for_session(&session.id, &now)
                .await;
            let _ = write_helper_request(
                &mut stdin,
                next_command_id,
                "control.releaseAll",
                common.clone(),
            )
            .await;
            next_command_id = next_command_id.saturating_add(1);
        }
        tokio::select! {
            status = child.wait(), if helper_exit.is_none() => {
                helper_exit = Some(status.ok());
                exit_drain.as_mut().reset(tokio::time::Instant::now() + HELPER_EXIT_DRAIN);
            }
            _ = &mut exit_drain, if helper_exit.is_some() => break,
            line_result = read_line_within(&mut reader, &mut line, MAX_SESSION_HELPER_LINE_BYTES) => {
                match line_result {
                    Ok(None) => break,
                    Ok(Some(raw)) => {
                        match serde_json::from_slice::<serde_json::Value>(&raw) {
                            Ok(value) => {
                                if open_timeline.observe(&value, started.elapsed()) {
                                    open_timeline.log(&state, &session);
                                }
                                if let Some(reason) = refused_startup_pick(&value, &startup_pending) {
                                    // The chosen window or display could not be
                                    // selected (it closed before sharing began).
                                    // Nothing else will move the view on.
                                    fail_session(&state, &session, &reason).await;
                                    intentional_stop = Some(StopCommand { reason: "capture_pick_refused".into(), end_session: false });
                                    break;
                                }
                                if value.get("id").and_then(serde_json::Value::as_u64) == Some(1) {
                                    // Another session's late report must not change the live view.
                                    if supervisor.set_session_capabilities(&session.id, &value) {
                                        if let Some(source_id) = simulator_source.as_deref() {
                                            supervisor.set_simulator_control(Some(helper_simulator_control(&value, source_id)));
                                            // The answer can follow the view going live; tell the phone.
                                            if let Ok(Some(current)) = state.store.computer_session(&session.id).await {
                                                publish_session_event(&state, &current).await;
                                            }
                                        }
                                    }
                                    signaling.lock().await.clipboard_capable = Some(value.pointer("/result/clipboard/version")
                                        .and_then(serde_json::Value::as_u64) == Some(1));
                                    // Older helpers do not know capability
                                    // negotiation; that response is optional
                                    // so view-only startup remains compatible.
                                    if value.get("error").is_some() {
                                        continue;
                                    }
                                }
                                if let Some(id) = value.get("id").and_then(helper_id) {
                                    startup_pending.remove(&id);
                                }
                                if let Some(id) = value.get("id").and_then(serde_json::Value::as_u64) {
                                    if let Some(response) = pending_control.remove(&id) {
                                        let result = if let Some(error) = value
                                            .get("error")
                                            .and_then(|error| error.get("code"))
                                            .and_then(serde_json::Value::as_str)
                                        {
                                            Err(error.to_owned())
                                        } else {
                                            value
                                                .get("result")
                                                .cloned()
                                                .ok_or_else(|| "helper response was invalid".to_owned())
                                        };
                                        let _ = response.send(result);
                                        continue;
                                    }
                                }
                                match apply_helper_value_with_signaling(&state, &session, &value, &signaling).await {
                                    Ok(HelperValueAction::ControlRevoked {
                                        lease_id,
                                        request_id,
                                        generation,
                                        geometry_revision,
                                        source_id,
                                        reason,
                                    }) => {
                                        {
                                            let mut current = signaling.lock().await;
                                            current.text_input_focused = None;
                                            current.revoked_lease = Some((lease_id.clone(), reason));
                                        }
                                        if let Ok(Some(lease)) = state.store.computer_control_lease(&lease_id).await {
                                            if lease.client_request_id == request_id
                                                && lease.session_id == session.id
                                                && lease.owner_device_id == session.owner_device_id
                                                && lease.host_installation_id == session.host_installation_id
                                                && lease.conversation_id == session.conversation_id
                                                && lease.session_generation == generation
                                                && ((lease.geometry_revision == geometry_revision && lease.source_id == source_id)
                                                    // A local Stop can arrive after helper rebind but
                                                    // before its durable source commit. It still owns
                                                    // this exact session/lease/consent and must win.
                                                    || (signaling.lock().await.source_rebinding
                                                        && geometry_revision > lease.geometry_revision))
                                            {
                                                let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
                                                let _ = state
                                                    .store
                                                    .release_computer_control_lease(&lease_id, &session.owner_device_id, &now)
                                                    .await;
                                            }
                                        }
                                    }
                                    Ok(HelperValueAction::StartCapture) => {
                                        let start_params = common.clone();
                                        let _ = write_helper_request(&mut stdin, next_command_id, "capture.start", start_params).await;
                                        next_command_id = next_command_id.saturating_add(1);
                                    }
                                    Ok(HelperValueAction::StopCapture { reason }) => {
                                        let mut stop_params = common.clone();
                                        if let Some(object) = stop_params.as_object_mut() {
                                            object.insert("reason".into(), serde_json::Value::String(reason.clone()));
                                        }
                                        let _ = write_helper_request(&mut stdin, next_command_id, "capture.stop", stop_params).await;
                                        intentional_stop = Some(StopCommand { reason, end_session: false });
                                        break;
                                    }
                                    Ok(HelperValueAction::None) => {}
                                    Ok(HelperValueAction::ProtocolError) => {
                                        fail_session(&state, &session, "helper_protocol_error").await;
                                        intentional_stop = Some(StopCommand { reason: "helper_protocol_error".into(), end_session: false });
                                        break;
                                    }
                                    Err(_) => {
                                        fail_session(&state, &session, "helper_state_update_failed").await;
                                        intentional_stop = Some(StopCommand { reason: "helper_state_update_failed".into(), end_session: false });
                                        break;
                                    }
                                }
                            }
                            Err(_) => {
                                fail_session(&state, &session, "helper_invalid_json").await;
                                intentional_stop = Some(StopCommand { reason: "helper_invalid_json".into(), end_session: false });
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        let reason = if error.kind() == std::io::ErrorKind::InvalidData {
                            "helper_line_too_large"
                        } else {
                            "helper_read_failed"
                        };
                        fail_session(&state, &session, reason).await;
                        intentional_stop = Some(StopCommand { reason: reason.into(), end_session: false });
                        break;
                    }
                }
            }
            stop = &mut stop_rx => {
                intentional_stop = Some(stop.unwrap_or(StopCommand { reason: "supervisor_dropped".into(), end_session: true }));
                break;
            }
            command = command_rx.recv() => {
                let Some(command) = command else {
                    intentional_stop = Some(StopCommand { reason: "signaling_channel_closed".into(), end_session: true });
                    break;
                };
                if let HelperCommand::Control { method, params, response } = command {
                    let mut full_params = common.clone();
                    if let (Some(full), Some(extra)) = (full_params.as_object_mut(), params.as_object()) {
                        full.extend(extra.clone());
                    }
                    let command_id = next_command_id;
                    pending_control.insert(command_id, response);
                    if write_helper_request(&mut stdin, command_id, &method, full_params).await.is_err() {
                        if let Some(response) = pending_control.remove(&command_id) {
                            let _ = response.send(Err("helper_write_failed".into()));
                        }
                        fail_session(&state, &session, "helper_write_failed").await;
                        intentional_stop = Some(StopCommand { reason: "helper_write_failed".into(), end_session: false });
                        break;
                    }
                    next_command_id = next_command_id.saturating_add(1);
                    continue;
                }
                let (method, params, delivered) = match command {
                    HelperCommand::RemoteAnswer { peer_revision, sdp, delivered } => {
                        let mut params = common.clone();
                        if let Some(object) = params.as_object_mut() {
                            object.insert("peerRevision".into(), serde_json::Value::String(peer_revision));
                            object.insert("sdp".into(), serde_json::Value::String(sdp));
                        }
                        ("signal.answer", params, delivered)
                    }
                    HelperCommand::RemoteCandidate { peer_revision, sequence, candidate, sdp_mid, sdp_m_line_index, username_fragment, delivered } => {
                        let mut params = common.clone();
                        if let Some(object) = params.as_object_mut() {
                            object.insert("peerRevision".into(), serde_json::Value::String(peer_revision));
                            object.insert("sequence".into(), serde_json::json!(sequence));
                            object.insert("candidate".into(), serde_json::Value::String(candidate));
                            object.insert("sdpMid".into(), sdp_mid.map_or(serde_json::Value::Null, serde_json::Value::String));
                            object.insert("sdpMLineIndex".into(), serde_json::json!(sdp_m_line_index));
                            object.insert("usernameFragment".into(), username_fragment.map_or(serde_json::Value::Null, serde_json::Value::String));
                        }
                        ("signal.candidate", params, delivered)
                    }
                    HelperCommand::CloseSignal { peer_revision, reason, delivered } => {
                        let mut params = common.clone();
                        if let Some(object) = params.as_object_mut() {
                            object.insert("peerRevision".into(), serde_json::Value::String(peer_revision));
                            object.insert("reason".into(), serde_json::Value::String(reason));
                        }
                        ("signal.close", params, delivered)
                    }
                    HelperCommand::Control { .. } => unreachable!(),
                };
                let was_delivered = write_helper_request(&mut stdin, next_command_id, method, params.clone()).await.is_ok();
                {
                    let mut current = signaling.lock().await;
                    match method {
                        "signal.answer" => {
                            if let Some(sdp) = params.get("sdp").and_then(serde_json::Value::as_str) {
                                if was_delivered { current.commit_answer(sdp); } else { current.rollback_answer(sdp); }
                            }
                        }
                        "signal.candidate" => {
                            if let Some(sequence) = params.get("sequence").and_then(serde_json::Value::as_u64) {
                                if was_delivered { current.commit_candidate(sequence); } else { current.rollback_candidate(sequence); }
                            }
                        }
                        "signal.close" if was_delivered => {
                            current.closed = true;
                            current.peer_state = "closed".into();
                            let reason = params.get("reason").and_then(serde_json::Value::as_str).unwrap_or("closed_by_viewer");
                            current.clear_ephemeral(Some(reason.to_owned()));
                        }
                        _ => {}
                    }
                }
                let _ = delivered.send(was_delivered);
                if !was_delivered {
                    fail_session(&state, &session, "helper_write_failed").await;
                    intentional_stop = Some(StopCommand { reason: "helper_write_failed".into(), end_session: false });
                    break;
                }
                next_command_id = next_command_id.saturating_add(1);
            }
            activity = viewer_activity_rx.recv(), if viewer_activity_open => {
                if activity.is_some() {
                    viewer_activity_lease.reset();
                } else {
                    viewer_activity_open = false;
                }
            }
            revoked = revocations.recv() => {
                match revoked {
                    Ok(device_id) if device_id == session.owner_device_id => {
                        intentional_stop = Some(StopCommand { reason: "owner_revoked".into(), end_session: true });
                        break;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
                    | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        intentional_stop = Some(StopCommand { reason: "revocation_channel_unavailable".into(), end_session: true });
                        break;
                    }
                }
            }
            _ = peer_recovery.timer.as_mut(), if peer_recovery.armed() => {
                let expired = {
                    let mut current = signaling.lock().await;
                    // A reconnect that raced the deadline wins.
                    let expired = current.peer_interrupted_since.is_some();
                    if expired {
                        current.fail("peer_connection_disconnected".into());
                    }
                    expired
                };
                if !expired {
                    continue;
                }
                if reconcile_media_state(&state, &session, &signaling).await.is_err() {
                    fail_session(&state, &session, "helper_state_update_failed").await;
                }
                let mut stop_params = common.clone();
                if let Some(object) = stop_params.as_object_mut() {
                    object.insert("reason".into(), serde_json::Value::String("peer_connection_terminal".into()));
                }
                let _ = write_helper_request(&mut stdin, next_command_id, "capture.stop", stop_params).await;
                intentional_stop = Some(StopCommand { reason: "peer_connection_disconnected".into(), end_session: false });
                break;
            }
            _ = opening_deadline.timer.as_mut(), if opening_deadline.armed => {
                // Polling, answers and candidates all wake this loop, but
                // only a phase change moves the deadline, so reaching it with
                // the view still opening means no picture is coming. A frame
                // that raced the deadline wins.
                let stalled = {
                    let mut current = signaling.lock().await;
                    let stalled = current.phase().is_opening();
                    if stalled {
                        current.fail("first_frame_timeout".into());
                    }
                    stalled
                };
                if !stalled {
                    continue;
                }
                if reconcile_media_state(&state, &session, &signaling).await.is_err() {
                    fail_session(&state, &session, "helper_state_update_failed").await;
                }
                let mut stop_params = common.clone();
                if let Some(object) = stop_params.as_object_mut() {
                    object.insert("reason".into(), serde_json::Value::String("first_frame_timeout".into()));
                }
                let _ = write_helper_request(&mut stdin, next_command_id, "capture.stop", stop_params).await;
                intentional_stop = Some(StopCommand { reason: "first_frame_timeout".into(), end_session: false });
                break;
            }
            _ = &mut startup_timer, if !startup_pending.is_empty() => {
                fail_session(&state, &session, "helper_startup_timeout").await;
                intentional_stop = Some(StopCommand { reason: "helper_startup_timeout".into(), end_session: false });
                break;
            }
            _ = viewer_activity_lease.timer.as_mut() => {
                // A valid request can race the deadline after coalescing into
                // the one-slot channel. Honor that activity before failing
                // closed instead of expiring a viewer that is still polling.
                if viewer_activity_rx.try_recv().is_ok() {
                    viewer_activity_lease.reset();
                    continue;
                }
                intentional_stop = Some(StopCommand { reason: "viewer_timeout".into(), end_session: true });
                break;
            }
        }
    }

    for (_, response) in pending_control {
        let _ = response.send(Err("computer helper stopped".into()));
    }

    let stage = if !startup_pending.is_empty() {
        "startup"
    } else if signaling.lock().await.shared {
        "sharing"
    } else {
        "connecting"
    };
    let (closed_by, exit) = if let Some(stop) = intentional_stop {
        let exit = stop_child(&mut child, &mut stdin, &handshake).await;
        if stop.end_session {
            end_session_for_owner(&state, &session, &stop.reason).await;
        }
        (stop.reason, exit)
    } else if let Some(exit) = helper_exit {
        fail_session(&state, &session, "helper_exited").await;
        ("helper_exited".to_owned(), exit)
    } else {
        // Output ended while the process may still run. Never wait on it
        // unbounded: that would keep the only computer view taken.
        match timeout(HELPER_OUTPUT_CLOSED_GRACE, child.wait()).await {
            Ok(status) => {
                fail_session(&state, &session, "helper_exited").await;
                ("helper_exited".to_owned(), status.ok())
            }
            Err(_) => {
                fail_session(&state, &session, "helper_output_closed").await;
                let exit = stop_child(&mut child, &mut stdin, &handshake).await;
                ("helper_output_closed".to_owned(), exit)
            }
        }
    };
    // The record comes before the slot frees: whoever sees the view gone
    // (the next view, a test, a diagnostics reader) finds why it ended.
    log_session_end(&state, &session, stage, &closed_by, exit, started.elapsed()).await;
    supervisor.clear_if(&session.id).await;
}

/// The reason a helper refused the startup `capture.pick` (id 3).
fn refused_startup_pick(
    value: &serde_json::Value,
    startup_pending: &std::collections::BTreeSet<String>,
) -> Option<String> {
    if value.get("id").and_then(helper_id).as_deref() != Some("3") || !startup_pending.contains("3")
    {
        return None;
    }
    // Python reports a missing X11 desktop as an RPC error; native helpers
    // report an unaccepted pick. Both carry the same actionable reason. Do not
    // turn the former into a generic protocol/helper failure on the phone.
    let error = value
        .pointer("/error/code")
        .and_then(serde_json::Value::as_str);
    if error.is_none() && value.pointer("/result/accepted") != Some(&serde_json::Value::Bool(false))
    {
        return None;
    }
    Some(
        error
            .or_else(|| {
                value
                    .pointer("/result/errorCode")
                    .and_then(serde_json::Value::as_str)
            })
            .filter(|code| diagnostic_code(code) == *code)
            .unwrap_or("source_not_found")
            .to_owned(),
    )
}

/// A helper or host reason code as it may appear in the daemon log: short,
/// with no spaces, so no sentence, title or path can pass for one.
fn diagnostic_code(reason: &str) -> &str {
    if (1..=128).contains(&reason.len())
        && reason
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        reason
    } else {
        "unlisted"
    }
}

/// Milliseconds from launching the helper to each startup hop (the first time
/// each is seen), so a slow opening names where it waited. Logged once, when
/// the phone's link is up and a frame has been produced.
#[derive(Default)]
struct OpenTimeline {
    hops: BTreeMap<&'static str, u64>,
    logged: bool,
}

impl OpenTimeline {
    fn hop_of(value: &serde_json::Value) -> Option<&'static str> {
        if value.get("id").and_then(serde_json::Value::as_u64) == Some(1) {
            return Some("capabilities");
        }
        match value.get("event").and_then(serde_json::Value::as_str)? {
            "capture.sourceSelected" => Some("sourceSelected"),
            "signal.offer" => Some("offer"),
            "capture.frameMetadata" => Some("firstFrame"),
            "signal.peerState"
                if matches!(
                    value.get("state").and_then(serde_json::Value::as_str),
                    Some("connected" | "completed")
                ) =>
            {
                Some("peerConnected")
            }
            _ => None,
        }
    }

    /// Records the hop `value` is, if new. True once, the moment the opening
    /// is complete: connected and a frame produced.
    fn observe(&mut self, value: &serde_json::Value, elapsed: Duration) -> bool {
        if let Some(hop) = Self::hop_of(value) {
            self.hops
                .entry(hop)
                .or_insert_with(|| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
        }
        let complete = !self.logged
            && self.hops.contains_key("peerConnected")
            && self.hops.contains_key("firstFrame");
        self.logged |= complete;
        complete
    }

    fn log(&self, state: &AppState, session: &StoredComputerSession) {
        let _ = state.logger.record(
            "info",
            "computer_session_opened",
            serde_json::json!({
                "session": session.id,
                "sourceKind": session.source_kind.as_deref().map(diagnostic_code),
                "hopsMs": self.hops,
            }),
        );
    }
}

/// One content-free record per computer view that stops: its first stored
/// cause, the stage it reached, what closed it and how the helper exited.
/// Never titles, SDP, helper output or per-frame data.
async fn log_session_end(
    state: &AppState,
    session: &StoredComputerSession,
    stage: &str,
    closed_by: &str,
    exit: Option<std::process::ExitStatus>,
    elapsed: Duration,
) {
    let current = state
        .store
        .computer_session(&session.id)
        .await
        .ok()
        .flatten();
    let reason = current
        .as_ref()
        .and_then(|current| current.failure_reason.as_deref())
        .map(diagnostic_code);
    let mut fields = serde_json::json!({
        "session": session.id,
        "stage": stage,
        "reason": reason,
        "closedBy": diagnostic_code(closed_by),
        "elapsedMs": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        "sourceKind": session.source_kind.as_deref().map(diagnostic_code),
        "quality": diagnostic_code(&session.video_quality),
        "platform": std::env::consts::OS,
    });
    if let Some(code) = exit.and_then(|exit| exit.code()) {
        fields["helperExitCode"] = serde_json::json!(code);
    }
    #[cfg(unix)]
    if let Some(signal) = exit.and_then(|exit| {
        use std::os::unix::process::ExitStatusExt;
        exit.signal()
    }) {
        fields["helperSignal"] = serde_json::json!(signal);
    }
    let _ = state.logger.record(
        if reason.is_some() { "warn" } else { "info" },
        "computer_session_ended",
        fields,
    );
}

fn helper_id(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) if !value.is_empty() => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HelperValueAction {
    None,
    ControlRevoked {
        lease_id: String,
        request_id: String,
        generation: u64,
        geometry_revision: u64,
        source_id: Option<String>,
        reason: String,
    },
    StartCapture,
    StopCapture {
        reason: String,
    },
    ProtocolError,
}

#[cfg(all(test, unix))]
async fn apply_helper_value(
    state: &AppState,
    session: &StoredComputerSession,
    value: &serde_json::Value,
) -> Result<bool, sqlx::Error> {
    let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
    Ok(!matches!(
        apply_helper_value_with_signaling(state, session, value, &signaling).await?,
        HelperValueAction::ProtocolError
    ))
}

async fn apply_helper_value_with_signaling(
    state: &AppState,
    session: &StoredComputerSession,
    value: &serde_json::Value,
    signaling: &Arc<tokio::sync::Mutex<SignalingState>>,
) -> Result<HelperValueAction, sqlx::Error> {
    if value.get("error").is_some() {
        return Ok(HelperValueAction::ProtocolError);
    }

    if let Some(event) = value.get("event").and_then(serde_json::Value::as_str) {
        let event_session = value.get("sessionID").and_then(serde_json::Value::as_str);
        let event_generation = value.get("generation").and_then(serde_json::Value::as_u64);
        let event_peer_revision = value
            .get("peerRevision")
            .and_then(serde_json::Value::as_str);
        if event.starts_with("signal.")
            && (event_session.is_none()
                || event_generation.is_none()
                || event_peer_revision.is_none()
                || !event_peer_revision.is_some_and(|revision| valid_signaling_text(revision, 128)))
        {
            return Ok(HelperValueAction::ProtocolError);
        }
        if (event_session.is_some() || event_generation.is_some())
            && (event_session != Some(session.id.as_str())
                || event_generation != Some(session.generation))
        {
            return Ok(HelperValueAction::None);
        }
        if event.starts_with("signal.") && event != "signal.peerReady" {
            let current = signaling.lock().await;
            if current.peer_revision.is_some()
                && current.peer_revision.as_deref() != event_peer_revision
            {
                return Ok(HelperValueAction::None);
            }
        }
        match event {
            "control.textFocus" => {
                let text = |key: &str, maximum: usize| {
                    value
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .filter(|text| !text.is_empty() && valid_signaling_text(text, maximum))
                };
                let (
                    Some(lease_id),
                    Some(request_id),
                    true,
                    Some(geometry_revision),
                    Some(text_input),
                ) = (
                    text("leaseID", 128),
                    text("requestID", 160),
                    event_session.is_some(),
                    value
                        .get("geometryRevision")
                        .and_then(serde_json::Value::as_u64),
                    value.get("textInput").and_then(serde_json::Value::as_bool),
                )
                else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                // The computer reads focus while control may be ending. Only
                // the session's live lease moves the phone's keyboard.
                let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
                let live = matches!(
                    state.store.computer_control_lease(lease_id).await,
                    Ok(Some(lease)) if lease.status == "active"
                        && lease.expires_at > now
                        && lease.session_id == session.id
                        && lease.session_generation == session.generation
                        && lease.client_request_id == request_id
                );
                let mut current = signaling.lock().await;
                let revoked = current
                    .revoked_lease
                    .as_ref()
                    .is_some_and(|(revoked, _)| revoked == lease_id);
                if !live || revoked {
                    // Control has ended; there is nothing to type into.
                    current.text_input_focused = None;
                } else if geometry_revision >= current.source_binding_revision {
                    // A report from the view before a window switch is stale.
                    current.text_input_focused = Some(text_input);
                }
                return Ok(HelperValueAction::None);
            }
            "control.revoked" => {
                if event_session != Some(session.id.as_str())
                    || event_generation != Some(session.generation)
                {
                    return Ok(HelperValueAction::ProtocolError);
                }
                let Some(reason) = value
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .filter(|reason| valid_signaling_text(reason, 128))
                else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                let Some(lease_id) = value
                    .get("leaseID")
                    .and_then(serde_json::Value::as_str)
                    .filter(|lease_id| valid_signaling_text(lease_id, 128))
                else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                if lease_id.is_empty() {
                    return Ok(HelperValueAction::ProtocolError);
                }
                let Some(request_id) = value
                    .get("requestID")
                    .and_then(serde_json::Value::as_str)
                    .filter(|request_id| valid_signaling_text(request_id, 160))
                else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                let Some(geometry_revision) = value
                    .get("geometryRevision")
                    .and_then(serde_json::Value::as_u64)
                else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                let source_id = match value.get("sourceID") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(source_id))
                        if valid_signaling_text(source_id, 160) =>
                    {
                        Some(source_id.clone())
                    }
                    _ => return Ok(HelperValueAction::ProtocolError),
                };
                return Ok(HelperValueAction::ControlRevoked {
                    lease_id: lease_id.to_owned(),
                    request_id: request_id.to_owned(),
                    generation: session.generation,
                    geometry_revision,
                    source_id,
                    reason: reason.to_owned(),
                });
            }
            "signal.peerReady" => {
                let mut current = signaling.lock().await;
                if current.peer_revision.is_none() {
                    current.peer_revision = event_peer_revision.map(str::to_owned);
                    current.closed = false;
                    current.peer_state = "new".into();
                    current.peer_interrupted_since = None;
                    current.first_frame_observed = false;
                }
                return Ok(HelperValueAction::None);
            }
            "signal.offer" => {
                let Some(sdp) = value.get("sdp").and_then(serde_json::Value::as_str) else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                if value.get("type").and_then(serde_json::Value::as_str) != Some("offer")
                    || !valid_sdp(sdp)
                {
                    return Ok(HelperValueAction::ProtocolError);
                }
                let mut current = signaling.lock().await;
                if current.closed {
                    return Ok(HelperValueAction::None);
                }
                if current.peer_revision.is_none() {
                    current.peer_revision = event_peer_revision.map(str::to_owned);
                }
                let offer = SignalingDescription {
                    kind: "offer".into(),
                    sdp: tailscale_last_in_sdp(sdp).into_owned(),
                };
                if let Some(existing) = current.offer.as_ref() {
                    if existing != &offer {
                        return Ok(HelperValueAction::ProtocolError);
                    }
                    return Ok(HelperValueAction::None);
                }
                current.offer = Some(offer);
                return Ok(HelperValueAction::None);
            }
            "signal.candidate" => {
                let Some(candidate) = value.get("candidate").and_then(serde_json::Value::as_str)
                else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                let sdp_m_line_index = value
                    .get("sdpMLineIndex")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok());
                let Some(sdp_m_line_index) = sdp_m_line_index else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                if sdp_m_line_index > i32::MAX as u32 {
                    return Ok(HelperValueAction::ProtocolError);
                }
                if candidate.len() > MAX_SIGNALING_CANDIDATE_BYTES
                    || !candidate.starts_with("candidate:")
                    || candidate.chars().any(char::is_control)
                {
                    return Ok(HelperValueAction::ProtocolError);
                }
                let sdp_mid = value
                    .get("sdpMid")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                if sdp_mid
                    .as_deref()
                    .is_some_and(|value| !valid_signaling_text(value, 128))
                {
                    return Ok(HelperValueAction::ProtocolError);
                }
                let username_fragment = value
                    .get("usernameFragment")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                if username_fragment
                    .as_deref()
                    .is_some_and(|value| !valid_signaling_text(value, 256))
                {
                    return Ok(HelperValueAction::ProtocolError);
                }
                let candidate = tailscale_last(candidate);
                let mut current = signaling.lock().await;
                if current.local_candidates.iter().any(|existing| {
                    existing.candidate == candidate
                        && existing.sdp_mid == sdp_mid
                        && existing.sdp_m_line_index == sdp_m_line_index
                        && existing.username_fragment == username_fragment
                }) {
                    return Ok(HelperValueAction::None);
                }
                if current.local_candidates.len() >= MAX_SIGNALING_CANDIDATES {
                    return Ok(HelperValueAction::ProtocolError);
                }
                let sequence = current
                    .local_candidates
                    .back()
                    .map_or(1, |candidate| candidate.sequence.saturating_add(1));
                current.local_candidates.push_back(SignalingCandidate {
                    sequence,
                    candidate: candidate.into_owned(),
                    sdp_mid,
                    sdp_m_line_index,
                    username_fragment,
                });
                return Ok(HelperValueAction::None);
            }
            "signal.peerState" => {
                let Some(peer_state) = value.get("state").and_then(serde_json::Value::as_str)
                else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                if !matches!(
                    peer_state,
                    "new"
                        | "checking"
                        | "connected"
                        | "completed"
                        | "failed"
                        | "disconnected"
                        | "closed"
                        | "unknown"
                ) {
                    return Ok(HelperValueAction::ProtocolError);
                }
                // `disconnected` is transient: WebRTC keeps checking and
                // usually reconnects after a roam. The supervisor's grace
                // timer, not this report, ends a view that does not recover.
                let terminal = matches!(peer_state, "failed" | "closed");
                {
                    let mut current = signaling.lock().await;
                    current.peer_state = peer_state.to_owned();
                    match peer_state {
                        "connected" | "completed" => current.peer_interrupted_since = None,
                        "disconnected" => {
                            current
                                .peer_interrupted_since
                                .get_or_insert_with(tokio::time::Instant::now);
                        }
                        _ => {}
                    }
                    if terminal {
                        current.fail(match peer_state {
                            "closed" => "peer_connection_closed".into(),
                            _ => "peer_connection_failed".into(),
                        });
                    }
                }
                reconcile_media_state(state, session, signaling).await?;
                if terminal {
                    return Ok(HelperValueAction::StopCapture {
                        reason: "peer_connection_terminal".into(),
                    });
                }
                return Ok(HelperValueAction::None);
            }
            "signal.failed" => {
                let Some(reason) = value
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .filter(|reason| valid_signaling_text(reason, 128))
                else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                signaling.lock().await.fail(reason.to_owned());
                reconcile_media_state(state, session, signaling).await?;
                return Ok(HelperValueAction::StopCapture {
                    reason: "peer_connection_failed".into(),
                });
            }
            "signal.closed" => {
                let Some(reason) = value
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .filter(|reason| valid_signaling_text(reason, 128))
                else {
                    return Ok(HelperValueAction::ProtocolError);
                };
                let mut current = signaling.lock().await;
                current.closed = true;
                current.peer_state = "closed".into();
                current.clear_ephemeral(Some(reason.to_owned()));
                drop(current);
                reconcile_media_state(state, session, signaling).await?;
                return Ok(HelperValueAction::StopCapture {
                    reason: "publisher_closed".into(),
                });
            }
            "signal.answerApplied" => return Ok(HelperValueAction::None),
            _ if event.starts_with("signal.") => return Ok(HelperValueAction::ProtocolError),
            _ => {}
        }
    }
    let status_value = value
        .get("result")
        .and_then(|result| result.get("status"))
        .or_else(|| value.get("status"));
    let Some(status_value) = status_value else {
        return Ok(
            if value.get("id").is_some() || value.get("event").is_some() {
                HelperValueAction::None
            } else {
                HelperValueAction::ProtocolError
            },
        );
    };
    let Ok(status) = serde_json::from_value::<HelperStatus>(status_value.clone()) else {
        return Ok(HelperValueAction::ProtocolError);
    };
    // A late event from a previous helper/session is ignored, never applied to
    // the current durable row.
    if status.session_id.as_deref() != Some(session.id.as_str())
        || status.generation != Some(session.generation)
    {
        return Ok(HelperValueAction::None);
    }
    if !matches!(
        status.state.as_str(),
        "awaitingSource"
            | "ready"
            | "starting"
            | "capturing"
            | "paused"
            | "suspended"
            | "idle"
            | "permissionDenied"
            | "sourceRemoved"
            | "failed"
            | "stopped"
            | "helperRestarted"
    ) {
        return Ok(HelperValueAction::ProtocolError);
    }
    if signaling.lock().await.source_rebinding {
        return Ok(HelperValueAction::None);
    }
    // Supervision outlives a view. Compare against the durable current binding,
    // not the source snapshot with which this helper was originally launched.
    let Some(current_session) = state.store.computer_session(&session.id).await? else {
        return Ok(HelperValueAction::None);
    };
    if current_session.state == "ended"
        || current_session.state == "failed"
        || status
            .geometry_revision
            .is_some_and(|revision| revision < current_session.geometry_revision)
        // Preparation can publish the revision before its first source is
        // selected. Only an established binding makes this mismatch stale.
        || (current_session.source_id.is_some()
            && status.geometry_revision == Some(current_session.geometry_revision)
            && status.source_id.is_some()
            && status.source_id != current_session.source_id)
    {
        return Ok(HelperValueAction::None);
    }
    let session = &current_session;
    let frame_metadata =
        value.get("event").and_then(serde_json::Value::as_str) == Some("capture.frameMetadata");
    let identity_valid_frame_metadata =
        frame_metadata && frame_metadata_matches_session(status.last_frame.as_ref(), session);
    let capture_prepared =
        value.get("event").and_then(serde_json::Value::as_str) == Some("capture.prepared");
    // The helper reports `awaitingSource` once prepared, before it applies the
    // pick the host sends right after. A session that already names its
    // source is still preparing then: relaying it would briefly tell the phone
    // to choose what to share on the Mac. The pick ends as `ready` or
    // `failed` (e.g. `window_not_found`) within a fraction of a second.
    let capture_state = if status.state == "awaitingSource" && session.source_id.is_some() {
        "preparing"
    } else {
        status.state.as_str()
    };
    let capture_changed = {
        let mut current = signaling.lock().await;
        // Recheck after the database await: a rebind may have completed while
        // this old event was reading its previous source snapshot.
        if current.source_rebinding
            || status.geometry_revision.unwrap_or(0) < current.source_binding_revision
        {
            return Ok(HelperValueAction::None);
        }
        let changed = capture_prepared
            || current.capture_state != capture_state
            || (identity_valid_frame_metadata
                && (!current.first_frame_observed
                    || current.frame_geometry_revision != status.geometry_revision));
        if capture_prepared {
            current.clear_ephemeral(None);
            current.closed = false;
            current.peer_state = "new".into();
        }
        if identity_valid_frame_metadata {
            current.first_frame_observed = true;
            current.frame_geometry_revision = status.geometry_revision;
        }
        current.capture_state = capture_state.to_owned();
        if matches!(
            status.state.as_str(),
            "permissionDenied" | "sourceRemoved" | "failed" | "stopped" | "helperRestarted"
        ) {
            current.fail(
                status
                    .reason
                    .as_deref()
                    .or(status.message.as_deref())
                    .unwrap_or("helper_capture_failed")
                    .to_owned(),
            );
        }
        changed
    };
    if frame_metadata && !capture_changed {
        return Ok(HelperValueAction::None);
    }
    let (state_value, failure_reason) = signaling.lock().await.desired_state();
    let source = value
        .get("source")
        .and_then(|value| serde_json::from_value::<HelperSource>(value.clone()).ok());
    let frame = identity_valid_frame_metadata
        .then_some(status.last_frame.as_ref())
        .flatten();
    let content_rect_json = source
        .as_ref()
        .and_then(|value| value.content_rect.as_ref())
        .and_then(|value| serde_json::to_string(value).ok());
    let updated = state
        .store
        .transition_computer_session(
            &session.id,
            session.generation,
            state_value,
            status
                .source_id
                .as_deref()
                .or_else(|| source.as_ref().and_then(|value| value.id.as_deref())),
            source.as_ref().and_then(|value| value.title.as_deref()),
            source.as_ref().and_then(|value| value.kind.as_deref()),
            frame
                .and_then(|value| value.source_width)
                .or_else(|| source.as_ref().and_then(|value| value.width)),
            frame
                .and_then(|value| value.source_height)
                .or_else(|| source.as_ref().and_then(|value| value.height)),
            frame
                .and_then(|value| value.scale)
                .or_else(|| source.as_ref().and_then(|value| value.scale)),
            content_rect_json.as_deref(),
            status.geometry_revision.unwrap_or(0),
            failure_reason.as_deref(),
            &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        )
        .await?;
    // A terminal capture state retires the helper now rather than at the
    // viewer timeout, so a retry can open the next view at once.
    if state_value == ComputerSessionState::Failed {
        if let Some(updated) = updated {
            let _ = state
                .store
                .release_computer_control_lease_for_session(&session.id, &updated.updated_at)
                .await;
            publish_session_event(state, &updated).await;
        }
        return Ok(HelperValueAction::StopCapture {
            reason: "capture_terminal".into(),
        });
    }
    let Some(updated) = updated else {
        return Ok(HelperValueAction::None);
    };
    publish_session_event(state, &updated).await;
    if event_is_source_selected(value, &status) {
        Ok(HelperValueAction::StartCapture)
    } else {
        Ok(HelperValueAction::None)
    }
}

fn event_is_source_selected(value: &serde_json::Value, status: &HelperStatus) -> bool {
    value.get("event").and_then(serde_json::Value::as_str) == Some("capture.sourceSelected")
        && status.state == "ready"
}

fn frame_metadata_matches_session(
    frame: Option<&HelperFrameMetadata>,
    session: &StoredComputerSession,
) -> bool {
    frame.is_some_and(|frame| {
        frame.session_id.as_deref() == Some(session.id.as_str())
            && frame.generation == Some(session.generation)
    })
}

fn desired_media_state(signaling: &SignalingState) -> (ComputerSessionState, Option<String>) {
    match signaling.phase() {
        ViewPhase::Failed | ViewPhase::Closed => (
            ComputerSessionState::Failed,
            signaling
                .failure_reason
                .clone()
                .or_else(|| Some("computer_media_unavailable".into())),
        ),
        ViewPhase::AwaitingSource => (ComputerSessionState::AwaitingSource, None),
        ViewPhase::Live => (ComputerSessionState::Live, None),
        // A dropped connection that may come back is not live (input is
        // refused) but not failed either.
        ViewPhase::Reconnecting
        | ViewPhase::Connecting
        | ViewPhase::OfferReady
        | ViewPhase::Preparing => (ComputerSessionState::Preparing, None),
    }
}

/// A shared window that is closed, minimized or hidden suspends capture but
/// keeps its stream. The view stays live so the person can choose another
/// window from the menu instead of the switch ending the view.
fn shows_live_view(signaling: &SignalingState) -> bool {
    matches!(signaling.capture_state.as_str(), "capturing" | "suspended")
        && signaling.first_frame_observed
        && matches!(signaling.peer_state.as_str(), "connected" | "completed")
}

async fn reconcile_media_state(
    state: &AppState,
    session: &StoredComputerSession,
    signaling: &Arc<tokio::sync::Mutex<SignalingState>>,
) -> Result<(), sqlx::Error> {
    let (desired, failure_reason) = {
        let mut current = signaling.lock().await;
        if current.source_rebinding {
            return Ok(());
        }
        current.desired_state()
    };
    let Some(current) = state.store.computer_session(&session.id).await? else {
        return Ok(());
    };
    // A failed view keeps its first failure reason.
    if current.state == ComputerSessionState::Ended.as_str()
        || current.state == ComputerSessionState::Failed.as_str()
    {
        return Ok(());
    }
    if current.state == desired.as_str() && current.failure_reason == failure_reason {
        return Ok(());
    }
    if let Ok(Some(updated)) = state
        .store
        .transition_computer_session(
            &session.id,
            session.generation,
            desired,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            current.geometry_revision,
            failure_reason.as_deref(),
            &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        )
        .await
    {
        if desired == ComputerSessionState::Failed {
            let _ = state
                .store
                .release_computer_control_lease_for_session(&session.id, &updated.updated_at)
                .await;
        }
        publish_session_event(state, &updated).await;
    }
    Ok(())
}

async fn fail_session(state: &AppState, session: &StoredComputerSession, reason: &str) {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    // The helper may have advanced the source binding since launch; a
    // launch-time revision would no longer match and leave the view open.
    let geometry_revision = match state.store.computer_session(&session.id).await {
        Ok(Some(current)) => current.geometry_revision.max(session.geometry_revision),
        _ => session.geometry_revision,
    };
    if let Ok(Some(updated)) = state
        .store
        .transition_computer_session(
            &session.id,
            session.generation,
            ComputerSessionState::Failed,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            geometry_revision,
            Some(reason),
            &now,
        )
        .await
    {
        let _ = state
            .store
            .release_computer_control_lease_for_session(&session.id, &now)
            .await;
        publish_session_event(state, &updated).await;
    }
}

async fn end_session_for_owner(state: &AppState, session: &StoredComputerSession, reason: &str) {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    if reason == "viewer_timeout" {
        if let Ok(Some(current)) = state.store.computer_session(&session.id).await {
            if let Ok(state_value) = ComputerSessionState::try_from(current.state.as_str()) {
                let _ = state
                    .store
                    .transition_computer_session(
                        &current.id,
                        current.generation,
                        state_value,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        current.geometry_revision,
                        Some(reason),
                        &now,
                    )
                    .await;
            }
        }
    }
    if let Ok(Some(ended)) = state
        .store
        .end_computer_session(
            &session.id,
            &session.owner_device_id,
            Some(session.generation),
            &now,
        )
        .await
    {
        publish_session_event(state, &ended).await;
    }
    let _ = state
        .store
        .release_computer_control_lease_for_session(&session.id, &now)
        .await;
}

async fn publish_session_event(state: &AppState, session: &StoredComputerSession) {
    let _ = publish_event_with_context(
        state,
        WonderEvent::ComputerSessionChanged {
            session_id: session.id.clone(),
            generation: session.generation,
            state: session.state.clone(),
        },
        EventContext {
            conversation_id: Some(session.conversation_id.clone()),
            ..EventContext::default()
        },
    )
    .await;
}

/// Stop and reap the helper within about a second, whatever state it is in.
async fn stop_child(
    child: &mut tokio::process::Child,
    stdin: &mut HelperInput,
    handshake: &str,
) -> Option<std::process::ExitStatus> {
    // Release held native input before asking the helper to exit. The helper
    // repeats this in its stop handler for crash/timeout races.
    let _ = timeout(
        HELPER_STOP_WRITE_TIMEOUT,
        write_helper_request(
            stdin,
            98,
            "control.releaseAll",
            serde_json::json!({ "handshake": handshake }),
        ),
    )
    .await;
    let _ = timeout(
        HELPER_STOP_WRITE_TIMEOUT,
        write_helper_request(
            stdin,
            99,
            "stop",
            serde_json::json!({ "handshake": handshake }),
        ),
    )
    .await;
    // Close the pipe so a helper reading its input sees the end.
    let _ = timeout(HELPER_STOP_WRITE_TIMEOUT, stdin.shutdown()).await;
    match timeout(Duration::from_millis(750), child.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            let _ = child.kill().await;
            child.wait().await.ok()
        }
    }
}

#[cfg(all(test, unix))]
mod supervisor_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn session() -> ComputerSessionCreate {
        ComputerSessionCreate {
            id: "fake-session".into(),
            client_request_id: "fake-request".into(),
            owner_device_id: "fake-phone".into(),
            host_installation_id: "test".into(),
            conversation_id: "fake-chat".into(),
            generation: 1,
            state: ComputerSessionState::Preparing,
            video_quality: "standard".into(),
            video_max_fps: 15,
            source_id: None,
            source_name: None,
            source_kind: None,
            source_width: None,
            source_height: None,
            source_scale: None,
            crop_json: None,
            geometry_revision: 0,
            failure_reason: None,
            now: "now".into(),
        }
    }

    fn stored_session() -> StoredComputerSession {
        StoredComputerSession {
            id: "session".into(),
            client_request_id: "request".into(),
            owner_device_id: "owner".into(),
            host_installation_id: "host".into(),
            conversation_id: "conversation".into(),
            generation: 3,
            state: ComputerSessionState::Preparing.as_str().into(),
            video_quality: "standard".into(),
            video_max_fps: 15,
            source_id: None,
            source_name: None,
            source_kind: None,
            source_width: None,
            source_height: None,
            source_scale: None,
            crop_json: None,
            geometry_revision: 0,
            failure_reason: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            last_state_at: "now".into(),
            ended_at: None,
        }
    }

    async fn poll_clipboard(
        state: &AppState,
        stored: &StoredComputerSession,
        device_id: &str,
        subscription: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let request = serde_json::from_value(serde_json::json!({
            "generation": stored.generation, "conversationId": stored.conversation_id,
            "hostInstallationId": stored.host_installation_id,
            "clipboardSubscriptionId": subscription, "clipboardCursor": 0,
        }))
        .unwrap();
        let reply = signal_poll(
            State(state.clone()),
            Some(Extension(AuthenticatedDevice {
                device_id: device_id.into(),
                session_binding: "fixture".into(),
            })),
            Path(stored.id.clone()),
            Json(request),
        )
        .await;
        let status = reply.status();
        let bytes = axum::body::to_bytes(reply.into_body(), 65_536)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        if status == StatusCode::OK {
            crate::test_support::assert_http_contract("computerSignalingResponse", &value);
        }
        (status, value)
    }

    #[tokio::test]
    async fn clipboard_poll_is_ephemeral_owner_bound_and_forwards_stop() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let mut create = session();
        create.host_installation_id = state.host_installation_id.clone();
        create.state = ComputerSessionState::Live;
        let stored = state.store.insert_computer_session(&create).await.unwrap();
        let (commands, mut requests) = tokio::sync::mpsc::channel(8);
        let (activity, _) = tokio::sync::mpsc::channel(1);
        let (stop, _) = tokio::sync::oneshot::channel();
        let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
        signaling.lock().await.clipboard_capable = Some(true);
        *state.computer_supervisor.active.lock().await = Some(ManagedHelper {
            session_id: create.id.clone(),
            owner_device_id: create.owner_device_id.clone(),
            stop: Some(stop),
            join: tokio::spawn(async {}),
            signaling: signaling.clone(),
            commands,
            viewer_activity: activity,
        });
        let owner = create.owner_device_id.as_str();
        let (denied, _) = poll_clipboard(&state, &stored, "other", Some("device")).await;
        assert_eq!(denied, StatusCode::NOT_FOUND);
        // Without any subscription, polls never wait on the helper.
        let (status, idle) = poll_clipboard(&state, &stored, owner, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(idle.get("clipboard").is_none());
        assert!(requests.try_recv().is_err());

        // Subscribe, stop, an idle poll (skipped), then a helper failure.
        let worker = tokio::spawn(async move {
            for (subscription, fails) in [
                (Some("device"), false),
                (None, false),
                (Some("device"), true),
            ] {
                let Some(HelperCommand::Control {
                    method,
                    params,
                    response,
                }) = requests.recv().await
                else {
                    panic!("missing clipboard poll")
                };
                assert_eq!(method, "clipboard.poll");
                assert_eq!(params["subscriptionID"].as_str(), subscription);
                if fails {
                    response
                        .send(Err("computer helper did not acknowledge the request".into()))
                        .unwrap();
                    continue;
                }
                let mut reply = serde_json::json!({"subscriptionID":subscription, "sequence":1,
                    "status":if subscription.is_some() { "watching" } else { "off" }});
                if subscription.is_some() {
                    reply["text"] = serde_json::json!("private relay fixture");
                }
                response.send(Ok(reply)).unwrap();
            }
            requests
        });
        let (_, watching) = poll_clipboard(&state, &stored, owner, Some("device")).await;
        assert_eq!(watching["clipboard"]["text"], "private relay fixture");
        let (_, stopped) = poll_clipboard(&state, &stored, owner, None).await;
        assert_eq!(stopped["clipboard"]["status"], "off");
        assert!(stopped["clipboard"].get("text").is_none());
        let (_, idle) = poll_clipboard(&state, &stored, owner, None).await;
        assert!(idle.get("clipboard").is_none());
        // A timeout is transient: the phone must not be told to update its Mac.
        let (_, failed) = poll_clipboard(&state, &stored, owner, Some("device")).await;
        assert_eq!(failed["clipboard"]["status"], "unavailable");
        assert_eq!(failed["clipboard"]["subscriptionId"], "device");
        assert!(failed["clipboard"].get("text").is_none());
        let mut requests = worker.await.unwrap();

        // Negotiation still pending is transient; a helper without the
        // capability omits the field. Neither calls the helper.
        for (capable, expected) in [(None, Some("unavailable")), (Some(false), None)] {
            signaling.lock().await.clipboard_capable = capable;
            let (_, value) = poll_clipboard(&state, &stored, owner, Some("device")).await;
            assert_eq!(value["clipboard"]["status"].as_str(), expected);
            assert!(requests.try_recv().is_err());
        }

        // Clipboard contents never enter durable session storage.
        let after = state
            .store
            .computer_session(&stored.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.updated_at, stored.updated_at);
        assert!(after.failure_reason.is_none());
        assert!(state
            .store
            .events_for_conversation(&stored.conversation_id)
            .await
            .unwrap()
            .is_empty());
    }

    /// Copy from Mac without text (permission, private marker, nothing to copy)
    /// is delivered input: the lease advances and control stays active.
    #[tokio::test]
    async fn copy_from_mac_without_text_keeps_control_and_explains_why() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        state.computer_supervisor.set_control_capable(true);
        let mut create = session();
        create.host_installation_id = state.host_installation_id.clone();
        create.state = ComputerSessionState::Live;
        create.source_id = Some("display:1".into());
        create.source_kind = Some("display".into());
        state.store.insert_computer_session(&create).await.unwrap();
        let (commands, mut requests) = tokio::sync::mpsc::channel(8);
        let (activity, _) = tokio::sync::mpsc::channel(1);
        let (stop, _) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let mut copies = [
                serde_json::json!({"accepted":true, "clipboardReason":"clipboard_permission_denied"}),
                serde_json::json!({"accepted":true, "clipboardReason":"clipboard_permission_prompt"}),
                serde_json::json!({"accepted":true, "clipboardText":"fixture"}),
            ]
            .into_iter();
            while let Some(HelperCommand::Control {
                method, response, ..
            }) = requests.recv().await
            {
                let result = match method.as_str() {
                    "control.consent" => serde_json::json!({"granted": true}),
                    "control.activate" => serde_json::json!({"activated": true}),
                    "control.input" => copies.next().unwrap(),
                    _ => panic!("unexpected helper request {method}"),
                };
                response.send(Ok(result)).unwrap();
            }
        });
        *state.computer_supervisor.active.lock().await = Some(ManagedHelper {
            session_id: create.id.clone(),
            owner_device_id: create.owner_device_id.clone(),
            stop: Some(stop),
            join: tokio::spawn(async {}),
            signaling: Arc::new(tokio::sync::Mutex::new(SignalingState::default())),
            commands,
            viewer_activity: activity,
        });
        let auth = Some(Extension(AuthenticatedDevice {
            device_id: create.owner_device_id.clone(),
            session_binding: "fixture".into(),
        }));
        let acquire = acquire_control(
            State(state.clone()),
            auth.clone(),
            Path(create.id.clone()),
            Json(
                serde_json::from_value(serde_json::json!({"clientRequestId":"copy-control",
                    "generation":1, "geometryRevision":0, "sourceId":"display:1"}))
                .unwrap(),
            ),
        )
        .await;
        let bytes = axum::body::to_bytes(acquire.into_body(), 65_536)
            .await
            .unwrap();
        let acquired: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(acquired["granted"], true);
        let mut reasons = Vec::new();
        for sequence in 1..=3 {
            let batch = serde_json::json!({"leaseId":acquired["lease"]["id"], "generation":1,
                "geometryRevision":0, "sourceId":"display:1", "sequence":sequence,
                "actions":[{"type":"clipboard", "operation":"copyToPhone"}]});
            let input = input_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(serde_json::from_value(batch).unwrap()),
            )
            .await;
            assert_eq!(input.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(input.into_body(), 65_536)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            crate::test_support::assert_http_contract("computerControlActionResponse", &value);
            assert_eq!(value["acknowledged"], true);
            assert_eq!(value["status"], "active");
            assert_eq!(value["lease"]["status"], "active");
            assert_eq!(value["lease"]["lastSequence"], sequence);
            reasons.push(value["reason"].as_str().unwrap().to_owned());
            assert_eq!(
                value["clipboardText"].as_str(),
                (sequence == 3).then_some("fixture")
            );
        }
        assert!(reasons[0].contains("Privacy & Security"));
        assert!(reasons[1].contains("Allow Paste"));
        // The button carries the computer's name, so the step names only its verb.
        assert!(reasons[1].ends_with("tap Copy again.") && !reasons[1].contains("Copy from Mac"));
        assert_eq!(reasons[2], "Input accepted.");
        worker.abort();
    }

    /// Missing providers and source-specific gates reject every control entry
    /// point with a recovery reason, before reserving or delivering input.
    #[tokio::test]
    async fn unavailable_hosts_reject_control_acquire_heartbeat_and_input_with_reason() {
        for (kind, window_blocked) in [
            ("window", false),
            ("display", false),
            ("simulator", false),
            ("window", true),
        ] {
            let (_dir, state) = crate::ingestion::tests::fixture().await;
            if window_blocked {
                state.computer_supervisor.set_control_capable(true);
                state
                    .computer_supervisor
                    .window_control_blocked
                    .store(true, Ordering::Release);
            }
            let mut create = session();
            create.host_installation_id = state.host_installation_id.clone();
            create.state = ComputerSessionState::Live;
            create.source_id = Some("window:42".into());
            create.source_kind = Some(kind.into());
            let stored = state.store.insert_computer_session(&create).await.unwrap();
            let snapshot = serde_json::to_value(response(&state, stored)).unwrap();
            assert_eq!(snapshot["control"]["available"], false);
            crate::test_support::assert_http_contract("computerSession", &snapshot);
            let auth = Some(Extension(AuthenticatedDevice {
                device_id: create.owner_device_id.clone(),
                session_binding: "fixture".into(),
            }));
            let body = serde_json::json!({
                "clientRequestId":"window-control", "leaseId":"old-window-lease",
                "generation":1, "geometryRevision":0, "sourceId":"window:42", "sequence":1,
                "actions":[{"type":"pointer", "x":0.5, "y":0.5, "phase":"move"}]
            });
            let acquire = acquire_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(serde_json::from_value(body.clone()).unwrap()),
            )
            .await;
            let heartbeat = heartbeat_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(serde_json::from_value(body.clone()).unwrap()),
            )
            .await;
            let input = input_control(
                State(state.clone()),
                auth,
                Path(create.id.clone()),
                Json(serde_json::from_value(body).unwrap()),
            )
            .await;
            for reply in [acquire, heartbeat, input] {
                assert_eq!(reply.status(), StatusCode::SERVICE_UNAVAILABLE);
                let bytes = axum::body::to_bytes(reply.into_body(), 65_536)
                    .await
                    .unwrap();
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(value["granted"], false);
                assert_eq!(value["control"]["available"], false);
                assert_eq!(
                    value["reason"],
                    if kind == "simulator" {
                        SIMULATOR_CONTROL_PENDING_REASON
                    } else if window_blocked {
                        WINDOW_VIEW_ONLY_REASON
                    } else {
                        CONTROL_UNAVAILABLE_REASON
                    }
                );
                crate::test_support::assert_http_contract("computerControlActionResponse", &value);
            }
            assert!(state
                .store
                .computer_control_lease_by_request(&create.owner_device_id, "window-control")
                .await
                .unwrap()
                .is_none());
            state
                .computer_supervisor
                .window_control_blocked
                .store(false, Ordering::Release);
            state.computer_supervisor.set_control_capable(true);
            let mut supported = stored_session();
            for kind in ["display", "window"] {
                supported.source_kind = Some(kind.into());
                assert!(session_control_capability(&state, &supported).available);
            }
        }
    }

    /// A simulator view is controllable only when the helper said so for that
    /// exact device. The Mac's own Accessibility and window gates do not apply;
    /// the owner's control switch does. Reasons are user language.
    #[test]
    fn simulator_control_follows_the_helper_answer_for_its_exact_device() {
        let source = "simulator:201B72F4-1302-4474-A801-737B18507A1A";
        let answer = |control: serde_json::Value| serde_json::json!({"id": 1, "result": {"protocolVersion": 1, "simulatorControl": control}});
        let ready = answer(serde_json::json!({"sourceID": source, "available": true,
            "provider": "simulator-hid-v1", "requiresAccessibility": false, "reason": null}));
        assert_eq!(
            helper_simulator_control(&ready, source),
            SimulatorControl {
                source_id: source.into(),
                available: true,
                reason: None
            }
        );
        let other_device =
            helper_simulator_control(&ready, "simulator:00000000-0000-0000-0000-000000000001");
        assert!(!other_device.available);
        let older_helper = serde_json::json!({"id": 1, "result": {"protocolVersion": 1}});
        assert!(!helper_simulator_control(&older_helper, source).available);
        let stopped = answer(serde_json::json!({"sourceID": source, "available": false,
            "provider": "simulator-hid-v1", "requiresAccessibility": false, "reason": "simulator_not_booted"}));

        let phone = |value: &serde_json::Value, block: u8| {
            simulator_control_capability(Some(helper_simulator_control(value, source)), block)
        };
        let on = phone(&ready, 0);
        assert!(on.available);
        assert_eq!(on.reason, SIMULATOR_CONTROL_AVAILABLE_REASON);
        // Accessibility is a Mac-input permission, not a simulator one.
        assert!(phone(&ready, 1).available);
        let off = phone(&ready, 5);
        assert!(!off.available);
        assert_eq!(off.reason, CONTROL_DISABLED_REASON);
        assert_eq!(phone(&older_helper, 0).reason, SIMULATOR_VIEW_ONLY_REASON);
        assert_eq!(phone(&stopped, 0).reason, SIMULATOR_STOPPED_REASON);
        assert_eq!(
            simulator_control_capability(None, 0).reason,
            SIMULATOR_CONTROL_PENDING_REASON
        );
        for capability in [
            on,
            off,
            phone(&older_helper, 0),
            simulator_control_capability(None, 0),
        ] {
            assert_eq!(capability.action, "none");
            assert!(
                !capability.reason.contains("simulator:") && !capability.reason.contains("dtuhid")
            );
            crate::test_support::assert_http_contract(
                "computerControlCapability",
                &serde_json::to_value(&capability).unwrap(),
            );
        }

        let supervisor = ComputerSessionSupervisor::default();
        supervisor.set_simulator_control(Some(helper_simulator_control(&ready, source)));
        assert!(supervisor.simulator_control_for(Some(source)).is_some());
        assert!(supervisor
            .simulator_control_for(Some("simulator:other"))
            .is_none());
        assert!(supervisor.simulator_control_for(None).is_none());
        // A helper restart forgets the answer until the new helper reports.
        supervisor.set_control_capable(false);
        assert!(supervisor.simulator_control_for(Some(source)).is_none());
    }

    /// Simulator input reuses the lease: touch, buttons, keys and US-keyboard
    /// text reach the helper unchanged; Mac-only actions, other text and Mac
    /// app launching are refused before a sequence or the helper is used.
    #[tokio::test]
    async fn simulator_input_uses_the_lease_and_refuses_what_a_simulator_cannot_take() {
        let source = "simulator:201B72F4-1302-4474-A801-737B18507A1A";
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        // No Mac input provider (Accessibility missing): simulators still work.
        state.computer_supervisor.set_control_capable(false);
        state
            .computer_supervisor
            .set_simulator_control(Some(SimulatorControl {
                source_id: source.into(),
                available: true,
                reason: None,
            }));
        let mut create = session();
        create.host_installation_id = state.host_installation_id.clone();
        create.state = ComputerSessionState::Live;
        create.source_id = Some(source.into());
        create.source_kind = Some("simulator".into());
        state.store.insert_computer_session(&create).await.unwrap();
        let (commands, mut requests) = tokio::sync::mpsc::channel(8);
        let (activity, _) = tokio::sync::mpsc::channel(1);
        let (stop, _) = tokio::sync::oneshot::channel();
        let (delivered_tx, mut delivered) = tokio::sync::mpsc::unbounded_channel();
        let worker = tokio::spawn(async move {
            while let Some(HelperCommand::Control {
                method,
                params,
                response,
            }) = requests.recv().await
            {
                let result = match method.as_str() {
                    "control.consent" => serde_json::json!({"granted": true}),
                    "control.activate" => serde_json::json!({"activated": true}),
                    "control.input" => {
                        delivered_tx.send(params["actions"].clone()).unwrap();
                        serde_json::json!({"accepted": true})
                    }
                    _ => panic!("unexpected helper request {method}"),
                };
                response.send(Ok(result)).unwrap();
            }
        });
        *state.computer_supervisor.active.lock().await = Some(ManagedHelper {
            session_id: create.id.clone(),
            owner_device_id: create.owner_device_id.clone(),
            stop: Some(stop),
            join: tokio::spawn(async {}),
            signaling: Arc::new(tokio::sync::Mutex::new(SignalingState::default())),
            commands,
            viewer_activity: activity,
        });
        let auth = Some(Extension(AuthenticatedDevice {
            device_id: create.owner_device_id.clone(),
            session_binding: "fixture".into(),
        }));
        let body = |reply: Response| async move {
            let status = reply.status();
            let bytes = axum::body::to_bytes(reply.into_body(), 65_536)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default(),
            )
        };
        let (status, acquired) = body(
            acquire_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(
                    serde_json::from_value(serde_json::json!({"clientRequestId":"sim-control",
                        "generation":1, "geometryRevision":0, "sourceId": source}))
                    .unwrap(),
                ),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(acquired["granted"], true);
        assert_eq!(acquired["control"]["available"], true);
        assert_eq!(
            acquired["control"]["reason"],
            SIMULATOR_CONTROL_AVAILABLE_REASON
        );
        let batch = |sequence: u64, actions: serde_json::Value| {
            serde_json::json!({"leaseId": acquired["lease"]["id"], "generation":1,
                "geometryRevision":0, "sourceId": source, "sequence": sequence, "actions": actions})
        };
        let gesture = serde_json::json!([
            {"type":"touch", "x":0.5, "y":0.9, "phase":"down", "delayMs":null},
            {"type":"touch", "x":0.5, "y":0.2, "phase":"move", "delayMs":null},
            {"type":"touch", "x":0.5, "y":0.2, "phase":"up", "delayMs":800},
            {"type":"button", "button":"home", "phase":"press"},
            {"type":"key", "key":"return", "phase":"press", "modifiers":0},
            {"type":"text", "text":"Hello, World!"}
        ]);
        crate::test_support::assert_http_contract(
            "computerInputBatchRequest",
            &batch(1, gesture.clone()),
        );
        let (status, value) = body(
            input_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(serde_json::from_value(batch(1, gesture.clone())).unwrap()),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        crate::test_support::assert_http_contract("computerControlActionResponse", &value);
        assert_eq!(value["lease"]["lastSequence"], 1);
        assert_eq!(value["control"]["available"], true);
        assert_eq!(delivered.recv().await.unwrap(), gesture);

        let refused = [
            (
                serde_json::json!([{"type":"pointer", "x":0.5, "y":0.5, "phase":"down", "button":"left"}]),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!([{"type":"clipboard", "operation":"copyToPhone"}]),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!([{"type":"button", "button":"power", "phase":"press"}]),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!([{"type":"touch", "x":1.5, "y":0.5, "phase":"down"}]),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!([{"type":"text", "text":"café"}]),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ];
        for (actions, expected) in refused {
            let reply = input_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(serde_json::from_value(batch(2, actions)).unwrap()),
            )
            .await;
            let (status, value) = body(reply).await;
            assert_eq!(status, expected);
            if expected == StatusCode::UNPROCESSABLE_ENTITY {
                crate::test_support::assert_http_contract("computerControlActionResponse", &value);
                assert_eq!(value["status"], "unsupported");
                assert_eq!(value["reason"], SIMULATOR_TEXT_REASON);
                // Control continues: the phone can paste instead.
                assert_eq!(value["control"]["available"], true);
            }
        }
        assert!(
            delivered.try_recv().is_err(),
            "refused input never reaches the helper"
        );
        let lease = state
            .store
            .computer_control_lease(acquired["lease"]["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((lease.status.as_str(), lease.last_sequence), ("active", 1));

        let (status, opened) = body(
            open_app(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(
                    serde_json::from_value(serde_json::json!({"leaseId": acquired["lease"]["id"],
                        "generation":1, "geometryRevision":0, "sourceId": source,
                        "bundleId":"com.apple.Safari"}))
                    .unwrap(),
                ),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(opened["opened"], false);
        worker.abort();

        // Computers never take simulator touch or buttons.
        for (actions, kind) in [
            (
                serde_json::json!([{"type":"touch", "x":0.5, "y":0.5, "phase":"down"}]),
                Some("display"),
            ),
            (
                serde_json::json!([{"type":"button", "button":"home", "phase":"press"}]),
                Some("window"),
            ),
        ] {
            let actions: Vec<InputAction> = serde_json::from_value(actions).unwrap();
            assert!(actions.iter().all(validate_input_action));
            assert!(!actions_fit_source(kind, &actions));
            assert!(actions_fit_source(Some("simulator"), &actions));
        }
    }

    // Contract owner: acquisition/release, not the lease poller. A reservation,
    // failed activation, lost heartbeat, or another chat's control never means
    // the owner accepted and handed back this request.
    #[tokio::test]
    async fn takeover_failed_acquisition_is_never_handback() {
        takeover_completion_cases(&["consent_denied", "activation_failed"]).await;
    }

    #[tokio::test]
    async fn takeover_lease_loss_is_never_handback() {
        takeover_completion_cases(&["lease_lost", "lease_expired"]).await;
    }

    #[tokio::test]
    async fn takeover_only_completes_the_explicitly_accepted_request() {
        takeover_completion_cases(&["returned", "unrelated"]).await;
    }

    async fn takeover_completion_cases(outcomes: &[&str]) {
        for &outcome in outcomes {
            let (_dir, state) = crate::ingestion::tests::fixture().await;
            state.computer_supervisor.set_control_capable(true);
            let mut create = session();
            create.host_installation_id = state.host_installation_id.clone();
            create.state = ComputerSessionState::Live;
            state.store.insert_computer_session(&create).await.unwrap();
            for (id, conversation) in [
                ("accepted", "fake-chat"),
                ("same-chat", "fake-chat"),
                ("other-chat", "other-chat"),
            ] {
                request_takeover(
                    &state,
                    TakeoverOpen {
                        id,
                        conversation_id: conversation,
                        agent: "Claude",
                        reason: "Sign in",
                        app: Some("Google Chrome"),
                        timeout: Duration::from_secs(60),
                    },
                )
                .await;
            }
            let (commands, mut requests) = tokio::sync::mpsc::channel(8);
            let (activity, _) = tokio::sync::mpsc::channel(1);
            let (stop, _) = tokio::sync::oneshot::channel();
            *state.computer_supervisor.active.lock().await = Some(ManagedHelper {
                session_id: create.id.clone(),
                owner_device_id: create.owner_device_id.clone(),
                stop: Some(stop),
                join: tokio::spawn(async {}),
                signaling: Arc::new(tokio::sync::Mutex::new(SignalingState::default())),
                commands,
                viewer_activity: activity,
            });
            let auth = Some(Extension(AuthenticatedDevice {
                device_id: create.owner_device_id.clone(),
                session_binding: "fixture".into(),
            }));
            // The accepted card must belong to this chat; invalid identities
            // fail before consent, reservations or helper calls.
            for request_id in ["other-chat", "missing"] {
                let reply = acquire_control(State(state.clone()), auth.clone(), Path(create.id.clone()),
                    Json(serde_json::from_value(serde_json::json!({
                        "clientRequestId":"invalid-takeover", "generation":1, "geometryRevision":0,
                        "takeoverRequestId":request_id,
                    })).unwrap())).await;
                assert_eq!(reply.status(), StatusCode::CONFLICT);
                assert!(requests.try_recv().is_err());
            }
            let acquire = acquire_control(State(state.clone()), auth.clone(), Path(create.id.clone()),
                Json(serde_json::from_value(serde_json::json!({
                    "clientRequestId":"takeover-control", "generation":1, "geometryRevision":0, "sourceId":null,
                    "takeoverRequestId": if outcome == "unrelated" { None } else { Some("accepted") },
                })).unwrap()));
            let contract_request = serde_json::json!({
                "clientRequestId":"takeover-control", "generation":1, "geometryRevision":0, "sourceId":null,
                "takeoverRequestId": if outcome == "unrelated" { None } else { Some("accepted") },
            });
            crate::test_support::assert_http_contract(
                "acquireComputerControlRequest",
                &contract_request,
            );
            let mut observed_reservation = None;
            let acquired = timeout(Duration::from_secs(30), async {
                tokio::pin!(acquire);
                loop {
                    tokio::select! {
                        reply = &mut acquire => break reply,
                        Some(HelperCommand::Control { method, response, .. }) = requests.recv() => {
                            let reply = match method.as_str() {
                                "control.consent" => {
                                    observed_reservation = takeover_state(&state, "accepted").await;
                                    serde_json::json!({"granted": outcome != "consent_denied"})
                                },
                                "control.activate" => serde_json::json!({"activated": outcome != "activation_failed"}),
                                "control.release" => serde_json::json!({"released":true}),
                                _ => panic!("unexpected helper request {method}"),
                            };
                            response.send(Ok(reply)).unwrap();
                        }
                    }
                }
            }).await.unwrap();
            if matches!(outcome, "consent_denied" | "activation_failed") {
                assert_eq!(
                    observed_reservation,
                    Some(TakeoverState::Waiting),
                    "{outcome}: reservation isn't control"
                );
            }
            let failed = matches!(outcome, "consent_denied" | "activation_failed");
            assert_eq!(
                acquired.status(),
                if failed {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::OK
                }
            );
            let bytes = axum::body::to_bytes(acquired.into_body(), 65_536)
                .await
                .unwrap();
            let acquired: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let expected = if failed || outcome == "unrelated" {
                TakeoverState::Waiting
            } else {
                TakeoverState::InControl
            };
            assert_eq!(
                takeover_state(&state, "accepted").await,
                Some(expected),
                "{outcome}"
            );
            if outcome != "lease_lost" {
                for other in ["same-chat", "other-chat"] {
                    assert_eq!(
                        takeover_state(&state, other).await,
                        Some(TakeoverState::Waiting),
                        "{outcome}: {other}"
                    );
                }
            }
            if failed {
                continue;
            }
            if outcome != "unrelated" {
                // Retrying one acquisition with a different accepted card must
                // not bind that same native control to a second request.
                let retry = acquire_control(State(state.clone()), auth.clone(), Path(create.id.clone()),
                    Json(serde_json::from_value(serde_json::json!({
                        "clientRequestId":"takeover-control", "generation":1, "geometryRevision":0,
                        "takeoverRequestId":"same-chat",
                    })).unwrap())).await;
                assert_eq!(retry.status(), StatusCode::CONFLICT);
                assert!(requests.try_recv().is_err());
            }
            if outcome == "lease_lost" {
                // Same durable effect as heartbeat expiry/session loss, with no
                // call to the owner's Give control back route.
                state
                    .store
                    .release_computer_control_lease_for_session(
                        &create.id,
                        &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    takeover_state(&state, "accepted").await,
                    Some(TakeoverState::Expired),
                    "lost control must finish the wait without claiming hand-back"
                );
                assert_eq!(
                    decline_takeover(State(state.clone()), auth.clone(), Path("accepted".into()))
                        .await
                        .status(),
                    StatusCode::NO_CONTENT
                );
                assert_eq!(
                    takeover_state(&state, "accepted").await,
                    Some(TakeoverState::Expired)
                );
                continue;
            }
            if outcome == "lease_expired" {
                let now = Utc::now();
                let at = now.to_rfc3339_opts(SecondsFormat::Millis, true);
                let expired = (now - chrono::Duration::seconds(1))
                    .to_rfc3339_opts(SecondsFormat::Millis, true);
                state
                    .store
                    .heartbeat_computer_control_lease(
                        acquired["lease"]["id"].as_str().unwrap(),
                        &create.owner_device_id,
                        &create.host_installation_id,
                        &create.id,
                        &create.conversation_id,
                        1,
                        None,
                        0,
                        &at,
                        &expired,
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert!(!owner_has_control(&state).await.unwrap());
                assert_eq!(
                    takeover_state(&state, "accepted").await,
                    Some(TakeoverState::Expired),
                    "expired control must finish the wait"
                );
            }
            let release = release_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(
                    serde_json::from_value(serde_json::json!({"leaseId": acquired["lease"]["id"],
                    "generation":1, "geometryRevision":0}))
                    .unwrap(),
                ),
            );
            let released = timeout(Duration::from_secs(30), async {
                tokio::pin!(release);
                loop {
                    tokio::select! {
                        reply = &mut release => break reply,
                        Some(HelperCommand::Control { method, response, .. }) = requests.recv() => {
                            assert_eq!(method, "control.release");
                            response.send(Ok(serde_json::json!({"released":true}))).unwrap();
                        }
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(released.status(), StatusCode::OK);
            assert_eq!(
                takeover_state(&state, "accepted").await,
                Some(match outcome {
                    "returned" => TakeoverState::Returned,
                    "lease_expired" => TakeoverState::Expired,
                    _ => TakeoverState::Waiting,
                })
            );
            for other in ["same-chat", "other-chat"] {
                assert_eq!(
                    takeover_state(&state, other).await,
                    Some(TakeoverState::Waiting)
                );
            }
            if outcome == "lease_expired" {
                assert_eq!(
                    state
                        .computer_supervisor
                        .advance_takeover("accepted", Utc::now() + chrono::Duration::seconds(61)),
                    Some(TakeoverState::Expired)
                );
            }
        }
    }

    // Closing or releasing must cancel a pending native decision, including a
    // late grant, without waiting for the 120-second consent deadline.
    #[tokio::test]
    async fn end_and_release_during_pending_consent_return_promptly() {
        for ending in [true, false] {
            let (_dir, state) = crate::ingestion::tests::fixture().await;
            state.computer_supervisor.set_control_capable(true);
            let mut create = session();
            create.host_installation_id = state.host_installation_id.clone();
            create.state = ComputerSessionState::Live;
            state.store.insert_computer_session(&create).await.unwrap();
            let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            let previous = ComputerControlLeaseCreate {
                id: "previous-lease".into(),
                client_request_id: "previous-request".into(),
                session_id: create.id.clone(),
                owner_device_id: create.owner_device_id.clone(),
                host_installation_id: create.host_installation_id.clone(),
                conversation_id: create.conversation_id.clone(),
                session_generation: 1,
                source_id: None,
                geometry_revision: 0,
                now: now.clone(),
                expires_at: (Utc::now() + chrono::Duration::seconds(120))
                    .to_rfc3339_opts(SecondsFormat::Millis, true),
            };
            assert!(matches!(
                state
                    .store
                    .acquire_computer_control_lease(&previous)
                    .await
                    .unwrap(),
                ComputerLeaseAcquireResult::Granted(_)
            ));
            state
                .store
                .release_computer_control_lease(&previous.id, &create.owner_device_id, &now)
                .await
                .unwrap();
            let (commands, mut requests) = tokio::sync::mpsc::channel(8);
            let (activity, _) = tokio::sync::mpsc::channel(1);
            let (stop, _) = tokio::sync::oneshot::channel();
            *state.computer_supervisor.active.lock().await = Some(ManagedHelper {
                session_id: create.id.clone(),
                owner_device_id: create.owner_device_id.clone(),
                stop: Some(stop),
                join: tokio::spawn(async {}),
                signaling: Arc::new(tokio::sync::Mutex::new(SignalingState::default())),
                commands,
                viewer_activity: activity,
            });
            let auth = Some(Extension(AuthenticatedDevice {
                device_id: create.owner_device_id.clone(),
                session_binding: "fixture".into(),
            }));
            let acquiring_state = state.clone();
            let acquiring_auth = auth.clone();
            let id = create.id.clone();
            let mut acquiring = tokio::spawn(async move {
                acquire_control(
                    State(acquiring_state),
                    acquiring_auth,
                    Path(id),
                    Json(
                        serde_json::from_value(
                            serde_json::json!({"clientRequestId":"pending-consent",
                        "generation":1,"geometryRevision":0}),
                        )
                        .unwrap(),
                    ),
                )
                .await
            });
            let Some(HelperCommand::Control {
                method,
                params,
                response: late_consent,
            }) = timeout(Duration::from_secs(60), requests.recv())
                .await
                .unwrap()
            else {
                panic!("missing consent")
            };
            assert_eq!(method, "control.consent");
            let lease_id = params["leaseID"].as_str().unwrap().to_owned();
            let unrelated_end = timeout(
                Duration::from_secs(60),
                end(
                    State(state.clone()),
                    auth.clone(),
                    Path("another-session".into()),
                    Query(GenerationQuery { generation: None }),
                ),
            )
            .await
            .expect("one session's consent blocked another session");
            assert_eq!(unrelated_end.status(), StatusCode::NO_CONTENT);
            let retry = timeout(
                Duration::from_secs(60),
                acquire_control(
                    State(state.clone()),
                    auth.clone(),
                    Path(create.id.clone()),
                    Json(
                        serde_json::from_value(
                            serde_json::json!({"clientRequestId":"pending-consent",
                    "generation":1,"geometryRevision":0}),
                        )
                        .unwrap(),
                    ),
                ),
            )
            .await
            .expect("retry waited on native consent");
            assert_eq!(retry.status(), StatusCode::CONFLICT);
            // A delayed release of an older lease must not cancel this new
            // request, even when both use the same session/view revision.
            let stale_release = release_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(
                    serde_json::from_value(serde_json::json!({"leaseId":previous.id,
                    "generation":1,"geometryRevision":0}))
                    .unwrap(),
                ),
            );
            let old_release_ack = async {
                let Some(HelperCommand::Control {
                    method,
                    params,
                    response,
                }) = requests.recv().await
                else {
                    panic!("missing release")
                };
                assert_eq!(method, "control.release");
                assert_eq!(params["leaseID"], previous.id);
                let _ = response.send(Ok(serde_json::json!({"released":false})));
            };
            let (stale_reply, _) = timeout(Duration::from_secs(60), async {
                tokio::join!(stale_release, old_release_ack)
            })
            .await
            .unwrap();
            assert_eq!(stale_reply.status(), StatusCode::OK);
            assert!(
                timeout(Duration::from_millis(30), &mut acquiring)
                    .await
                    .is_err(),
                "an old lease release cancelled a new decision"
            );
            assert!(!late_consent.is_closed());
            let mut cancelling = Box::pin(async {
                if ending {
                    end(
                        State(state.clone()),
                        auth.clone(),
                        Path(create.id.clone()),
                        Query(GenerationQuery {
                            generation: Some(1),
                        }),
                    )
                    .await
                } else {
                    release_control(
                        State(state.clone()),
                        auth.clone(),
                        Path(create.id.clone()),
                        Json(
                            serde_json::from_value(serde_json::json!({"leaseId":lease_id,
                            "generation":1,"geometryRevision":0}))
                            .unwrap(),
                        ),
                    )
                    .await
                }
            });
            // The helper can acknowledge cancellation without deciding consent;
            // it answers releases only until the end/release call returns (the
            // command channel stays open, so waiting for it to close would
            // only run out the timeout).
            let reply = timeout(Duration::from_secs(60), async {
                loop {
                    tokio::select! {
                        reply = &mut cancelling => break reply,
                        Some(HelperCommand::Control { method, response, .. }) = requests.recv() => {
                            assert_eq!(
                                method, "control.release",
                                "a cancelled decision must never activate"
                            );
                            let _ = response.send(Ok(serde_json::json!({"released":true})));
                        }
                    }
                }
            })
            .await;
            assert!(
                reply.is_ok(),
                "end/release waited behind pending consent (ending={ending})"
            );
            assert_eq!(
                reply.unwrap().status(),
                if ending {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::OK
                }
            );
            let acquired = timeout(Duration::from_secs(60), acquiring)
                .await
                .expect("cancelled acquisition still waited for consent")
                .unwrap();
            assert_ne!(acquired.status(), StatusCode::OK);
            let durable = state
                .store
                .computer_control_lease(&lease_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(durable.status, "released");
            // A stale native answer must have no waiting acquisition to activate.
            assert!(late_consent
                .send(Ok(serde_json::json!({"granted":true})))
                .is_err());
            assert!(requests.try_recv().is_err());
            assert_eq!(
                state
                    .store
                    .computer_session(&create.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                if ending { "ended" } else { "live" }
            );
            state.computer_supervisor.active.lock().await.take();
        }
    }

    #[tokio::test]
    async fn source_rebind_refuses_lease_mismatches_and_unavailable_windows_before_helper_delivery()
    {
        for requested in [None, Some("wrong-lease"), Some("held-lease")] {
            let (_dir, state) = crate::ingestion::tests::fixture().await;
            state.computer_supervisor.set_control_capable(true);
            state
                .computer_supervisor
                .window_control_blocked
                .store(true, Ordering::Release);
            let mut create = session();
            create.host_installation_id = state.host_installation_id.clone();
            create.state = ComputerSessionState::Live;
            let stored = state.store.insert_computer_session(&create).await.unwrap();
            let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            let expires = (Utc::now() + chrono::Duration::seconds(120))
                .to_rfc3339_opts(SecondsFormat::Millis, true);
            let lease = ComputerControlLeaseCreate {
                id: "held-lease".into(),
                client_request_id: "held-request".into(),
                session_id: create.id.clone(),
                owner_device_id: create.owner_device_id.clone(),
                host_installation_id: create.host_installation_id.clone(),
                conversation_id: create.conversation_id.clone(),
                session_generation: 1,
                source_id: None,
                geometry_revision: 0,
                now,
                expires_at: expires,
            };
            assert!(matches!(
                state
                    .store
                    .acquire_computer_control_lease(&lease)
                    .await
                    .unwrap(),
                ComputerLeaseAcquireResult::Granted(_)
            ));
            let (commands, mut requests) = tokio::sync::mpsc::channel(8);
            let (activity, _) = tokio::sync::mpsc::channel(1);
            let (stop, _) = tokio::sync::oneshot::channel();
            let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
            *state.computer_supervisor.active.lock().await = Some(ManagedHelper {
                session_id: create.id.clone(),
                owner_device_id: create.owner_device_id.clone(),
                stop: Some(stop),
                join: tokio::spawn(async {}),
                signaling: signaling.clone(),
                commands,
                viewer_activity: activity,
            });
            let reply = timeout(Duration::from_secs(60), set_source(State(state.clone()),
                Some(Extension(AuthenticatedDevice { device_id:create.owner_device_id.clone(), session_binding:"fixture".into() })),
                Path(create.id.clone()), Json(serde_json::from_value(serde_json::json!({"generation":1,
                    "geometryRevision":0,"leaseId":requested,"source":{"id":"app:editor#43","kind":"window"}})).unwrap())))
                .await.expect("unavailable window or lease mismatch reached the helper");
            if requested == Some("held-lease") {
                assert_eq!(reply.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(reply.into_body(), 65_536)
                    .await
                    .unwrap();
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(value["accepted"], false);
                assert_eq!(value["lease"]["id"], lease.id);
                assert!(value["reason"]
                    .as_str()
                    .unwrap()
                    .contains("Give control back"));
            } else {
                assert_eq!(reply.status(), StatusCode::CONFLICT);
            }
            assert!(matches!(
                requests.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            let unchanged = state
                .store
                .computer_session(&create.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(unchanged.state, "live");
            assert_eq!(unchanged.source_id, stored.source_id);
            assert_eq!(unchanged.geometry_revision, stored.geometry_revision);
            assert!(!signaling.lock().await.source_rebinding);
            assert_eq!(
                state
                    .store
                    .computer_control_lease(&lease.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                "active"
            );
            state.computer_supervisor.active.lock().await.take();
        }
    }

    /// An explicit refusal before the helper changes its binding must leave
    /// the current view and lease usable. An ambiguous delivery still fails closed.
    #[tokio::test]
    async fn source_rebind_refusal_keeps_the_current_view_and_lease() {
        // A closed or minimized shared window leaves capture suspended; a
        // refused switch from it must keep the view too (measured on macOS 26).
        for (moved, capture) in [
            (false, "capturing"),
            (true, "capturing"),
            (false, "suspended"),
        ] {
            let (_dir, state) = crate::ingestion::tests::fixture().await;
            state.computer_supervisor.set_control_capable(true);
            let mut create = session();
            create.host_installation_id = state.host_installation_id.clone();
            create.state = ComputerSessionState::Live;
            create.source_id = Some("window:42".into());
            create.source_kind = Some("window".into());
            let stored = state.store.insert_computer_session(&create).await.unwrap();
            let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            let lease = ComputerControlLeaseCreate {
                id: "held-lease".into(),
                client_request_id: "held-request".into(),
                session_id: create.id.clone(),
                owner_device_id: create.owner_device_id.clone(),
                host_installation_id: create.host_installation_id.clone(),
                conversation_id: create.conversation_id.clone(),
                session_generation: 1,
                source_id: stored.source_id.clone(),
                geometry_revision: 0,
                now,
                expires_at: (Utc::now() + chrono::Duration::seconds(120))
                    .to_rfc3339_opts(SecondsFormat::Millis, true),
            };
            assert!(matches!(
                state
                    .store
                    .acquire_computer_control_lease(&lease)
                    .await
                    .unwrap(),
                ComputerLeaseAcquireResult::Granted(_)
            ));
            let (commands, mut requests) = tokio::sync::mpsc::channel(8);
            let (activity, _) = tokio::sync::mpsc::channel(1);
            let (stop, _) = tokio::sync::oneshot::channel();
            let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
            *state.computer_supervisor.active.lock().await = Some(ManagedHelper {
                session_id: create.id.clone(),
                owner_device_id: create.owner_device_id.clone(),
                stop: Some(stop),
                join: tokio::spawn(async {}),
                signaling: signaling.clone(),
                commands,
                viewer_activity: activity,
            });
            let worker = tokio::spawn(async move {
                let Some(HelperCommand::Control {
                    method,
                    params,
                    response,
                }) = requests.recv().await
                else {
                    panic!("missing rebind")
                };
                assert_eq!(method, "capture.rebind");
                response
                    .send(Ok(
                        serde_json::json!({"accepted":false, "errorCode":"source_not_found",
                    "status":{"sessionID":params["sessionID"],"generation":params["generation"],
                        "state":capture, "sourceID":if moved {"window:43"} else {"window:42"},
                        "geometryRevision":if moved {1} else {0}}}),
                    ))
                    .unwrap();
                requests
            });
            let reply = set_source(
                State(state.clone()),
                Some(Extension(AuthenticatedDevice {
                    device_id: create.owner_device_id.clone(),
                    session_binding: "fixture".into(),
                })),
                Path(create.id.clone()),
                Json(
                    serde_json::from_value(serde_json::json!({"generation":1,
                    "geometryRevision":0,"sourceId":"window:42","leaseId":lease.id,
                    "source":{"id":"app:editor#43","kind":"window"}}))
                    .unwrap(),
                ),
            )
            .await;
            let mut requests = worker.await.unwrap();
            let durable = state
                .store
                .computer_session(&create.id)
                .await
                .unwrap()
                .unwrap();
            let durable_lease = state
                .store
                .computer_control_lease(&lease.id)
                .await
                .unwrap()
                .unwrap();
            if moved {
                assert_eq!(reply.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(durable.state, "failed");
                assert_ne!(durable_lease.status, "active");
            } else {
                assert_eq!(reply.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(reply.into_body(), 65_536)
                    .await
                    .unwrap();
                let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(reply["accepted"], false);
                assert!(reply["reason"].as_str().unwrap().contains("window"));
                assert_eq!(reply["session"]["id"], stored.id);
                assert_eq!(reply["lease"]["id"], lease.id);
                assert_eq!(durable.state, "live");
                assert_eq!(durable.source_id, stored.source_id);
                assert_eq!(durable.geometry_revision, stored.geometry_revision);
                assert_eq!(durable_lease.status, "active");
                assert!(!signaling.lock().await.source_rebinding);
                assert!(requests.try_recv().is_err());
            }
            state.computer_supervisor.active.lock().await.take();
        }
    }

    #[test]
    fn a_closed_or_hidden_shared_window_keeps_the_view_live() {
        let mut signaling = SignalingState {
            capture_state: "suspended".into(),
            peer_state: "connected".into(),
            first_frame_observed: true,
            ..SignalingState::default()
        };
        assert_eq!(
            desired_media_state(&signaling).0,
            ComputerSessionState::Live
        );
        assert_eq!(signaling.public_state(), "live");
        signaling.first_frame_observed = false;
        assert_eq!(
            desired_media_state(&signaling).0,
            ComputerSessionState::Preparing
        );
        signaling.first_frame_observed = true;
        signaling.capture_state = "sourceRemoved".into();
        assert_eq!(
            desired_media_state(&signaling).0,
            ComputerSessionState::Failed
        );
    }

    #[tokio::test]
    async fn failure_reasons_reach_the_phone_in_plain_words() {
        let sck = |code: i64| {
            format!("stream_error_com.apple.ScreenCaptureKit.SCStreamErrorDomain_{code}")
        };
        assert_eq!(
            failure_words(&sck(-3805), Some("window")),
            "Your computer interrupted screen sharing. Try again."
        );
        assert!(failure_words(&sck(-3801), None).contains("Screen & System Audio Recording"));
        assert!(failure_words(&sck(-3815), None).contains("no longer available"));
        assert_eq!(
            failure_words("source_removed", Some("simulator")),
            "This simulator has stopped."
        );
        assert!(failure_words("source_removed", Some("window")).contains("window closed"));
        let sentence = "The computer view changed. Reopen it.";
        assert_eq!(failure_words(sentence, None), sentence);
        // A Wayland login names the missing X11 session instead of a generic stop.
        assert!(failure_words("wayland_unsupported", None)
            .starts_with("Computer view needs an X11 session"));
        assert!(failure_words("x11_display_required", None).contains("X11 desktop"));
        // Helper stops, startup failures and connection setup each say what
        // happened instead of one stopped-sharing sentence.
        let generic = failure_words("some_future_code", None);
        let helper_stopped = failure_words("helper_exited", None);
        let not_started = failure_words("helper_startup_timeout", None);
        let setup = failure_words("vp8_unavailable", None);
        let dropped = failure_words("peer_connection_disconnected", None);
        for (group, codes) in [
            (
                &helper_stopped,
                &["helper_output_closed", "helper_protocol_error"][..],
            ),
            (&not_started, &["computer_provider_failed_to_start"][..]),
            (
                &setup,
                &[
                    "offer_failed",
                    "peer_setup_timeout",
                    "remote_description_failed",
                ][..],
            ),
        ] {
            for code in codes {
                assert_eq!(&failure_words(code, None), group, "{code}");
            }
        }
        let groups = [&generic, &helper_stopped, &not_started, &setup, &dropped];
        for (index, group) in groups.iter().enumerate() {
            assert!(!groups[index + 1..].contains(group), "{group}");
        }
        let codes = [
            sck(-3802),
            sck(-3805),
            sck(-3811),
            sck(-3817),
            sck(-3821),
            "stream_error_NSOSStatusErrorDomain_-50".into(),
            "helper_capture_failed".into(),
            "helper_exited".into(),
            "helper_output_closed".into(),
            "helper_protocol_error".into(),
            "helper_startup_timeout".into(),
            "computer_provider_failed_to_start".into(),
            "vp8_unavailable".into(),
            "peer_setup_timeout".into(),
            "computer_media_unavailable".into(),
            "peer_connection_disconnected".into(),
            "viewer_timeout".into(),
            "source_rebind_failed".into(),
            "screen_recording_permission_required".into(),
            "window_not_found".into(),
            "no_display".into(),
            "capture_unavailable".into(),
            "some_future_code".into(),
        ];
        for code in codes {
            let words = failure_words(&code, Some("window"));
            assert!(
                !words.contains('_') && !words.contains("-38"),
                "{code} -> {words}"
            );
            // The signaling contract bounds the reason at 128 characters.
            assert!(words.chars().count() <= 128, "{code} -> {words}");
        }

        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let mut stored = stored_session();
        stored.failure_reason = Some(sck(-3805));
        stored.source_kind = Some("window".into());
        let session = serde_json::to_value(response(&state, stored.clone())).unwrap();
        assert_eq!(
            session["failureReason"],
            "Your computer interrupted screen sharing. Try again."
        );
        let signaling = SignalingState {
            failure_reason: Some(sck(-3805)),
            ..SignalingState::default()
        };
        let snapshot = serde_json::to_value(signaling_snapshot(
            &stored.id,
            stored.generation,
            stored.source_kind.as_deref(),
            &signaling,
            0,
        ))
        .unwrap();
        assert_eq!(
            snapshot["failureReason"],
            "Your computer interrupted screen sharing. Try again."
        );
        crate::test_support::assert_http_contract("computerSignalingResponse", &snapshot);
    }

    #[tokio::test]
    async fn refused_input_says_which_lease_or_sequence_problem_stopped_control() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let mut create = session();
        create.host_installation_id = state.host_installation_id.clone();
        create.state = ComputerSessionState::Live;
        create.source_id = Some("display:1".into());
        state.store.insert_computer_session(&create).await.unwrap();
        let (commands, _requests) = tokio::sync::mpsc::channel(1);
        let (viewer_activity, _) = tokio::sync::mpsc::channel(1);
        let handle = SignalingHandle {
            signaling: Arc::new(tokio::sync::Mutex::new(SignalingState::default())),
            commands,
            viewer_activity,
        };
        let lease = |id: &str, seconds: i64| ComputerControlLeaseCreate {
            id: id.into(),
            client_request_id: format!("{id}-request"),
            session_id: create.id.clone(),
            owner_device_id: create.owner_device_id.clone(),
            host_installation_id: create.host_installation_id.clone(),
            conversation_id: create.conversation_id.clone(),
            session_generation: 1,
            source_id: create.source_id.clone(),
            geometry_revision: 0,
            now: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            expires_at: (Utc::now() + chrono::Duration::seconds(seconds))
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        };
        let code = |lease_id: &'static str, reason: &'static str| {
            let state = state.clone();
            let handle = &handle;
            async move { input_refusal_code(&state, handle, lease_id, reason).await }
        };

        // The phone's lease timed out: the store still holds it, past expiry.
        state
            .store
            .acquire_computer_control_lease(&lease("stale", -1))
            .await
            .unwrap();
        assert_eq!(
            code("stale", "lease binding is stale").await,
            "lease_expired"
        );
        // Released (by the phone or the end of the view).
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        state
            .store
            .release_computer_control_lease("stale", &create.owner_device_id, &now)
            .await
            .unwrap();
        state
            .store
            .acquire_computer_control_lease(&lease("held", 120))
            .await
            .unwrap();
        assert_eq!(code("held", "lease binding is stale").await, "view_changed");
        state
            .store
            .release_computer_control_lease("held", &create.owner_device_id, &now)
            .await
            .unwrap();
        assert_eq!(
            code("held", "lease binding is stale").await,
            "lease_released"
        );
        assert_eq!(code("missing", "lease not found").await, "lease_released");
        // Stopped at the Mac: the helper's revoke reason wins, for store and helper refusals.
        handle.signaling.lock().await.revoked_lease = Some(("held".into(), "local_stop".into()));
        assert_eq!(code("held", "lease binding is stale").await, "local_stop");
        assert_eq!(code("held", "lease_not_active").await, "local_stop");
        assert_eq!(code("other", "lease_not_active").await, "lease_not_active");
        assert_eq!(
            code("held", "input sequence must increase by one").await,
            "input_out_of_order"
        );
        assert_eq!(
            code("held", "computer helper did not acknowledge the request").await,
            "helper_unavailable"
        );
        assert_eq!(
            code("held", "window_not_focused").await,
            "window_not_focused"
        );

        let fallback = input_failure_reason("unrecognized");
        let mut seen = std::collections::BTreeSet::new();
        for reason in [
            "local_stop",
            "lease_expired",
            "lease_released",
            "input_out_of_order",
            "sequence_payload_conflict",
            "capture_not_live",
            "view_changed",
            "input_delivery_failed",
            "helper_unavailable",
            "lease_unavailable",
            "input_unavailable",
            "window_not_focused",
            "control_disabled",
            "input_desktop_unavailable",
            "input_timed_out",
            "request_timed_out",
            "xclip_required",
            "display_unavailable",
            "clipboard_too_large",
        ] {
            let words = input_failure_reason(reason);
            assert_ne!(words, fallback, "{reason} must say what stopped control");
            assert!(!words.contains('_'), "{reason} -> {words}");
            seen.insert(words);
        }
        assert!(input_failure_reason("local_stop").contains("stopped on the computer"));
        assert!(input_failure_reason("lease_expired").contains("timed out"));
        assert!(input_failure_reason("input_out_of_order").contains("out of order"));
        assert!(
            seen.len() >= 11,
            "lease, sequence and delivery refusals read differently"
        );
        // Linux and Windows helpers report these too.
        for reason in [Some("clipboard_no_text"), Some("clipboard_too_large"), None] {
            assert!(!clipboard_copy_reason(reason).contains("Mac"), "{reason:?}");
        }
    }

    #[tokio::test]
    async fn text_field_focus_reaches_the_controlling_phone() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let mut create = session();
        create.state = ComputerSessionState::Live;
        create.source_id = Some("window:7".into());
        create.geometry_revision = 2;
        let stored = state.store.insert_computer_session(&create).await.unwrap();
        let lease = |id: &str, seconds: i64| ComputerControlLeaseCreate {
            id: id.into(),
            client_request_id: "request".into(),
            session_id: stored.id.clone(),
            owner_device_id: stored.owner_device_id.clone(),
            host_installation_id: stored.host_installation_id.clone(),
            conversation_id: stored.conversation_id.clone(),
            session_generation: stored.generation,
            source_id: Some("window:7".into()),
            geometry_revision: 2,
            now: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            expires_at: (Utc::now() + chrono::Duration::seconds(seconds))
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        };
        state
            .store
            .acquire_computer_control_lease(&lease("lease", 120))
            .await
            .unwrap();
        let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
        let event = |geometry_revision: u64, text_input: serde_json::Value| {
            serde_json::json!({
                "event": "control.textFocus", "sessionID": stored.id, "generation": stored.generation,
                "leaseID": "lease", "requestID": "request", "geometryRevision": geometry_revision,
                "sourceID": "window:7", "textInput": text_input,
            })
        };
        let snapshot = |signaling: &SignalingState| {
            serde_json::to_value(signaling_snapshot(
                &stored.id,
                stored.generation,
                None,
                signaling,
                0,
            ))
            .unwrap()
        };
        assert!(
            snapshot(&*signaling.lock().await)
                .get("textInputFocused")
                .is_none(),
            "Unknown until the computer reports"
        );

        let applied =
            apply_helper_value_with_signaling(&state, &stored, &event(2, true.into()), &signaling)
                .await
                .unwrap();
        assert_eq!(applied, HelperValueAction::None);
        let focused = snapshot(&*signaling.lock().await);
        assert_eq!(focused["textInputFocused"], true);
        crate::test_support::assert_http_contract("computerSignalingResponse", &focused);

        // A report from the view before a window switch cannot raise the keyboard.
        signaling.lock().await.source_binding_revision = 3;
        apply_helper_value_with_signaling(&state, &stored, &event(2, false.into()), &signaling)
            .await
            .unwrap();
        assert_eq!(signaling.lock().await.text_input_focused, Some(true));
        apply_helper_value_with_signaling(&state, &stored, &event(3, false.into()), &signaling)
            .await
            .unwrap();
        assert_eq!(
            snapshot(&*signaling.lock().await)["textInputFocused"],
            false
        );

        let malformed =
            apply_helper_value_with_signaling(&state, &stored, &event(3, "yes".into()), &signaling)
                .await
                .unwrap();
        assert_eq!(malformed, HelperValueAction::ProtocolError);
        let mut other = event(3, true.into());
        other["sessionID"] = "other-session".into();
        apply_helper_value_with_signaling(&state, &stored, &other, &signaling)
            .await
            .unwrap();
        assert_eq!(signaling.lock().await.text_input_focused, Some(false));
        let mut other_request = event(3, true.into());
        other_request["requestID"] = "another-request".into();
        apply_helper_value_with_signaling(&state, &stored, &other_request, &signaling)
            .await
            .unwrap();
        assert_eq!(signaling.lock().await.text_input_focused, None);

        signaling.lock().await.clear_ephemeral(None);
        assert!(snapshot(&*signaling.lock().await)
            .get("textInputFocused")
            .is_none());
    }

    /// Control can end on the computer while it reads which element has
    /// focus. That late report must not raise the keyboard after control ended.
    #[tokio::test]
    async fn text_field_focus_from_an_ended_lease_is_dropped() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let mut create = session();
        create.state = ComputerSessionState::Live;
        let stored = state.store.insert_computer_session(&create).await.unwrap();
        let lease = |id: &str, seconds: i64| ComputerControlLeaseCreate {
            id: id.into(),
            client_request_id: format!("{id}-request"),
            session_id: stored.id.clone(),
            owner_device_id: stored.owner_device_id.clone(),
            host_installation_id: stored.host_installation_id.clone(),
            conversation_id: stored.conversation_id.clone(),
            session_generation: stored.generation,
            source_id: None,
            geometry_revision: 0,
            now: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            expires_at: (Utc::now() + chrono::Duration::seconds(seconds))
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        };
        let event = |lease_id: &str, text_input: bool| {
            serde_json::json!({
                "event": "control.textFocus", "sessionID": stored.id, "generation": stored.generation,
                "leaseID": lease_id, "requestID": format!("{lease_id}-request"),
                "geometryRevision": 0, "textInput": text_input,
            })
        };
        let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
        let report = |lease_id: &'static str, text_input: bool| {
            let (state, stored, signaling) = (state.clone(), stored.clone(), signaling.clone());
            let value = event(lease_id, text_input);
            async move {
                let applied =
                    apply_helper_value_with_signaling(&state, &stored, &value, &signaling)
                        .await
                        .unwrap();
                assert_eq!(applied, HelperValueAction::None);
                signaling.lock().await.text_input_focused
            }
        };

        state
            .store
            .acquire_computer_control_lease(&lease("held", 120))
            .await
            .unwrap();
        assert_eq!(report("held", true).await, Some(true));
        // Stopped on the computer: the helper's revoke reached the daemon
        // before the store released the lease, then a read finished late.
        signaling.lock().await.revoked_lease = Some(("held".into(), "local_stop".into()));
        assert_eq!(report("held", true).await, None);
        // Released by the phone or by the end of the view.
        signaling.lock().await.revoked_lease = None;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        state
            .store
            .release_computer_control_lease("held", &stored.owner_device_id, &now)
            .await
            .unwrap();
        signaling.lock().await.text_input_focused = Some(true);
        assert_eq!(report("held", true).await, None);
        // Timed out, and a lease that never existed.
        state
            .store
            .acquire_computer_control_lease(&lease("stale", -1))
            .await
            .unwrap();
        assert_eq!(report("stale", true).await, None);
        assert_eq!(report("unknown", true).await, None);
    }

    /// The dock route answers the owner only, and its icons come from the
    /// generalised icon route.
    #[tokio::test]
    async fn dock_route_lists_the_hosts_dock_and_serves_its_icons() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let mut create = session();
        create.host_installation_id = state.host_installation_id.clone();
        state.store.insert_computer_session(&create).await.unwrap();
        let auth = Some(Extension(AuthenticatedDevice {
            device_id: create.owner_device_id.clone(),
            session_binding: "fixture".into(),
        }));
        let app = |id: &str, icon: Option<Vec<u8>>| crate::computer_apps::DockApp {
            id: id.into(),
            name: id.into(),
            running: true,
            icon,
        };
        let icon = {
            let mut out = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(64, 64))
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            out.into_inner()
        };
        *crate::computer_apps::TEST_DOCK.lock().unwrap() = Some(crate::computer_apps::Dock {
            available: true,
            pinned: vec![app("org.example.Files.desktop", Some(icon.clone()))],
            recent: vec![app("xterm", None)],
        });
        let body = |response: Response| async move {
            let bytes = axum::body::to_bytes(response.into_body(), 65_536)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        assert_eq!(
            dock(State(state.clone()), None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let answer = body(dock(State(state.clone()), auth.clone()).await).await;
        assert_eq!(answer["available"], true);
        assert_eq!(
            answer["pinned"][0]["iconURL"],
            "/api/v1/computer/apps/icon?id=org.example.Files.desktop"
        );
        assert_eq!(
            answer["recent"][0],
            serde_json::json!({"id": "xterm", "name": "xterm", "running": true})
        );
        let served = installed_app_icon(
            State(state.clone()),
            auth.clone(),
            Query(InstalledAppQuery {
                id: "org.example.Files.desktop".into(),
            }),
        )
        .await;
        assert_eq!(served.status(), StatusCode::OK);
        assert!(body(served).await["iconPNG"]
            .as_str()
            .is_some_and(|png| !png.is_empty()));
        let missing = installed_app_icon(
            State(state),
            auth,
            Query(InstalledAppQuery { id: "xterm".into() }),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    /// The launcher lists installed apps and opens one only while the phone
    /// holds control of this exact view; launching renews the lease.
    #[tokio::test]
    async fn installed_apps_open_only_with_the_current_control_lease() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        state.computer_supervisor.set_control_capable(true);
        crate::computer_apps::set_catalog_for_test(vec![crate::computer_apps::InstalledApp {
            id: "com.apple.calculator".into(),
            name: "Calculator".into(),
            path: "/System/Applications/Calculator.app".into(),
        }]);
        let mut create = session();
        create.host_installation_id = state.host_installation_id.clone();
        create.state = ComputerSessionState::Live;
        create.source_id = Some("display:1".into());
        create.source_kind = Some("display".into());
        state.store.insert_computer_session(&create).await.unwrap();
        let (commands, mut requests) = tokio::sync::mpsc::channel(8);
        let (activity, _) = tokio::sync::mpsc::channel(1);
        let (stop, _) = tokio::sync::oneshot::channel();
        let renew = Arc::new(AtomicBool::new(true));
        let helper_renews = renew.clone();
        tokio::spawn(async move {
            while let Some(HelperCommand::Control {
                method, response, ..
            }) = requests.recv().await
            {
                let result = match method.as_str() {
                    "control.consent" => serde_json::json!({"granted": true}),
                    "control.activate" => serde_json::json!({"activated": true}),
                    "control.heartbeat" => {
                        serde_json::json!({"renewed": helper_renews.load(Ordering::SeqCst)})
                    }
                    _ => serde_json::json!({"accepted": true}),
                };
                let _ = response.send(Ok(result));
            }
        });
        *state.computer_supervisor.active.lock().await = Some(ManagedHelper {
            session_id: create.id.clone(),
            owner_device_id: create.owner_device_id.clone(),
            stop: Some(stop),
            join: tokio::spawn(async {}),
            signaling: Arc::new(tokio::sync::Mutex::new(SignalingState::default())),
            commands,
            viewer_activity: activity,
        });
        let auth = Some(Extension(AuthenticatedDevice {
            device_id: create.owner_device_id.clone(),
            session_binding: "fixture".into(),
        }));
        let body = |response: Response| async move {
            let bytes = axum::body::to_bytes(response.into_body(), 65_536)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        let launched = || crate::computer_apps::LAUNCHED.lock().unwrap().clone();

        let listed = installed_apps(State(state.clone()), auth.clone()).await;
        assert_eq!(
            body(listed).await,
            serde_json::json!({"available": true, "apps": [{"id": "com.apple.calculator", "name": "Calculator"}]})
        );
        assert_eq!(
            installed_apps(State(state.clone()), None).await.status(),
            StatusCode::UNAUTHORIZED
        );

        let open = |binding: serde_json::Value, bundle: &str| {
            let mut request = binding;
            request["bundleId"] = bundle.into();
            open_app(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(serde_json::from_value(request).unwrap()),
            )
        };
        // Without control there is no lease to prove.
        let no_lease = serde_json::json!({"leaseId":"missing","generation":1,"geometryRevision":0,"sourceId":"display:1"});
        assert_eq!(
            open(no_lease, "com.apple.calculator").await.status(),
            StatusCode::CONFLICT
        );
        assert!(launched().is_empty());

        let acquire = acquire_control(
            State(state.clone()),
            auth.clone(),
            Path(create.id.clone()),
            Json(
                serde_json::from_value(serde_json::json!({"clientRequestId":"launcher",
                    "generation":1,"geometryRevision":0,"sourceId":"display:1"}))
                .unwrap(),
            ),
        )
        .await;
        let acquired = body(acquire).await;
        assert_eq!(acquired["granted"], true);
        let binding = serde_json::json!({"leaseId":acquired["lease"]["id"],"generation":1,"geometryRevision":0,"sourceId":"display:1"});

        // A stale view, an unknown app or a malformed ID opens nothing.
        let mut stale = binding.clone();
        stale["geometryRevision"] = 7.into();
        assert_eq!(
            open(stale, "com.apple.calculator").await.status(),
            StatusCode::CONFLICT
        );
        let unknown = open(binding.clone(), "com.example.gone").await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body(unknown).await["reason"],
            "That app is no longer installed on this computer."
        );
        assert_eq!(
            open(binding.clone(), "../evil").await.status(),
            StatusCode::BAD_REQUEST
        );
        assert!(launched().is_empty());

        let opened = open(binding.clone(), "com.apple.calculator").await;
        assert_eq!(opened.status(), StatusCode::OK);
        let opened = body(opened).await;
        assert_eq!(opened["opened"], true);
        assert_eq!(opened["lease"]["id"], acquired["lease"]["id"]);
        assert_eq!(launched(), ["com.apple.calculator"]);

        // Control turned off on the computer: the helper refuses renewal, so
        // the launch is refused and the lease ends.
        renew.store(false, Ordering::SeqCst);
        assert_eq!(
            open(binding, "com.apple.calculator").await.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(launched(), ["com.apple.calculator"]);
    }

    #[tokio::test]
    async fn slow_input_allows_heartbeat_and_authenticated_release_before_delivery_finishes() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        state.computer_supervisor.set_control_capable(true);
        let mut create = session();
        create.host_installation_id = state.host_installation_id.clone();
        create.state = ComputerSessionState::Live;
        create.source_id = Some("display:1".into());
        create.source_kind = Some("display".into());
        state.store.insert_computer_session(&create).await.unwrap();
        let (commands, mut requests) = tokio::sync::mpsc::channel(8);
        let (activity, _) = tokio::sync::mpsc::channel(1);
        let (stop, _) = tokio::sync::oneshot::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let mut started = Some(started);
            let mut pending = None;
            while let Some(HelperCommand::Control {
                method, response, ..
            }) = requests.recv().await
            {
                let value = match method.as_str() {
                    "control.consent" => serde_json::json!({"granted":true}),
                    "control.activate" => serde_json::json!({"activated":true}),
                    "control.heartbeat" => serde_json::json!({"renewed":true}),
                    "control.input" => {
                        pending = Some(response);
                        started.take().unwrap().send(()).unwrap();
                        continue;
                    }
                    "control.release" => {
                        if let Some(response) = pending.take() {
                            let _ = response.send(Ok(
                                serde_json::json!({"accepted":false,"reason":"lease_not_active"}),
                            ));
                        }
                        let _ = response.send(Ok(serde_json::json!({"released":true})));
                        return;
                    }
                    _ => panic!("unexpected helper request {method}"),
                };
                response.send(Ok(value)).unwrap();
            }
        });
        *state.computer_supervisor.active.lock().await = Some(ManagedHelper {
            session_id: create.id.clone(),
            owner_device_id: create.owner_device_id.clone(),
            stop: Some(stop),
            join: tokio::spawn(async {}),
            signaling: Arc::new(tokio::sync::Mutex::new(SignalingState::default())),
            commands,
            viewer_activity: activity,
        });
        let auth = Some(Extension(AuthenticatedDevice {
            device_id: create.owner_device_id.clone(),
            session_binding: "fixture".into(),
        }));
        let acquired=acquire_control(State(state.clone()),auth.clone(),Path(create.id.clone()),Json(serde_json::from_value(serde_json::json!({"clientRequestId":"input-control","generation":1,"geometryRevision":0,"sourceId":"display:1"})).unwrap())).await;
        assert_eq!(acquired.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(acquired.into_body(), 65536)
            .await
            .unwrap();
        let acquired: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let binding = serde_json::json!({"leaseId":acquired["lease"]["id"],"generation":1,"geometryRevision":0,"sourceId":"display:1"});
        let mut batch = binding.clone();
        batch["sequence"] = serde_json::json!(1);
        batch["actions"] = serde_json::json!([{"type":"text","text":"owned fixture"}]);
        let sending = {
            let state = state.clone();
            let auth = auth.clone();
            let id = create.id.clone();
            tokio::spawn(async move {
                input_control(
                    State(state),
                    auth,
                    Path(id),
                    Json(serde_json::from_value(batch).unwrap()),
                )
                .await
            })
        };
        timeout(Duration::from_secs(10), ready)
            .await
            .unwrap()
            .unwrap();
        // The input deadline differs from admission: advance beyond the old
        // two-second deadline while keeping the helper's delivery pending.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(!sending.is_finished());
        tokio::time::resume();
        let heartbeat = timeout(
            Duration::from_secs(10),
            heartbeat_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(serde_json::from_value(binding.clone()).unwrap()),
            ),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat.status(), StatusCode::OK);
        assert!(!sending.is_finished());
        let released = timeout(
            Duration::from_secs(10),
            release_control(
                State(state.clone()),
                auth,
                Path(create.id),
                Json(serde_json::from_value(binding).unwrap()),
            ),
        )
        .await
        .unwrap();
        assert_eq!(released.status(), StatusCode::OK);
        assert_eq!(
            timeout(Duration::from_secs(10), sending)
                .await
                .unwrap()
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        worker.await.unwrap();
        state.computer_supervisor.active.lock().await.take();
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn window_sources_accept_acquire_heartbeat_and_input_with_a_verified_provider() {
        for kind in ["window", "display"] {
            let (_dir, state) = crate::ingestion::tests::fixture().await;
            state.computer_supervisor.set_control_capable(true);
            let mut create = session();
            create.host_installation_id = state.host_installation_id.clone();
            create.state = ComputerSessionState::Live;
            create.source_id = Some("window:42".into());
            create.source_kind = Some(kind.into());
            state.store.insert_computer_session(&create).await.unwrap();
            let (commands, mut requests) = tokio::sync::mpsc::channel(8);
            let (activity, _) = tokio::sync::mpsc::channel(1);
            let (stop, _) = tokio::sync::oneshot::channel();
            let helper_session_id = create.id.clone();
            let worker = tokio::spawn(async move {
                let mut methods = Vec::new();
                while let Some(HelperCommand::Control {
                    method,
                    params,
                    response,
                }) = requests.recv().await
                {
                    methods.push(method.clone());
                    let result = match method.as_str() {
                        "control.consent" => serde_json::json!({"granted": true}),
                        "control.activate" => serde_json::json!({"activated": true}),
                        "control.heartbeat" => serde_json::json!({"renewed": true}),
                        "control.input" => serde_json::json!({"accepted": true}),
                        "capture.rebind" => {
                            // Match the real helper boundary, including switches
                            // while merely viewing (no control lease exists).
                            assert_eq!(params["sessionID"], helper_session_id);
                            assert_eq!(params["generation"], 1);
                            let id = params["nextSourceID"].as_str().unwrap_or("display:1");
                            serde_json::json!({"accepted":true, "status": {"state":"capturing",
                                "sessionID":params["sessionID"], "generation":params["generation"],
                                "sourceID":id, "geometryRevision":params["geometryRevision"].as_u64().unwrap() + 1,
                                "configuration":{"width":1600,"height":900}},
                                "source":{"id":id,"title":"Selected view","kind":if id.starts_with("app:") {"window"} else {"display"},
                                "width":800,"height":600,"scale":1.0,"contentRect":{"x":0,"y":0,"width":800,"height":600}}})
                        }
                        _ => panic!("unexpected helper request {method}"),
                    };
                    response.send(Ok(result)).unwrap();
                    if methods.len() == 9 {
                        return methods;
                    }
                }
                methods
            });
            *state.computer_supervisor.active.lock().await = Some(ManagedHelper {
                session_id: create.id.clone(),
                owner_device_id: create.owner_device_id.clone(),
                stop: Some(stop),
                join: tokio::spawn(async {}),
                signaling: Arc::new(tokio::sync::Mutex::new(SignalingState::default())),
                commands,
                viewer_activity: activity,
            });
            let auth = Some(Extension(AuthenticatedDevice {
                device_id: create.owner_device_id.clone(),
                session_binding: "fixture".into(),
            }));
            let viewing_switch = set_source(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(
                    serde_json::from_value(serde_json::json!({
                        "generation":1, "geometryRevision":0, "sourceId":"window:42",
                        "source":{"id":"app:editor#42", "kind":"window"},
                    }))
                    .unwrap(),
                ),
            )
            .await;
            assert_eq!(viewing_switch.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(viewing_switch.into_body(), 65_536)
                .await
                .unwrap();
            let viewed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(viewed["accepted"], true);
            assert_eq!(viewed["session"]["id"], create.id);
            assert_eq!(viewed["session"]["state"], "live");
            assert_eq!(viewed["session"]["source"]["id"], "app:editor#42");
            assert_eq!(viewed["session"]["geometryRevision"], 1);
            assert!(viewed["lease"].is_null());
            let acquire = acquire_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(
                    serde_json::from_value(
                        serde_json::json!({"clientRequestId":"window-control", "generation":1,
                    "geometryRevision":1,"sourceId":"app:editor#42"}),
                    )
                    .unwrap(),
                ),
            )
            .await;
            assert_eq!(acquire.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(acquire.into_body(), 65_536)
                .await
                .unwrap();
            let acquired: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(acquired["granted"], true);
            let binding = serde_json::json!({"leaseId":acquired["lease"]["id"],"generation":1,"geometryRevision":1,"sourceId":"app:editor#42"});
            let heartbeat = heartbeat_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(serde_json::from_value(binding.clone()).unwrap()),
            )
            .await;
            assert_eq!(heartbeat.status(), StatusCode::OK);
            let mut batch = binding.clone();
            batch["sequence"] = serde_json::json!(1);
            batch["actions"] =
                serde_json::json!([{"type":"pointer","x":0.25,"y":0.5,"phase":"move"}]);
            let input = input_control(
                State(state.clone()),
                auth.clone(),
                Path(create.id.clone()),
                Json(serde_json::from_value(batch.clone()).unwrap()),
            )
            .await;
            assert_eq!(input.status(), StatusCode::OK);
            for (device, conversation) in [
                ("other-phone", create.conversation_id.as_str()),
                (create.owner_device_id.as_str(), "another-conversation"),
            ] {
                let denied = super::create(State(state.clone()), Some(Extension(AuthenticatedDevice {
                    device_id: device.into(), session_binding: "fixture".into(),
                })), Json(serde_json::from_value(serde_json::json!({
                    "clientRequestId":create.client_request_id, "conversationId":conversation,
                    "hostInstallationId":create.host_installation_id, "rebind":binding,
                })).unwrap())).await;
                assert_eq!(denied.status(), StatusCode::NOT_FOUND);
            }
            let mut current_binding = binding.clone();
            for target in [Some("app:editor#43"), None] {
                let mut request = current_binding.clone();
                request["source"] = target
                    .map(|id| serde_json::json!({"id":id, "kind":"window"}))
                    .unwrap_or(serde_json::Value::Null);
                // Exercise the existing sessions endpoint's explicit rebind
                // command, including its owner/request/conversation dispatch.
                let switched = super::create(
                    State(state.clone()),
                    auth.clone(),
                    Json(
                        serde_json::from_value(serde_json::json!({
                            "clientRequestId":create.client_request_id,
                            "conversationId":create.conversation_id,
                            "hostInstallationId":create.host_installation_id,
                            "rebind":request,
                        }))
                        .unwrap(),
                    ),
                )
                .await;
                assert_eq!(switched.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(switched.into_body(), 65_536)
                    .await
                    .unwrap();
                let switched: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(switched["accepted"], true);
                assert_eq!(switched["session"]["supportsControlSourceRebind"], true);
                assert_eq!(switched["session"]["id"], create.id);
                assert_eq!(switched["session"]["source"]["width"], 1600);
                assert_eq!(switched["session"]["source"]["height"], 900);
                assert_eq!(switched["session"]["source"]["crop"]["width"], 800.0);
                assert_eq!(switched["lease"]["id"], acquired["lease"]["id"]);
                assert_eq!(switched["lease"]["lastSequence"], 1);
                assert_eq!(
                    switched["lease"]["acquiredAt"],
                    acquired["lease"]["acquiredAt"]
                );
                current_binding["geometryRevision"] =
                    switched["session"]["geometryRevision"].clone();
                current_binding["sourceId"] = switched["session"]["source"]["id"].clone();
            }
            // An already queued old-view batch and heartbeat cannot reach the
            // newly selected window, even with the same lease and next sequence.
            batch["sequence"] = serde_json::json!(2);
            assert_eq!(
                input_control(
                    State(state.clone()),
                    auth.clone(),
                    Path(create.id.clone()),
                    Json(serde_json::from_value(batch.clone()).unwrap())
                )
                .await
                .status(),
                StatusCode::CONFLICT
            );
            assert_eq!(
                heartbeat_control(
                    State(state.clone()),
                    auth.clone(),
                    Path(create.id.clone()),
                    Json(serde_json::from_value(binding).unwrap())
                )
                .await
                .status(),
                StatusCode::CONFLICT
            );
            batch["geometryRevision"] = current_binding["geometryRevision"].clone();
            batch["sourceId"] = current_binding["sourceId"].clone();
            assert_eq!(
                input_control(
                    State(state.clone()),
                    auth.clone(),
                    Path(create.id.clone()),
                    Json(serde_json::from_value(batch).unwrap())
                )
                .await
                .status(),
                StatusCode::OK
            );
            assert_eq!(
                heartbeat_control(
                    State(state.clone()),
                    auth,
                    Path(create.id.clone()),
                    Json(serde_json::from_value(current_binding).unwrap())
                )
                .await
                .status(),
                StatusCode::OK
            );
            let durable = state
                .store
                .computer_control_lease(acquired["lease"]["id"].as_str().unwrap())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(durable.last_sequence, 2);
            assert_eq!(durable.geometry_revision, 3);
            assert_eq!(durable.source_id.as_deref(), Some("display:1"));
            assert_eq!(
                worker.await.unwrap(),
                [
                    "capture.rebind",
                    "control.consent",
                    "control.activate",
                    "control.heartbeat",
                    "control.input",
                    "capture.rebind",
                    "capture.rebind",
                    "control.input",
                    "control.heartbeat"
                ]
            );
            state.computer_supervisor.active.lock().await.take();
        }
    }

    #[test]
    fn signaling_binding_hides_wrong_host_or_conversation_and_conflicts_stale_generation() {
        let session = stored_session();
        let wrong_host = SignalingBinding {
            generation: 3,
            conversation_id: "conversation".into(),
            host_installation_id: "other-host".into(),
            peer_revision: None,
        };
        assert_eq!(
            signal_binding_error(&session, &wrong_host)
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let wrong_conversation = SignalingBinding {
            generation: 3,
            conversation_id: "other-conversation".into(),
            host_installation_id: "host".into(),
            peer_revision: None,
        };
        assert_eq!(
            signal_binding_error(&session, &wrong_conversation)
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let stale = SignalingBinding {
            generation: 2,
            conversation_id: "conversation".into(),
            host_installation_id: "host".into(),
            peer_revision: None,
        };
        assert_eq!(
            signal_binding_error(&session, &stale).unwrap().status(),
            StatusCode::CONFLICT
        );
        let current = SignalingBinding {
            generation: 3,
            conversation_id: "conversation".into(),
            host_installation_id: "host".into(),
            peer_revision: None,
        };
        assert!(signal_binding_error(&session, &current).is_none());
    }

    #[test]
    fn signaling_bounds_reject_oversized_or_wrong_m_line_values() {
        let binding = SignalingBinding {
            generation: 1,
            conversation_id: "conversation".into(),
            host_installation_id: "host".into(),
            peer_revision: None,
        };
        let base = SignalingCandidateRequest {
            binding,
            sequence: 1,
            candidate: "candidate:host".into(),
            sdp_mid: Some("0".into()),
            sdp_m_line_index: 0,
            username_fragment: Some("ufrag".into()),
        };
        assert!(valid_ice_candidate(&base));
        assert!(!valid_ice_candidate(&SignalingCandidateRequest {
            candidate: "candidate:host".into(),
            sdp_m_line_index: i32::MAX as u32 + 1,
            ..base
        }));
        let oversized = format!("v=0\na={} ", "x".repeat(MAX_SIGNALING_SDP_BYTES));
        assert!(!valid_sdp(&oversized));
        assert!(!valid_signaling_text(&"x".repeat(129), 128));
        assert!(!valid_signaling_text("bad\ntext", 128));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn signaling_answer_and_candidate_idempotency_is_reusable_after_rollback() {
        let mut signaling = SignalingState::default();
        signaling.offer = Some(SignalingDescription {
            kind: "offer".into(),
            sdp: "v=0\na=x".into(),
        });
        assert_eq!(signaling.reserve_answer("v=0\na=answer"), Ok(true));
        assert_eq!(
            signaling.reserve_answer("v=0\na=answer"),
            Err("computer answer is still being applied")
        );
        assert!(signaling.reserve_answer("v=0\na=other").is_err());
        signaling.rollback_answer("v=0\na=answer");
        assert_eq!(signaling.reserve_answer("v=0\na=other"), Ok(true));
        signaling.commit_answer("v=0\na=other");
        assert_eq!(signaling.reserve_answer("v=0\na=other"), Ok(false));

        let candidate = SignalingCandidate {
            sequence: 1,
            candidate: "candidate:host".into(),
            sdp_mid: Some("0".into()),
            sdp_m_line_index: 0,
            username_fragment: Some("ufrag".into()),
        };
        assert_eq!(signaling.reserve_candidate(candidate.clone()), Ok(true));
        assert_eq!(
            signaling.reserve_candidate(candidate.clone()),
            Err("computer ICE candidate is still being applied")
        );
        signaling.rollback_candidate(1);
        assert_eq!(signaling.reserve_candidate(candidate.clone()), Ok(true));
        signaling.commit_candidate(1);
        assert_eq!(signaling.remote_candidates.len(), 1);
        assert_eq!(signaling.reserve_candidate(candidate), Ok(false));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn signaling_state_requires_capture_and_encrypted_peer_for_live() {
        let mut signaling = SignalingState::default();
        signaling.capture_state = "ready".into();
        assert_eq!(signaling.public_state(), "preparing");
        signaling.capture_state = "capturing".into();
        assert_eq!(
            desired_media_state(&signaling).0,
            ComputerSessionState::Preparing
        );
        signaling.peer_state = "connected".into();
        assert_eq!(signaling.public_state(), "preparing");
        assert_eq!(
            desired_media_state(&signaling).0,
            ComputerSessionState::Preparing
        );
        signaling.first_frame_observed = true;
        assert_eq!(signaling.public_state(), "live");
        assert_eq!(
            desired_media_state(&signaling).0,
            ComputerSessionState::Live
        );
        // A dropped connection may come back: not live, not failed.
        signaling.peer_state = "disconnected".into();
        signaling.peer_interrupted_since = Some(tokio::time::Instant::now());
        assert_eq!(signaling.public_state(), "reconnecting");
        assert_eq!(
            desired_media_state(&signaling),
            (ComputerSessionState::Preparing, None)
        );
        // Only the supervisor's grace timer makes it final.
        signaling.failure_reason = Some("peer_connection_disconnected".into());
        assert_eq!(signaling.public_state(), "failed");
        assert_eq!(
            desired_media_state(&signaling).0,
            ComputerSessionState::Failed
        );
        signaling.clear_ephemeral(Some("closed".into()));
        assert!(signaling.peer_interrupted_since.is_none());
        assert!(signaling.offer.is_none());
        assert!(signaling.local_candidates.is_empty());
        assert!(signaling.remote_candidates.is_empty());
        assert!(signaling.remote_candidates_pending.is_empty());
        assert!(!signaling.first_frame_observed);
    }

    #[test]
    fn identity_valid_frame_metadata_is_required_to_observe_the_first_frame() {
        let session = stored_session();
        let wrong = HelperFrameMetadata {
            session_id: Some("other-session".into()),
            generation: Some(session.generation),
            source_width: Some(3024),
            source_height: Some(1964),
            scale: Some(2.0),
        };
        assert!(!frame_metadata_matches_session(Some(&wrong), &session));

        let valid = HelperFrameMetadata {
            session_id: Some(session.id.clone()),
            generation: Some(session.generation),
            source_width: Some(3024),
            source_height: Some(1964),
            scale: Some(2.0),
        };
        assert!(frame_metadata_matches_session(Some(&valid), &session));
    }

    #[tokio::test(start_paused = true)]
    async fn unbound_preparing_viewer_activity_lease_expires() {
        let (_activity_tx, mut activity_rx) = tokio::sync::mpsc::channel::<()>(1);
        let mut lease = ViewerActivityLease::with_duration(Duration::from_millis(50));
        let lease_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = lease.timer.as_mut() => break "viewer_timeout",
                    activity = activity_rx.recv() => {
                        if activity.is_none() {
                            break "viewer_timeout";
                        }
                        lease.reset();
                    }
                }
            }
        });
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(75)).await;
        tokio::task::yield_now().await;
        assert_eq!(lease_task.await.unwrap(), "viewer_timeout");
    }

    #[tokio::test]
    async fn signaling_handle_refreshes_only_after_endpoint_validation() {
        let supervisor = Arc::new(ComputerSessionSupervisor::default());
        let (activity_tx, mut activity_rx) = tokio::sync::mpsc::channel(1);
        let (stop_tx, _stop_rx) = tokio::sync::oneshot::channel();
        let (commands_tx, _commands_rx) = tokio::sync::mpsc::channel(1);
        *supervisor.active.lock().await = Some(ManagedHelper {
            session_id: "session".into(),
            owner_device_id: "owner".into(),
            stop: Some(stop_tx),
            join: tokio::spawn(async {}),
            signaling: Arc::new(tokio::sync::Mutex::new(SignalingState::default())),
            commands: commands_tx,
            viewer_activity: activity_tx,
        });

        assert!(supervisor
            .signaling_handle("session", "other-owner")
            .await
            .is_none());
        let handle = supervisor
            .signaling_handle("session", "owner")
            .await
            .expect("the authenticated owner should resolve its active helper");
        assert!(activity_rx.try_recv().is_err());
        handle.mark_viewer_active();
        assert_eq!(activity_rx.recv().await, Some(()));

        supervisor.active.lock().await.take();
    }

    #[tokio::test(start_paused = true)]
    async fn validated_owner_scoped_signaling_activity_keeps_viewer_lease_alive() {
        let supervisor = Arc::new(ComputerSessionSupervisor::default());
        let (activity_tx, mut activity_rx) = tokio::sync::mpsc::channel(1);
        let (_stop_tx, _stop_rx) = tokio::sync::oneshot::channel();
        let (commands_tx, _commands_rx) = tokio::sync::mpsc::channel(1);
        let join = tokio::spawn(async {});
        *supervisor.active.lock().await = Some(ManagedHelper {
            session_id: "session".into(),
            owner_device_id: "owner".into(),
            stop: Some(_stop_tx),
            join,
            signaling: Arc::new(tokio::sync::Mutex::new(SignalingState::default())),
            commands: commands_tx,
            viewer_activity: activity_tx,
        });

        let mut lease = ViewerActivityLease::with_duration(Duration::from_millis(100));
        let lease_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = lease.timer.as_mut() => break "viewer_timeout",
                    activity = activity_rx.recv() => {
                        if activity.is_none() {
                            break "viewer_timeout";
                        }
                        lease.reset();
                    }
                }
            }
        });
        tokio::task::yield_now().await;

        for _ in 0..2 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let handle = supervisor
                .signaling_handle("session", "owner")
                .await
                .expect("the authenticated owner should resolve its active helper");
            handle.mark_viewer_active();
            tokio::task::yield_now().await;
        }

        tokio::time::sleep(Duration::from_millis(60)).await;
        tokio::task::yield_now().await;
        assert!(!lease_task.is_finished());
        tokio::time::sleep(Duration::from_millis(60)).await;
        tokio::task::yield_now().await;
        assert_eq!(lease_task.await.unwrap(), "viewer_timeout");

        supervisor.active.lock().await.take();
    }

    #[tokio::test]
    async fn frame_metadata_is_identity_bound_and_refreshes_changed_geometry() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let mut create = session();
        create.source_id = Some("display:1".into());
        create.source_width = Some(1920);
        create.source_height = Some(1080);
        create.source_scale = Some(1.0);
        let stored = state.store.insert_computer_session(&create).await.unwrap();
        let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
        let value = serde_json::json!({
            "event": "capture.frameMetadata",
            "status": {
                "state": "capturing",
                "sessionID": stored.id,
                "generation": stored.generation,
                "sourceID": "display:1",
                "geometryRevision": 1,
                "lastFrame": {
                    "sessionID": "stale-session",
                    "generation": stored.generation + 1,
                    "sourceWidth": 3024,
                    "sourceHeight": 1964,
                    "scale": 2.0
                }
            }
        });

        assert_eq!(
            apply_helper_value_with_signaling(&state, &stored, &value, &signaling)
                .await
                .unwrap(),
            HelperValueAction::None
        );
        let current = state
            .store
            .computer_session(&stored.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.state, ComputerSessionState::Preparing.as_str());
        assert_eq!(current.source_width, Some(1920));
        assert_eq!(current.source_height, Some(1080));
        assert_eq!(current.source_scale, Some(1.0));
        assert_eq!(current.geometry_revision, 1);
        // Native rotation geometry must still reach the phone after
        // first-frame notification coalescing has taken effect.
        let mut valid = value.clone();
        valid["status"]["lastFrame"]["sessionID"] = serde_json::json!(stored.id);
        valid["status"]["lastFrame"]["generation"] = serde_json::json!(stored.generation);
        for (revision, width, height) in [(2, 720, 1280), (3, 1280, 720)] {
            valid["status"]["geometryRevision"] = serde_json::json!(revision);
            valid["status"]["lastFrame"]["sourceWidth"] = serde_json::json!(width);
            valid["status"]["lastFrame"]["sourceHeight"] = serde_json::json!(height);
            apply_helper_value_with_signaling(&state, &stored, &valid, &signaling)
                .await
                .unwrap();
            let updated = state
                .store
                .computer_session(&stored.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(updated.source_width, Some(width));
            assert_eq!(updated.source_height, Some(height));
            assert_eq!(updated.geometry_revision, revision);
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// The helper's own order on open, measured from Wonder 1.0.179's helper
    /// picking a real window: `capture.prepared` (awaitingSource) at ~100 ms,
    /// the pick reply (still awaitingSource), then `capture.sourceSelected`
    /// (ready) or `capture.failed` (window_not_found) ~70 ms later. A session
    /// that names its window never tells the phone to choose a source.
    #[tokio::test]
    async fn requested_source_never_reads_awaiting_source_while_the_pick_applies() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let rows: [(&str, Option<&str>, &str, &str, &str); 3] = [
            (
                "window requested",
                Some("app:com.example#7"),
                "capture.sourceSelected",
                "preparing",
                "preparing",
            ),
            (
                "missing window",
                Some("app:com.example#8"),
                "capture.failed",
                "preparing",
                "failed",
            ),
            (
                "first default display",
                None,
                "capture.sourceSelected",
                "awaitingSource",
                "preparing",
            ),
        ];
        for (index, (name, source, outcome, opening, settled)) in rows.into_iter().enumerate() {
            let mut create = session();
            create.id = format!("source-session-{index}");
            create.client_request_id = format!("source-request-{index}");
            create.source_id = source.map(str::to_owned);
            let stored = state.store.insert_computer_session(&create).await.unwrap();
            let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
            let selected_source = source.unwrap_or("display:1");
            let status = |state: &str, reason: Option<&str>| {
                serde_json::json!({
                    "state": state,
                    "sessionID": stored.id,
                    "generation": stored.generation,
                    "sourceID": if state == "ready" { serde_json::json!(selected_source) } else { serde_json::Value::Null },
                    "geometryRevision": 1,
                    "reason": reason,
                })
            };
            let ended = if outcome == "capture.failed" {
                status("failed", Some("window_not_found"))
            } else {
                status("ready", None)
            };
            let sequence = [
                (
                    serde_json::json!({"event": "capture.prepared", "status": status("awaitingSource", None)}),
                    opening,
                ),
                (
                    serde_json::json!({"id": 3, "result": {"status": status("awaitingSource", None)}}),
                    opening,
                ),
                (
                    serde_json::json!({"event": outcome, "status": ended,
                        "source": if outcome == "capture.sourceSelected" {
                            serde_json::json!({"id":selected_source,"title":"Chosen view",
                                "kind":if source.is_some() {"window"} else {"display"},"width":800,"height":600})
                        } else { serde_json::Value::Null }}),
                    settled,
                ),
            ];
            for (step, (value, expected)) in sequence.into_iter().enumerate() {
                let action = apply_helper_value_with_signaling(&state, &stored, &value, &signaling)
                    .await
                    .unwrap();
                let current = state
                    .store
                    .computer_session(&stored.id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(current.state, expected, "{name} step {step}");
                if step == 2 && outcome == "capture.sourceSelected" {
                    assert_eq!(
                        current.source_id.as_deref(),
                        Some(selected_source),
                        "{name}"
                    );
                    assert_eq!(action, HelperValueAction::StartCapture, "{name}");
                }
                let public = signaling.lock().await.public_state();
                assert_eq!(
                    public == "awaitingSource",
                    expected == "awaitingSource",
                    "{name} step {step}: stream reads {public}"
                );
            }
            if outcome == "capture.failed" {
                let current = state
                    .store
                    .computer_session(&stored.id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    current.failure_reason.as_deref(),
                    Some("window_not_found"),
                    "{name}"
                );
            }
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[test]
    fn clipboard_relay_accepts_only_bounded_text_for_the_current_subscription() {
        let value = serde_json::json!({"subscriptionID":"current", "sequence":3, "status":"watching", "text":"fixture"});
        let response = clipboard_response(&value, Some("current")).unwrap();
        assert_eq!(response.text.as_deref(), Some("fixture"));
        assert!(clipboard_response(&value, Some("stale")).is_none());
        assert!(clipboard_response(&value, None).is_none());
        let mut invalid = value.clone();
        invalid["text"] = serde_json::json!("é".repeat(MAX_SHARED_CLIPBOARD_BYTES / 2 + 1));
        assert!(clipboard_response(&invalid, Some("current")).is_none());
        invalid["text"] = serde_json::json!("a".repeat(MAX_SHARED_CLIPBOARD_BYTES));
        assert!(clipboard_response(&invalid, Some("current")).is_some());
        invalid["status"] = serde_json::json!("permissionRequired");
        assert!(clipboard_response(&invalid, Some("current")).is_none());
        let stopped = serde_json::json!({"sequence":3, "status":"off"});
        assert!(clipboard_response(&stopped, None).unwrap().text.is_none());
        // Preparing/paused capture can safely report off while the viewer asks
        // to subscribe; this is a wait state, not an unsupported host.
        assert_eq!(
            clipboard_response(&stopped, Some("current"))
                .unwrap()
                .status,
            "off"
        );
    }

    #[test]
    fn clipboard_signaling_envelope_matches_the_http_contract() {
        let validator = |definition: &str| {
            let authority: serde_json::Value = serde_json::from_str(include_str!(
                "../../../packages/protocol/schemas/wonder-http-v1.json"
            ))
            .unwrap();
            jsonschema::validator_for(&serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$ref": format!("#/$defs/{definition}"),
                "$defs": authority["$defs"]
            }))
            .unwrap()
        };
        let poll_schema = validator("computerSignalingPollRequest");
        let response_schema = validator("computerSignalingResponse");

        let old_poll = serde_json::json!({
            "generation": 1, "conversationId": "conversation", "hostInstallationId": "host", "cursor": 0
        });
        let mut poll = old_poll.clone();
        poll["clipboardSubscriptionId"] = serde_json::json!("subscription");
        poll["clipboardCursor"] = serde_json::json!(3);
        for wire in [&old_poll, &poll] {
            crate::tests::validate_http_contract("computerSignalingPollRequest", wire);
            serde_json::from_value::<SignalingPollRequest>(wire.clone()).unwrap();
        }
        for (field, value) in [
            ("clipboardSubscriptionId", serde_json::json!("")),
            (
                "clipboardSubscriptionId",
                serde_json::json!("x".repeat(129)),
            ),
            ("clipboardSubscriptionId", serde_json::json!("bad\u{1b}id")),
            ("clipboardCursor", serde_json::json!(-1)),
            ("clipboardText", serde_json::json!("fixture")),
        ] {
            let mut invalid = poll.clone();
            invalid[field] = value;
            assert!(!poll_schema.is_valid(&invalid), "{field}");
        }

        let mut snapshot = signaling_snapshot("session", 1, None, &SignalingState::default(), 0);
        crate::tests::validate_http_contract(
            "computerSignalingResponse",
            &serde_json::to_value(&snapshot).unwrap(),
        );
        for (host, subscription) in [
            (
                serde_json::json!({"subscriptionID":"subscription", "sequence":4, "status":"watching", "text":"a".repeat(MAX_SHARED_CLIPBOARD_BYTES)}),
                Some("subscription"),
            ),
            (
                serde_json::json!({"subscriptionID":"subscription", "sequence":4, "status":"permissionRequired"}),
                Some("subscription"),
            ),
            (serde_json::json!({"sequence":0, "status":"off"}), None),
        ] {
            snapshot.clipboard = clipboard_response(&host, subscription);
            assert!(snapshot.clipboard.is_some());
            crate::tests::validate_http_contract(
                "computerSignalingResponse",
                &serde_json::to_value(&snapshot).unwrap(),
            );
        }
        snapshot.clipboard = Some(ClipboardResponse::unavailable(Some("subscription"), 4));
        crate::tests::validate_http_contract(
            "computerSignalingResponse",
            &serde_json::to_value(&snapshot).unwrap(),
        );

        let base = serde_json::to_value(&snapshot).unwrap();
        for clipboard in [
            serde_json::json!({"subscriptionId":null, "sequence":1, "status":"off", "text":"fixture"}),
            serde_json::json!({"subscriptionId":"subscription", "sequence":1, "status":"permissionRequired", "text":"fixture"}),
            serde_json::json!({"subscriptionId":"subscription", "sequence":1, "status":"unavailable", "text":"fixture"}),
            serde_json::json!({"subscriptionId":null, "sequence":1, "status":"watching", "text":"fixture"}),
            serde_json::json!({"subscriptionId":"subscription", "sequence":1, "status":"watching", "text":""}),
            serde_json::json!({"subscriptionId":"subscription", "sequence":1, "status":"paused"}),
            serde_json::json!({"subscriptionId":"subscription", "sequence":-1, "status":"watching"}),
            serde_json::json!({"subscriptionId":"subscription", "status":"watching"}),
            serde_json::json!({"subscriptionId":"subscription", "sequence":1, "status":"watching", "types":["public.utf8-plain-text"]}),
        ] {
            let mut invalid = base.clone();
            invalid["clipboard"] = clipboard.clone();
            assert!(!response_schema.is_valid(&invalid), "{clipboard}");
        }
    }

    #[test]
    fn signaling_poll_cursor_is_stateless_and_retries_lost_responses() {
        let mut signaling = SignalingState::default();
        signaling.local_candidates.push_back(SignalingCandidate {
            sequence: 1,
            candidate: "candidate:one".into(),
            sdp_mid: None,
            sdp_m_line_index: 0,
            username_fragment: None,
        });
        let first = signaling_snapshot("session", 1, None, &signaling, 0);
        assert_eq!(first.next_cursor, 1);
        let retry = signaling_snapshot("session", 1, None, &signaling, 0);
        assert_eq!(retry.cursor, 0);
        assert_eq!(retry.next_cursor, 1);
        assert_eq!(retry.candidates, first.candidates);
        let ahead = signaling_snapshot("session", 1, None, &signaling, 9);
        assert_eq!(ahead.cursor, 9);
        assert_eq!(ahead.next_cursor, 9);
        assert!(ahead.candidates.is_empty());
    }

    #[tokio::test]
    async fn fake_jsonl_helper_is_handshaken_idempotent_and_reaped() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        let helper_log = directory.path().join("computer-helper.jsonl");
        let stopped = directory.path().join("computer-helper.stopped");
        let capabilities = serde_json::json!({
            "id": 1,
            "result": {
                "protocolVersion": 1,
                "control": {
                    "available": true,
                    "provider": "core-graphics-v1",
                    "requiresAccessibility": true,
                    "windowControlAvailable": false
                }
            }
        });
        let response = serde_json::json!({
            "id": 2,
            "result": {"status": {
                "state": "awaitingSource",
                "sessionID": "fake-session",
                "generation": 1,
                "sourceID": null,
                "geometryRevision": 0
            }}
        });
        let response_two = serde_json::json!({
            "id": 3,
            "result": {"status": {
                "state": "awaitingSource",
                "sessionID": "fake-session",
                "generation": 1,
                "sourceID": null,
                "geometryRevision": 0
            }}
        });
        let helper = directory.path().join("fake-computer-helper.sh");
        let script = format!(
            "#!/bin/sh\nread first\nprintf '%s\\n' \"$first\" > '{}'\nprintf '%s\\n' '{}'\nread second\nprintf '%s\\n' \"$second\" >> '{}'\nprintf '%s\\n' '{}'\nread third\nprintf '%s\\n' \"$third\" >> '{}'\nprintf '%s\\n' '{}'\nwhile IFS= read -r line; do case \"$line\" in *stop*) touch '{}' ; exit 0;; esac; done\n",
            helper_log.display(),
            capabilities,
            helper_log.display(),
            response,
            helper_log.display(),
            response_two,
            stopped.display(),
        );
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        state.computer_use_enabled = true;
        state.computer_use_bin = Some(helper.clone());

        let mut requested = session();
        requested.video_quality = "medium".into();
        requested.video_max_fps = 60;
        let stored = state
            .store
            .insert_computer_session(&requested)
            .await
            .unwrap();
        let supervisor = Arc::clone(&state.computer_supervisor);
        // This is a healthy handshake fixture. OS startup and store I/O must
        // not consume its five-second protocol startup or viewer lease clocks.
        tokio::time::pause();
        super::tests::observe_helper(async {
            supervisor
                .start(state.clone(), stored.clone())
                .await
                .unwrap();
            // Retrying the same owner/session must not spawn another worker.
            supervisor
                .start(state.clone(), stored.clone())
                .await
                .unwrap();
            loop {
                let current = state
                    .store
                    .computer_session("fake-session")
                    .await
                    .unwrap()
                    .unwrap();
                assert_ne!(
                    current.state,
                    ComputerSessionState::Failed.as_str(),
                    "capture helper failed: {:?}",
                    current.failure_reason
                );
                // The prepare reply alone does not prove the third request
                // was received. Wait for complete records before reading them.
                let requests = std::fs::read_to_string(&helper_log)
                    .unwrap_or_default()
                    .lines()
                    .filter(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
                    .count();
                if current.state == ComputerSessionState::AwaitingSource.as_str() && requests == 3 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        tokio::time::resume();

        let requests = std::fs::read_to_string(&helper_log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["method"], "capabilities");
        assert_eq!(requests[1]["method"], "capture.prepare");
        assert_eq!(requests[2]["method"], "capture.pick");
        assert!(requests.iter().all(|request| {
            !request["method"]
                .as_str()
                .unwrap_or_default()
                .starts_with("control.")
        }));
        assert_eq!(requests[1]["params"]["sessionID"], "fake-session");
        assert_eq!(requests[1]["params"]["generation"], 1);
        assert_eq!(requests[1]["params"]["videoQuality"], "medium");
        assert_eq!(requests[1]["params"]["videoMaxFramesPerSecond"], 60);
        assert!(requests[1]["params"]["handshake"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        assert!(supervisor.control_capable());
        let mut selected = stored.clone();
        selected.source_kind = Some("window".into());
        let window_control = session_control_capability(&state, &selected);
        assert!(!window_control.available);
        assert_eq!(
            window_control.reason,
            "This window is view only. Show the whole screen to control the Mac."
        );
        selected.source_kind = Some("display".into());
        assert!(session_control_capability(&state, &selected).available);

        let stale = serde_json::json!({
            "event": "capture.sourceSelected",
            "status": {
                "state": "ready",
                "sessionID": "other-session",
                "generation": 1,
                "sourceID": "display:stale",
                "geometryRevision": 99
            }
        });
        assert!(apply_helper_value(&state, &stored, &stale).await.unwrap());
        let current = state
            .store
            .computer_session("fake-session")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.state, ComputerSessionState::AwaitingSource.as_str());
        assert_eq!(current.geometry_revision, 0);

        supervisor.stop(&state, &stored.id, "test_cleanup").await;
        assert!(stopped.exists());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unviewed_helper_timeout_stops_and_ends_durable_session() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        let capabilities = serde_json::json!({
            "id": 1,
            "result": {
                "protocolVersion": 1,
                "control": {
                    "available": false,
                    "provider": "core-graphics-v1",
                    "requiresAccessibility": true
                }
            }
        });
        let response = serde_json::json!({
            "id": 2,
            "result": {"status": {
                "state": "awaitingSource",
                "sessionID": "fake-session",
                "generation": 1,
                "sourceID": null,
                "geometryRevision": 0
            }}
        });
        let response_two = serde_json::json!({
            "id": 3,
            "result": {"status": {
                "state": "awaitingSource",
                "sessionID": "fake-session",
                "generation": 1,
                "sourceID": null,
                "geometryRevision": 0
            }}
        });
        let helper = directory.path().join("fake-computer-helper-timeout.sh");
        let script = format!(
            "#!/bin/sh\nread first\nprintf '%s\\n' '{}'\nread second\nprintf '%s\\n' '{}'\nread third\nprintf '%s\\n' '{}'\nwhile IFS= read -r line; do case \"$line\" in *stop*) exit 0;; esac; done\n",
            capabilities,
            response,
            response_two,
        );
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        state.computer_use_enabled = true;
        state.computer_use_bin = Some(helper);
        let stored = state
            .store
            .insert_computer_session(&session())
            .await
            .unwrap();
        let supervisor = Arc::clone(&state.computer_supervisor);
        supervisor
            .start_with_timings(
                state.clone(),
                stored.clone(),
                ViewTimings {
                    viewer_activity_lease: Duration::from_millis(500),
                    ..ViewTimings::DEFAULT
                },
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let ended = state
                    .store
                    .computer_session(&stored.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state
                    == ComputerSessionState::Ended.as_str();
                let reaped = supervisor.active.lock().await.is_none();
                if ended && reaped {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let current = state
            .store
            .computer_session(&stored.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.state, ComputerSessionState::Ended.as_str());
        assert_eq!(current.failure_reason.as_deref(), Some("viewer_timeout"));
        assert!(current.ended_at.is_some());
        assert!(supervisor.active.lock().await.is_none());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// Capabilities, prepare and pick replies for a fixture helper session.
    /// A helper that accepts the pick (status `state`) and then never sends a
    /// frame, started with the given clocks for a window the phone polls.
    async fn start_view_without_frames(
        state_name: &str,
        timings: ViewTimings,
    ) -> (
        tempfile::TempDir,
        AppState,
        StoredComputerSession,
        Arc<ComputerSessionSupervisor>,
    ) {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        let mut replies = startup_replies("fake-session");
        let source = if state_name == "awaitingSource" {
            serde_json::Value::Null
        } else {
            serde_json::json!("app:w#1")
        };
        let status = serde_json::json!({
            "state": state_name, "sessionID": "fake-session", "generation": 1,
            "sourceID": source, "geometryRevision": 0});
        replies[1] = serde_json::json!({"id": 2, "result": {"status": status}}).to_string();
        replies[2] = serde_json::json!({"id": 3, "result": {"accepted": true,
            "action": "pick", "status": status}})
        .to_string();
        state.computer_use_enabled = true;
        state.computer_use_bin = Some(startup_helper(
            directory.path(),
            "no-frame-helper",
            &replies,
            WAITS_FOR_STOP,
        ));
        let mut create = session();
        if state_name != "awaitingSource" {
            create.source_id = Some("app:w#1".into());
            create.source_kind = Some("window".into());
        }
        let stored = state.store.insert_computer_session(&create).await.unwrap();
        let supervisor = Arc::clone(&state.computer_supervisor);
        supervisor
            .start_with_timings(state.clone(), stored.clone(), timings)
            .await
            .unwrap();
        (directory, state, stored, supervisor)
    }

    /// The phone's `signal_poll` marks the viewer active on every request.
    async fn poll_as_phone(supervisor: &ComputerSessionSupervisor) -> bool {
        match supervisor.active.lock().await.as_ref() {
            Some(helper) => {
                let _ = helper.viewer_activity.try_send(());
                true
            }
            None => false,
        }
    }

    /// SM-03: the helper accepted the pick and never sent a frame while the
    /// phone kept polling (each poll renews the idle lease). The view must
    /// fail on its own clock with a reason the phone can retry from.
    #[tokio::test]
    async fn view_with_no_first_frame_fails_while_the_phone_keeps_polling() {
        let timings = ViewTimings {
            viewer_activity_lease: Duration::from_secs(10),
            opening_deadline: Duration::from_secs(10),
        };
        let (directory, state, stored, supervisor) =
            start_view_without_frames("starting", timings).await;
        let started = std::time::Instant::now();
        while poll_as_phone(&supervisor).await && started.elapsed() < Duration::from_secs(30) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let elapsed = started.elapsed();
        let row = state
            .store
            .computer_session(&stored.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state,
            ComputerSessionState::Failed.as_str(),
            "after {elapsed:?}"
        );
        assert_eq!(row.failure_reason.as_deref(), Some("first_frame_timeout"));
        assert!(
            elapsed >= Duration::from_secs(10) && elapsed < Duration::from_secs(30),
            "ended after {elapsed:?}, not at the 10 s opening deadline"
        );
        until_slot_empty(&supervisor, Duration::from_secs(10)).await;
        let ended = ended_records(directory.path());
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0]["closedBy"], "first_frame_timeout");
        assert_eq!(
            failure_words("first_frame_timeout", Some("simulator")),
            "This simulator didn’t show a picture in time. Try again."
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// Choosing what to share is the person's pace, not a stall: the opening
    /// deadline runs only in opening phases and restarts on each re-entry.
    #[tokio::test]
    async fn opening_deadline_follows_the_phase_not_the_polling() {
        let mut deadline = OpeningDeadline::with_budget(Duration::from_secs(20));
        assert!(!deadline.armed);
        deadline.sync(ViewPhase::Preparing);
        assert!(deadline.armed);
        let first = deadline.timer.deadline();
        deadline.sync(ViewPhase::Connecting);
        assert_eq!(
            deadline.timer.deadline(),
            first,
            "moving within opening keeps the clock"
        );
        deadline.sync(ViewPhase::AwaitingSource);
        assert!(!deadline.armed, "choosing a source has no deadline");
        deadline.sync(ViewPhase::Live);
        assert!(!deadline.armed);
        tokio::time::sleep(Duration::from_millis(20)).await;
        deadline.sync(ViewPhase::Preparing);
        assert!(deadline.armed);
        assert!(
            deadline.timer.deadline() > first,
            "re-entering opening restarts the clock"
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn view_phase_names_each_stage_and_only_opening_phases_run_the_deadline() {
        let mut signaling = SignalingState::default();
        assert_eq!(signaling.phase(), ViewPhase::Preparing);
        signaling.capture_state = "awaitingSource".into();
        assert_eq!(signaling.phase(), ViewPhase::AwaitingSource);
        signaling.capture_state = "capturing".into();
        signaling.first_frame_observed = true;
        // A frame without the phone's link is still opening.
        assert_eq!(signaling.phase(), ViewPhase::Preparing);
        signaling.peer_state = "connected".into();
        assert_eq!(signaling.phase(), ViewPhase::Live);
        signaling.peer_interrupted_since = Some(tokio::time::Instant::now());
        assert_eq!(signaling.phase(), ViewPhase::Reconnecting);
        signaling.peer_interrupted_since = None;
        signaling.fail("first_frame_timeout".into());
        assert_eq!(signaling.phase(), ViewPhase::Failed);
        for phase in [
            ViewPhase::Closed,
            ViewPhase::Failed,
            ViewPhase::Reconnecting,
            ViewPhase::Live,
            ViewPhase::AwaitingSource,
        ] {
            assert!(!phase.is_opening(), "{phase:?}");
        }
        for phase in [
            ViewPhase::Preparing,
            ViewPhase::OfferReady,
            ViewPhase::Connecting,
        ] {
            assert!(phase.is_opening(), "{phase:?}");
        }
    }

    fn startup_replies(session_id: &str) -> [String; 3] {
        let status = serde_json::json!({
            "state": "awaitingSource",
            "sessionID": session_id,
            "generation": 1,
            "sourceID": null,
            "geometryRevision": 0
        });
        [
            serde_json::json!({"id": 1, "result": {"protocolVersion": 1}}).to_string(),
            serde_json::json!({"id": 2, "result": {"status": status}}).to_string(),
            serde_json::json!({"id": 3, "result": {"status": status}}).to_string(),
        ]
    }

    /// A shell helper that answers the three startup requests, then runs `rest`.
    fn startup_helper(
        directory: &FsPath,
        name: &str,
        replies: &[String; 3],
        rest: &str,
    ) -> PathBuf {
        let helper = directory.join(name);
        let script = format!(
            "#!/bin/sh\necho $$ > '{}'\nread first\nprintf '%s\\n' '{}'\nread second\nprintf '%s\\n' '{}'\nread third\nprintf '%s\\n' '{}'\n{rest}\n",
            directory.join(format!("{name}.pid")).display(),
            replies[0],
            replies[1],
            replies[2],
        );
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        helper
    }

    const WAITS_FOR_STOP: &str =
        "while IFS= read -r line; do case \"$line\" in *stop*) exit 0;; esac; done";

    async fn until_slot_empty(supervisor: &ComputerSessionSupervisor, limit: Duration) {
        tokio::time::timeout(limit, async {
            while supervisor.active.lock().await.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the computer view slot was not freed");
    }

    fn ended_records(directory: &FsPath) -> Vec<serde_json::Value> {
        std::fs::read_to_string(directory.join("logs/test.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| record["event"] == "computer_session_ended")
            .collect()
    }

    #[tokio::test]
    async fn helper_that_closes_its_output_but_stays_up_is_reaped_and_frees_the_view() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        // Authenticate, close stdout, then stay up without reading or exiting.
        let helper = startup_helper(
            directory.path(),
            "silent-helper",
            &startup_replies("fake-session"),
            "exec 1>&-\nexec sleep 60",
        );
        state.computer_use_enabled = true;
        state.computer_use_bin = Some(helper);
        let stored = state
            .store
            .insert_computer_session(&session())
            .await
            .unwrap();
        let supervisor = Arc::clone(&state.computer_supervisor);
        supervisor
            .start(state.clone(), stored.clone())
            .await
            .unwrap();
        until_slot_empty(&supervisor, Duration::from_secs(10)).await;

        let current = state
            .store
            .computer_session(&stored.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.state, ComputerSessionState::Failed.as_str());
        assert_eq!(
            current.failure_reason.as_deref(),
            Some("helper_output_closed")
        );
        let pid = std::fs::read_to_string(directory.path().join("silent-helper.pid")).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!alive.success(), "helper {pid} was not reaped");
        let records = ended_records(directory.path());
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["reason"], "helper_output_closed");
        assert_eq!(records[0]["stage"], "connecting");
        assert_eq!(records[0]["severity"], "warn");
        assert!(records[0]["helperSignal"].is_number() || records[0]["helperExitCode"].is_number());

        // The freed slot takes the next view.
        let mut next = session();
        next.id = "fake-session-2".into();
        next.client_request_id = "fake-request-2".into();
        state.computer_use_bin = Some(startup_helper(
            directory.path(),
            "next-helper",
            &startup_replies("fake-session-2"),
            WAITS_FOR_STOP,
        ));
        let next = state.store.insert_computer_session(&next).await.unwrap();
        supervisor.start(state.clone(), next.clone()).await.unwrap();
        assert_eq!(
            supervisor
                .active
                .lock()
                .await
                .as_ref()
                .map(|active| active.session_id.clone()),
            Some(next.id.clone())
        );
        supervisor.stop(&state, &next.id, "test_cleanup").await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn the_last_report_before_a_helper_exits_is_the_kept_cause() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        state.computer_use_enabled = true;
        let supervisor = Arc::clone(&state.computer_supervisor);
        // Hold both FIFO ends before launching the helper. On assertion failure,
        // releasing through this owned nonblocking FD cannot wait for a late
        // reader, and lets the suffix writer finish when it is scheduled.
        struct SuffixGate {
            path: PathBuf,
            writer: Option<std::fs::File>,
            released: bool,
        }
        impl SuffixGate {
            fn release(&mut self) -> std::io::Result<()> {
                use std::io::Write;
                if !self.released {
                    if let Some(writer) = self.writer.as_mut() {
                        writer.write_all(b"release\n")?;
                    }
                    self.released = true;
                }
                Ok(())
            }
        }
        impl Drop for SuffixGate {
            fn drop(&mut self) {
                let _ = self.release();
            }
        }
        // Real child/SQLite I/O runs while the supervisor clock stays still.
        // OS scheduling cannot consume the production 500 ms drain budget.
        tokio::time::pause();
        for run in 0..9 {
            let mut create = session();
            create.id = format!("exiting-session-{run}");
            create.client_request_id = format!("exiting-request-{run}");
            let mut replies = startup_replies(&create.id);
            if run == 8 {
                // Only the final pick reply reaches awaitingSource, so observing
                // that state proves startup_pending is clear before advancing.
                let mut preparing: serde_json::Value = serde_json::from_str(&replies[1]).unwrap();
                preparing["result"]["status"]["state"] = "starting".into();
                replies[1] = preparing.to_string();
            }
            let failed = serde_json::json!({
                "event": "capture.failed",
                "status": { "state": "failed", "reason": "capture_backend_failed",
                    "sessionID": create.id, "generation": 1,
                    "sourceID": null, "geometryRevision": 0 }
            })
            .to_string();
            let mut gate = SuffixGate {
                path: directory.path().join(format!("suffix-{run}")),
                writer: None,
                released: false,
            };
            let rest = if run < 4 {
                format!("printf '%s\\n' '{failed}'\nexit 3")
            } else {
                assert!(std::process::Command::new("mkfifo")
                    .arg(&gate.path)
                    .status()
                    .unwrap()
                    .success());
                use std::os::unix::fs::OpenOptionsExt;
                gate.writer = Some(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&gate.path)
                        .unwrap(),
                );
                // Open the reader before forking: a reaped main helper proves
                // its background writer inherited an already-open FIFO FD.
                let (head, tail) = failed.split_at(failed.len() / 2);
                format!("exec 3< '{}'\nprintf '%s' '{head}'\n(IFS= read -r release <&3; printf '%s\\n' '{tail}') &\nexit 3", gate.path.display())
            };
            let name = format!("exiting-helper-{run}");
            state.computer_use_bin = Some(startup_helper(directory.path(), &name, &replies, &rest));
            let stored = super::tests::observe_helper(state.store.insert_computer_session(&create))
                .await
                .unwrap();
            let mut session_events = state.events.subscribe();
            super::tests::observe_helper(supervisor.start(state.clone(), stored.clone()))
                .await
                .unwrap();
            if run >= 4 {
                super::tests::observe_helper(async {
                    let pid_file = directory.path().join(format!("{name}.pid"));
                    let pid: i32 = loop {
                        if let Some(pid) = std::fs::read_to_string(&pid_file)
                            .ok()
                            .and_then(|value| value.trim().parse().ok())
                        {
                            break pid;
                        }
                        tokio::task::yield_now().await;
                    };
                    while crate::platform::process_alive(pid) {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                assert!(
                    supervisor.active.lock().await.is_some(),
                    "a partial line must wait for its suffix or drain deadline"
                );
                if run == 8 {
                    super::tests::observe_helper(async {
                        loop {
                            let event = session_events.recv().await.unwrap();
                            if matches!(event.event, WonderEvent::ComputerSessionChanged {
                                session_id, state, ..
                            } if session_id == stored.id && state == ComputerSessionState::AwaitingSource.as_str()) {
                                break;
                            }
                        }
                    }).await;
                    // Publication follows the final pick's SQLite journal write.
                    // No producer I/O remains before returning to child.wait.
                    // A writer that never finishes cannot hold the view forever.
                    // Bounded clock steps allow child.wait and SQLite continuations
                    // to be scheduled before the actual drain deadline is armed.
                    for _ in 0..20 {
                        if supervisor.active.lock().await.is_none() {
                            break;
                        }
                        tokio::task::yield_now().await;
                        tokio::time::advance(HELPER_EXIT_DRAIN).await;
                    }
                } else {
                    gate.release().unwrap();
                }
            }
            super::tests::observe_helper(async {
                while supervisor.active.lock().await.is_some() {
                    tokio::task::yield_now().await;
                }
                // The slot is freed before the final async log write.
                while ended_records(directory.path()).len() < run + 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            if run == 8 {
                gate.release().unwrap();
            }
            let current = super::tests::observe_helper(state.store.computer_session(&stored.id))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(current.state, ComputerSessionState::Failed.as_str());
            assert_eq!(
                current.failure_reason.as_deref(),
                Some(if run == 8 {
                    "helper_exited"
                } else {
                    "capture_backend_failed"
                }),
                "run {run}"
            );
        }
        tokio::time::resume();
        let records = ended_records(directory.path());
        assert_eq!(records.len(), 9);
        assert!(records
            .iter()
            .enumerate()
            .all(|(run, record)| record["reason"]
                == if run == 8 {
                    "helper_exited"
                } else {
                    "capture_backend_failed"
                }
                && record["helperExitCode"] == 3
                && record["stage"] == "connecting"));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_refused_startup_pick_fails_the_view_instead_of_preparing() {
        for reason in [
            "source_not_found",
            "wayland_unsupported",
            "x11_display_required",
        ] {
            let (directory, mut state) = crate::ingestion::tests::fixture().await;
            let mut replies = startup_replies("fake-session");
            // Preparing advances the binding past the launch-time revision; the
            // refusal must still fail the row.
            replies[1] = serde_json::json!({
                "id": 2,
                "result": {"status": {
                    "state": "awaitingSource",
                    "sessionID": "fake-session",
                    "generation": 1,
                    "sourceID": "app:closed#1",
                    "geometryRevision": 2
                }}
            })
            .to_string();
            replies[2] = serde_json::json!({
                "id": 3,
                "result": {
                    "accepted": false,
                    "action": "pick",
                    "errorCode": "source_not_found",
                    "status": {
                        "state": "awaitingSource",
                        "sessionID": "fake-session",
                        "generation": 1,
                        "sourceID": null,
                        "geometryRevision": 0
                    }
                }
            })
            .to_string();
            if reason != "source_not_found" {
                replies[2] = serde_json::json!({"id": 3, "error": {"code": reason}}).to_string();
            }
            state.computer_use_enabled = true;
            state.computer_use_bin = Some(startup_helper(
                directory.path(),
                "refusing-helper",
                &replies,
                WAITS_FOR_STOP,
            ));
            let mut create = session();
            create.source_id = Some("app:closed#1".into());
            create.source_kind = Some("window".into());
            let stored = state.store.insert_computer_session(&create).await.unwrap();
            let supervisor = Arc::clone(&state.computer_supervisor);
            supervisor
                .start(state.clone(), stored.clone())
                .await
                .unwrap();
            // Well before the 30 s viewer timeout that used to end it.
            until_slot_empty(&supervisor, Duration::from_secs(10)).await;
            let current = state
                .store
                .computer_session(&stored.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(current.state, ComputerSessionState::Failed.as_str());
            assert_eq!(current.failure_reason.as_deref(), Some(reason));
            let records = ended_records(directory.path());
            assert_eq!(records[0]["reason"], reason);
            assert_eq!(records[0]["closedBy"], "capture_pick_refused");
            assert_eq!(records[0]["stage"], "startup");
            assert_eq!(records[0]["helperExitCode"], 0);
            state.projects.codex.lock().await.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn viewer_timeout_ends_a_failed_view_without_replacing_its_cause() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let mut create = session();
        create.state = ComputerSessionState::Live;
        let stored = state.store.insert_computer_session(&create).await.unwrap();
        fail_session(&state, &stored, "peer_setup_timeout").await;
        end_session_for_owner(&state, &stored, "viewer_timeout").await;
        let current = state
            .store
            .computer_session(&stored.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.state, ComputerSessionState::Ended.as_str());
        assert_eq!(
            current.failure_reason.as_deref(),
            Some("peer_setup_timeout")
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// A helper that stays up until stopped and logs each stop. With
    /// `capture_state` it reports that capture state for its own session.
    fn viewer_helper(directory: &FsPath, name: &str, capture_state: &str) -> PathBuf {
        let stops = directory.join(format!("{name}.stops"));
        let capabilities = serde_json::json!({
            "id": 1,
            "result": {"protocolVersion": 1, "control": {
                "available": true, "provider": "core-graphics-v1",
                "requiresAccessibility": true, "windowControlAvailable": false
            }}
        });
        let helper = directory.join(format!("{name}.sh"));
        let script = format!(
            "#!/bin/sh\nread first\nprintf '%s\\n' '{capabilities}'\nread second\nsid=$(printf '%s' \"$second\" | sed -n 's/.*\"sessionID\":\"\\([^\"]*\\)\".*/\\1/p')\nprintf '{{\"id\":2,\"result\":{{\"status\":{{\"state\":\"{capture_state}\",\"reason\":\"first_failure\",\"sessionID\":\"%s\",\"generation\":1,\"sourceID\":null,\"geometryRevision\":0}}}}}}\\n' \"$sid\"\nread third\nprintf '{{\"id\":3,\"result\":{{\"status\":{{\"state\":\"{capture_state}\",\"reason\":\"second_failure\",\"sessionID\":\"%s\",\"generation\":1,\"sourceID\":null,\"geometryRevision\":0}}}}}}\\n' \"$sid\"\nwhile IFS= read -r line; do case \"$line\" in *stop*) echo \"$sid\" >> '{}'; exit 0;; esac; done\n",
            stops.display(),
        );
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        helper
    }

    async fn open_view(
        state: &AppState,
        device: &str,
        request_id: &str,
        extra: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut value = serde_json::json!({
            "clientRequestId": request_id,
            "conversationId": wonder_store::HOST_VIEW_CONVERSATION_ID,
            "hostInstallationId": state.host_installation_id,
        });
        if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            value.extend(extra.clone());
        }
        crate::test_support::assert_http_contract("startComputerSessionRequest", &value);
        let response = create(
            State(state.clone()),
            Some(Extension(AuthenticatedDevice {
                device_id: device.into(),
                session_binding: "fixture".into(),
            })),
            Json(serde_json::from_value(value).unwrap()),
        )
        .await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn session_state(state: &AppState, id: &str) -> String {
        state
            .store
            .computer_session(id)
            .await
            .unwrap()
            .unwrap()
            .state
    }

    async fn active_helper(state: &AppState) -> Option<String> {
        state
            .computer_supervisor
            .active
            .lock()
            .await
            .as_ref()
            .map(|managed| managed.session_id.clone())
    }

    async fn wait_until_control_capable(state: &AppState) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !state.computer_supervisor.control_capable() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn host_replaces_its_own_view_and_offers_take_over_to_another_device() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        state.computer_use_enabled = true;
        state.computer_use_bin = Some(viewer_helper(directory.path(), "viewer", "awaitingSource"));
        for (id, label) in [("phone", "iPhone"), ("pad", "iPad"), ("old", "Old iPad")] {
            state
                .store
                .upsert_owner_device(id, label, "{}", "now")
                .await
                .unwrap();
        }
        state.store.revoke_owner_device("old", "now").await.unwrap();

        let (status, first) = open_view(&state, "phone", "open-1", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{first}");
        let first_id = first["id"].as_str().unwrap().to_owned();

        // App death or a lost close: the same phone reopening replaces its own
        // view at once instead of waiting out the old one.
        let (status, second) = open_view(&state, "phone", "open-2", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{second}");
        let second_id = second["id"].as_str().unwrap().to_owned();
        assert_eq!(session_state(&state, &first_id).await, "ended");
        assert_eq!(
            active_helper(&state).await.as_deref(),
            Some(second_id.as_str())
        );
        // Only the replaced view's helper is stopped. It records the stop
        // when it exits within `stop_child`'s grace; on a loaded machine it
        // may be killed first, which ends it as surely but records nothing.
        let stops =
            std::fs::read_to_string(directory.path().join("viewer.stops")).unwrap_or_default();
        assert!(stops.lines().all(|stopped| stopped == first_id), "{stops}");

        // Another device is told who is viewing and is never silently switched.
        let (status, busy) = open_view(&state, "pad", "pad-1", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        crate::test_support::assert_http_contract("computerViewerConflict", &busy);
        assert_eq!(busy["code"], "viewerBusy");
        assert_eq!(busy["deviceName"], "iPhone");
        assert_eq!(busy["message"], "This computer is being viewed on iPhone.");
        assert_eq!(busy["sessionId"], second_id.as_str());
        assert_ne!(session_state(&state, &second_id).await, "ended");

        // Take over names the view it saw; a view that changed since is refused.
        let (status, _) = open_view(
            &state,
            "pad",
            "pad-2",
            serde_json::json!({"takeOver": true}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, changed) = open_view(
            &state,
            "pad",
            "pad-3",
            serde_json::json!({"takeOver": true, "replaces": {"sessionId": first_id, "generation": 1}}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        crate::test_support::assert_http_contract("computerViewerConflict", &changed);
        assert_eq!(changed["code"], "viewChanged");
        assert_eq!(changed["sessionId"], second_id.as_str());

        wait_until_control_capable(&state).await;
        let (status, taken) = open_view(
            &state,
            "pad",
            "pad-4",
            serde_json::json!({"takeOver": true, "replaces": {"sessionId": second_id, "generation": busy["generation"]}}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{taken}");
        let taken_id = taken["id"].as_str().unwrap().to_owned();
        assert_eq!(session_state(&state, &second_id).await, "ended");
        assert_eq!(
            active_helper(&state).await.as_deref(),
            Some(taken_id.as_str())
        );

        // The phone's late close of its replaced view leaves the iPad's control on.
        wait_until_control_capable(&state).await;
        let response = end(
            State(state.clone()),
            Some(Extension(AuthenticatedDevice {
                device_id: "phone".into(),
                session_binding: "fixture".into(),
            })),
            Path(second_id.clone()),
            Query(GenerationQuery {
                generation: Some(1),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(state.computer_supervisor.control_capable());
        assert_eq!(
            active_helper(&state).await.as_deref(),
            Some(taken_id.as_str())
        );
        assert_ne!(session_state(&state, &taken_id).await, "ended");

        // A revoked device cannot end someone else's view.
        let (status, _) = open_view(
            &state,
            "old",
            "old-1",
            serde_json::json!({"takeOver": true, "replaces": {"sessionId": taken_id, "generation": 1}}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_ne!(session_state(&state, &taken_id).await, "ended");

        // Simultaneous replacements of one view: one wins, the other is told
        // the view changed, and one helper serves the one active row.
        let replaces = serde_json::json!({"replaces": {"sessionId": taken_id, "generation": 1}});
        let (left, right) = tokio::join!(
            open_view(&state, "pad", "pad-5", replaces.clone()),
            open_view(&state, "pad", "pad-6", replaces.clone()),
        );
        let mut statuses = [left.0, right.0];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::CREATED, StatusCode::CONFLICT]);
        let (winner, loser) = if left.0 == StatusCode::CREATED {
            (left.1, right.1)
        } else {
            (right.1, left.1)
        };
        assert_eq!(loser["code"], "viewChanged");
        let active = state
            .store
            .active_computer_sessions_for_host(&state.host_installation_id)
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, winner["id"].as_str().unwrap());
        assert_eq!(
            active_helper(&state).await.as_deref(),
            Some(active[0].id.as_str())
        );

        state.computer_supervisor.shutdown(&state).await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn take_over_is_decided_again_when_it_reaches_the_host() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        state.computer_use_enabled = true;
        state.computer_use_bin = Some(viewer_helper(directory.path(), "viewer", "awaitingSource"));
        for (id, label) in [("phone", "iPhone"), ("pad", "iPad")] {
            state
                .store
                .upsert_owner_device(id, label, "{}", "now")
                .await
                .unwrap();
        }
        let phone = || {
            Some(Extension(AuthenticatedDevice {
                device_id: "phone".into(),
                session_binding: "fixture".into(),
            }))
        };

        // A take over of a view that has since closed is stale: the iPad is
        // told the view changed rather than opening on a decision about it.
        let (status, first) = open_view(&state, "phone", "open-1", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{first}");
        let first_id = first["id"].as_str().unwrap().to_owned();
        let response = end(
            State(state.clone()),
            phone(),
            Path(first_id.clone()),
            Query(GenerationQuery {
                generation: Some(1),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let stale = serde_json::json!({"takeOver": true, "replaces": {"sessionId": first_id, "generation": 1}});
        let (status, changed) = open_view(&state, "pad", "pad-1", stale).await;
        assert_eq!(status, StatusCode::CONFLICT, "{changed}");
        crate::test_support::assert_http_contract("computerViewerConflict", &changed);
        assert_eq!(changed["code"], "viewChanged");
        assert_eq!(changed["sessionId"], first_id.as_str());
        // The phone reopening its own closed view just opens a new one.
        let own = serde_json::json!({"replaces": {"sessionId": first_id, "generation": 1}});
        let (status, second) = open_view(&state, "phone", "open-2", own).await;
        assert_eq!(status, StatusCode::CREATED, "{second}");
        let second_id = second["id"].as_str().unwrap().to_owned();

        // A device revoked while its take over waits for the host is refused
        // and leaves the phone's view alone.
        let lifecycle = state.computer_supervisor.lifecycle.lock().await;
        let queued = tokio::spawn({
            let state = state.clone();
            let replaces = serde_json::json!({"takeOver": true, "replaces": {"sessionId": second_id, "generation": 1}});
            async move { open_view(&state, "pad", "pad-2", replaces).await }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while state
                .computer_supervisor
                .session_control(&second_id)
                .try_lock()
                .is_ok()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("take over reaches the lifecycle owner");
        state.store.revoke_owner_device("pad", "now").await.unwrap();
        drop(lifecycle);
        let (status, _) = queued.await.unwrap();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_ne!(session_state(&state, &second_id).await, "ended");
        assert_eq!(
            active_helper(&state).await.as_deref(),
            Some(second_id.as_str())
        );

        state.computer_supervisor.shutdown(&state).await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_capture_retires_its_helper_at_once_and_keeps_the_first_reason() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        state.computer_use_enabled = true;
        state.computer_use_bin = Some(viewer_helper(
            directory.path(),
            "failing",
            "permissionDenied",
        ));
        state
            .store
            .upsert_owner_device("phone", "iPhone", "{}", "now")
            .await
            .unwrap();
        let (status, failed) = open_view(&state, "phone", "open-1", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{failed}");
        let failed_id = failed["id"].as_str().unwrap().to_owned();
        // Well inside the 30-second viewer lease that used to hold the slot.
        tokio::time::timeout(Duration::from_secs(10), async {
            while active_helper(&state).await.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let current = state
            .store
            .computer_session(&failed_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.state, "failed");
        assert_eq!(current.failure_reason.as_deref(), Some("first_failure"));

        state.computer_use_bin = Some(viewer_helper(directory.path(), "retry", "awaitingSource"));
        let (status, retry) = open_view(&state, "phone", "open-2", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{retry}");
        assert_eq!(active_helper(&state).await.as_deref(), retry["id"].as_str());

        // The failed view's late close ends only that view: the retry keeps control.
        wait_until_control_capable(&state).await;
        let response = end(
            State(state.clone()),
            Some(Extension(AuthenticatedDevice {
                device_id: "phone".into(),
                session_binding: "fixture".into(),
            })),
            Path(failed_id.clone()),
            Query(GenerationQuery {
                generation: Some(1),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(session_state(&state, &failed_id).await, "ended");
        assert!(state.computer_supervisor.control_capable());
        assert_eq!(active_helper(&state).await.as_deref(), retry["id"].as_str());
        state.computer_supervisor.shutdown(&state).await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_stale_view_cannot_change_the_live_views_capabilities() {
        let supervisor = ComputerSessionSupervisor::default();
        let capable = serde_json::json!({"id": 1, "result": {"protocolVersion": 1, "control": {
            "available": true, "provider": "core-graphics-v1", "requiresAccessibility": true
        }}});
        let unavailable = serde_json::json!({"id": 1, "result": {"protocolVersion": 1, "control": {
            "available": false, "provider": "core-graphics-v1", "requiresAccessibility": true
        }}});
        supervisor.claim_capabilities("live");
        supervisor.set_session_capabilities("live", &capable);
        supervisor.set_session_capabilities("old", &unavailable);
        supervisor.release_capabilities("old");
        supervisor.clear_if("old").await;
        assert!(supervisor.control_capable());
        supervisor.release_capabilities("live");
        assert!(!supervisor.control_capable());
    }

    #[tokio::test]
    async fn provider_availability_requires_enabled_executable() {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        let helper = directory.path().join("provider");
        std::fs::write(&helper, "#!/bin/sh\n").unwrap();
        state.computer_use_bin = Some(helper.clone());
        state.computer_use_enabled = true;
        assert!(!provider_available(&state));
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(provider_available(&state));
        state.computer_use_enabled = false;
        assert!(!provider_available(&state));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn helper_output_framing_delivers_without_eof_and_rejects_oversize() {
        let (mut writer, reader) = tokio::io::duplex(256);
        let mut reader = BufReader::new(reader);
        let write = tokio::spawn(async move {
            writer.write_all(b"first\nsecond\n").await.unwrap();
            // Hold the writer open, so a read that waited for EOF never ends.
            std::future::pending::<()>().await;
        });
        let mut buffer = Vec::new();
        let first = timeout(
            Duration::from_secs(60),
            read_bounded_line(&mut reader, &mut buffer),
        )
        .await
        .expect("a complete helper line must not wait for EOF")
        .unwrap();
        assert_eq!(first, Some(b"first".to_vec()));
        assert_eq!(
            read_bounded_line(&mut reader, &mut buffer).await.unwrap(),
            Some(b"second".to_vec())
        );
        write.abort();

        let oversized = vec![b'x'; MAX_HELPER_LINE_BYTES + 1];
        let mut oversized_reader = BufReader::new(oversized.as_slice());
        let error = read_bounded_line(&mut oversized_reader, &mut buffer)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        // A supervisor branch can cancel the read after consuming bytes.
        // Resuming must keep those bytes, including during the exit drain.
        let (mut writer, stream) = tokio::io::duplex(256);
        let mut reader = BufReader::new(stream);
        writer.write_all(b"last failure").await.unwrap();
        {
            let read = read_bounded_line(&mut reader, &mut buffer);
            tokio::pin!(read);
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                    read.as_mut(),
                    cx
                )))
                .await
                .is_pending()
            );
        }
        assert_eq!(buffer, b"last failure");
        writer.write_all(b" reason\n").await.unwrap();
        assert_eq!(
            read_bounded_line(&mut reader, &mut buffer).await.unwrap(),
            Some(b"last failure reason".to_vec())
        );
        assert!(buffer.is_empty());

        // Only the session helper's output may carry a fully escaped shared
        // clipboard; the dialog watcher and discovery keep the default bound.
        let mut clipboard = serde_json::to_vec(&serde_json::json!({"id": 9, "result": {
            "clipboard": {"status": "watching", "text": "\u{1}".repeat(MAX_SHARED_CLIPBOARD_BYTES)}
        }}))
        .unwrap();
        clipboard.push(b'\n');
        assert!(clipboard.len() > MAX_SHARED_CLIPBOARD_BYTES * 6);
        let error = read_bounded_line(&mut BufReader::new(clipboard.as_slice()), &mut buffer)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let line = read_line_within(
            &mut BufReader::new(clipboard.as_slice()),
            &mut buffer,
            MAX_SESSION_HELPER_LINE_BYTES,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(line.len() + 1, clipboard.len());
        let oversized = vec![b'x'; MAX_SESSION_HELPER_LINE_BYTES + 1];
        let error = read_line_within(
            &mut BufReader::new(oversized.as_slice()),
            &mut buffer,
            MAX_SESSION_HELPER_LINE_BYTES,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn helper_signaling_events_reject_malformed_or_unknown_and_ignore_stale_peers() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let session = stored_session();
        let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
        signaling.lock().await.peer_revision = Some("current-peer".into());

        let missing_revision = serde_json::json!({
            "event": "signal.offer",
            "sessionID": session.id,
            "generation": session.generation,
            "type": "offer",
            "sdp": "v=0\na=sendonly"
        });
        assert_eq!(
            apply_helper_value_with_signaling(&state, &session, &missing_revision, &signaling)
                .await
                .unwrap(),
            HelperValueAction::ProtocolError
        );

        let unknown = serde_json::json!({
            "event": "signal.unrecognized",
            "sessionID": session.id,
            "generation": session.generation,
            "peerRevision": "current-peer"
        });
        assert_eq!(
            apply_helper_value_with_signaling(&state, &session, &unknown, &signaling)
                .await
                .unwrap(),
            HelperValueAction::ProtocolError
        );

        let stale = serde_json::json!({
            "event": "signal.offer",
            "sessionID": session.id,
            "generation": session.generation,
            "peerRevision": "stale-peer",
            "type": "offer",
            "sdp": "v=0\na=sendonly"
        });
        assert_eq!(
            apply_helper_value_with_signaling(&state, &session, &stale, &signaling)
                .await
                .unwrap(),
            HelperValueAction::None
        );
        assert!(signaling.lock().await.offer.is_none());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    fn candidate_priority(candidate: &str) -> u32 {
        candidate.split(' ').nth(3).unwrap().parse().unwrap()
    }

    /// Candidates as gathered on a Mac (libwebrtc M153) and by aiortc, which
    /// gives every host candidate one priority.
    #[test]
    fn tailscale_candidates_rank_below_every_home_network_candidate() {
        let home = [
            "candidate:2192625468 1 udp 2122129151 192.168.1.102 50290 typ host generation 0 ufrag QnII network-id 3 network-cost 50",
            "candidate:1908505262 1 udp 2122265343 fdc9:fcb3:e634:848d:ca5:264:7a:b690 63906 typ host generation 0 ufrag QnII network-id 2 network-cost 50",
            "candidate:5 1 udp 2130706431 10.0.0.8 41000 typ host",
        ];
        let tailscale = [
            "candidate:3603418031 1 udp 2121998079 100.102.158.89 52869 typ host generation 0 ufrag QnII network-id 6 network-cost 50",
            "candidate:1359260457 1 udp 2122068735 fd7a:115c:a1e0::f32c:9e5a 50412 typ host generation 0 ufrag QnII network-id 7 network-cost 50",
            "candidate:6 2 udp 2130706430 100.64.0.9 41001 typ host",
        ];
        for candidate in home {
            assert_eq!(tailscale_last(candidate), candidate);
        }
        let lowest_home = home.iter().map(|c| candidate_priority(c)).min().unwrap();
        for candidate in tailscale {
            let lowered = tailscale_last(candidate);
            let priority = candidate_priority(&lowered);
            assert!(priority < lowest_home, "{lowered}");
            // Same type preference (host) and component; only the local preference drops.
            assert_eq!(priority >> 24, candidate_priority(candidate) >> 24);
            assert_eq!(priority & 0xFF, candidate_priority(candidate) & 0xFF);
            let unchanged = |c: &str| {
                c.split(' ')
                    .enumerate()
                    .filter(|(i, _)| *i != 3)
                    .map(|(_, f)| f.to_owned())
                    .collect::<Vec<_>>()
            };
            assert_eq!(unchanged(&lowered), unchanged(candidate));
            assert_eq!(tailscale_last(&lowered), lowered, "lowering is idempotent");
        }
        for untouched in [
            "candidate:7 1 udp 2122129151 3f1c2a9e-1.local 50290 typ host",
            "candidate:8 1 udp notanumber 100.102.158.89 1 typ host",
            "candidate:9 1 udp 2122129151",
        ] {
            assert!(matches!(tailscale_last(untouched), Cow::Borrowed(_)));
        }

        let offer = format!(
            "v=0\r\na=group:BUNDLE 0\r\na={}\r\na={}\r\na=end-of-candidates\r\n",
            home[2], tailscale[0]
        );
        let lowered = tailscale_last_in_sdp(&offer);
        assert_eq!(
            lowered,
            format!(
                "v=0\r\na=group:BUNDLE 0\r\na={}\r\na={}\r\na=end-of-candidates\r\n",
                home[2],
                tailscale_last(tailscale[0])
            )
        );
        let lf_only = offer.replace("\r\n", "\n");
        assert_eq!(
            tailscale_last_in_sdp(&lf_only),
            lowered.replace("\r\n", "\n")
        );
        assert!(matches!(
            tailscale_last_in_sdp("v=0\na=sendonly"),
            Cow::Borrowed(_)
        ));
    }

    #[tokio::test]
    async fn relayed_candidates_offer_the_home_network_before_tailscale() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let session = stored_session();
        let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
        signaling.lock().await.peer_revision = Some("peer".into());
        let home = "candidate:1 1 udp 2130706431 192.168.1.102 50290 typ host";
        let tailscale = "candidate:2 1 udp 2130706431 100.102.158.89 52869 typ host";
        let event = |name: &str, extra: serde_json::Value| {
            let mut value = serde_json::json!({
                "event": name,
                "sessionID": session.id,
                "generation": session.generation,
                "peerRevision": "peer",
            });
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            value
        };

        // aiortc puts its candidates in the offer and also sends each one.
        let sdp = format!("v=0\r\na=mid:0\r\na={home}\r\na={tailscale}\r\n");
        let offer = event(
            "signal.offer",
            serde_json::json!({"type": "offer", "sdp": sdp}),
        );
        for _ in 0..2 {
            assert_eq!(
                apply_helper_value_with_signaling(&state, &session, &offer, &signaling)
                    .await
                    .unwrap(),
                HelperValueAction::None,
                "a repeated offer is the same offer after lowering"
            );
        }
        for candidate in [home, tailscale, tailscale] {
            let value = event(
                "signal.candidate",
                serde_json::json!({"candidate": candidate, "sdpMid": "0", "sdpMLineIndex": 0}),
            );
            assert_eq!(
                apply_helper_value_with_signaling(&state, &session, &value, &signaling)
                    .await
                    .unwrap(),
                HelperValueAction::None
            );
        }
        let snapshot = signaling_snapshot(
            &session.id,
            session.generation,
            None,
            &*signaling.lock().await,
            0,
        );
        let offered = snapshot.offer.unwrap().sdp;
        assert!(offered.contains(&format!("a={home}\r\n")));
        assert!(offered.contains(&format!("a={}\r\n", tailscale_last(tailscale))));
        let relayed: Vec<_> = snapshot
            .candidates
            .iter()
            .map(|c| c.candidate.as_str())
            .collect();
        assert_eq!(relayed, [home, tailscale_last(tailscale).as_ref()]);
        assert!(candidate_priority(relayed[1]) < candidate_priority(relayed[0]));

        // The phone's own candidates reach the helper the same way.
        let request = |candidate: &str| SignalingCandidateRequest {
            binding: SignalingBinding {
                host_installation_id: session.host_installation_id.clone(),
                conversation_id: session.conversation_id.clone(),
                generation: session.generation,
                peer_revision: Some("peer".into()),
            },
            sequence: 1,
            candidate: candidate.into(),
            sdp_mid: Some("0".into()),
            sdp_m_line_index: 0,
            username_fragment: None,
        };
        let phone_home =
            "candidate:3 1 udp 2122260223 192.168.1.40 60000 typ host generation 0 network-cost 10";
        let phone_tailscale =
            "candidate:4 1 udp 2122194687 100.80.1.2 60001 typ host generation 0 network-cost 50";
        assert_eq!(
            forwarded_candidate(&request(phone_home)).candidate,
            phone_home
        );
        let forwarded = forwarded_candidate(&request(phone_tailscale));
        assert_eq!(forwarded.candidate, tailscale_last(phone_tailscale));
        assert!(candidate_priority(&forwarded.candidate) < candidate_priority(phone_home));
        let mut phone = SignalingState::default();
        assert_eq!(phone.reserve_candidate(forwarded), Ok(true));
        phone.commit_candidate(1);
        assert_eq!(
            phone.reserve_candidate(forwarded_candidate(&request(phone_tailscale))),
            Ok(false),
            "a retried phone candidate matches what the helper received"
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn helper_control_revocation_requires_the_complete_session_bound_identity() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let session = stored_session();
        let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState::default()));
        let event = serde_json::json!({
            "event": "control.revoked",
            "sessionID": session.id,
            "generation": session.generation,
            "leaseID": "lease",
            "requestID": "request",
            "geometryRevision": 7,
            "sourceID": "display:1",
            "reason": "local_stop"
        });
        assert_eq!(
            apply_helper_value_with_signaling(&state, &session, &event, &signaling)
                .await
                .unwrap(),
            HelperValueAction::ControlRevoked {
                lease_id: "lease".into(),
                request_id: "request".into(),
                generation: session.generation,
                geometry_revision: 7,
                source_id: Some("display:1".into()),
                reason: "local_stop".into(),
            }
        );

        let mut malformed = event;
        malformed.as_object_mut().unwrap().remove("requestID");
        assert_eq!(
            apply_helper_value_with_signaling(&state, &session, &malformed, &signaling)
                .await
                .unwrap(),
            HelperValueAction::ProtocolError
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    async fn durable(state: &AppState, id: &str) -> StoredComputerSession {
        state.store.computer_session(id).await.unwrap().unwrap()
    }

    fn peer_state_event(session: &StoredComputerSession, peer_state: &str) -> serde_json::Value {
        serde_json::json!({
            "event": "signal.peerState",
            "sessionID": session.id,
            "generation": session.generation,
            "peerRevision": "peer",
            "state": peer_state,
        })
    }

    // A Wi-Fi roam reports `disconnected`; the view must pause, not end, and
    // come back live when the connection does. `failed` still ends it at once.
    #[tokio::test]
    async fn a_dropped_connection_pauses_the_view_until_it_comes_back() {
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        let mut create = session();
        create.state = ComputerSessionState::Live;
        let stored = state.store.insert_computer_session(&create).await.unwrap();
        let signaling = Arc::new(tokio::sync::Mutex::new(SignalingState {
            peer_revision: Some("peer".into()),
            peer_state: "connected".into(),
            capture_state: "capturing".into(),
            first_frame_observed: true,
            ..SignalingState::default()
        }));

        for round in 0..2 {
            assert_eq!(
                apply_helper_value_with_signaling(
                    &state,
                    &stored,
                    &peer_state_event(&stored, "disconnected"),
                    &signaling
                )
                .await
                .unwrap(),
                HelperValueAction::None,
                "round {round}: a drop must not stop capture"
            );
            let paused = durable(&state, &stored.id).await;
            // Input and control require a live view, so they wait too.
            assert_eq!(paused.state, "preparing", "round {round}");
            assert_eq!(paused.failure_reason, None, "round {round}");
            {
                let current = signaling.lock().await;
                assert_eq!(current.public_state(), "reconnecting", "round {round}");
                assert!(current.peer_interrupted_since.is_some());
            }
            // Signaling keeps answering the phone while it reconnects.
            let binding = SignalingBinding {
                host_installation_id: paused.host_installation_id.clone(),
                conversation_id: paused.conversation_id.clone(),
                generation: paused.generation,
                peer_revision: None,
            };
            assert!(signal_binding_error(&paused, &binding).is_none());

            let first = signaling.lock().await.peer_interrupted_since;
            apply_helper_value_with_signaling(
                &state,
                &stored,
                &peer_state_event(&stored, "checking"),
                &signaling,
            )
            .await
            .unwrap();
            // Still checking: the grace keeps counting from the first drop.
            assert_eq!(signaling.lock().await.peer_interrupted_since, first);

            apply_helper_value_with_signaling(
                &state,
                &stored,
                &peer_state_event(&stored, "connected"),
                &signaling,
            )
            .await
            .unwrap();
            assert_eq!(
                durable(&state, &stored.id).await.state,
                "live",
                "round {round}"
            );
            assert!(signaling.lock().await.peer_interrupted_since.is_none());
            assert_eq!(signaling.lock().await.public_state(), "live");
        }

        assert_eq!(
            apply_helper_value_with_signaling(
                &state,
                &stored,
                &peer_state_event(&stored, "failed"),
                &signaling
            )
            .await
            .unwrap(),
            HelperValueAction::StopCapture {
                reason: "peer_connection_terminal".into()
            }
        );
        let failed = durable(&state, &stored.id).await;
        assert_eq!(failed.state, "failed");
        assert_eq!(
            failed.failure_reason.as_deref(),
            Some("peer_connection_failed")
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn the_recovery_timer_follows_the_first_drop_and_disarms_on_return() {
        tokio::time::pause();
        let grace = Duration::from_secs(15);
        let mut recovery = PeerRecoveryTimer::with_grace(grace);
        assert!(!recovery.armed());
        let dropped = tokio::time::Instant::now();
        recovery.sync(Some(dropped));
        assert!(recovery.armed());
        assert_eq!(recovery.timer.deadline(), dropped + grace);
        tokio::time::advance(Duration::from_secs(10)).await;
        // A later report of the same drop does not restart the grace.
        recovery.sync(Some(dropped));
        assert_eq!(recovery.timer.deadline(), dropped + grace);
        recovery.sync(None);
        assert!(!recovery.armed());
        // A second drop gets a full grace of its own.
        let again = tokio::time::Instant::now();
        recovery.sync(Some(again));
        assert_eq!(recovery.timer.deadline(), again + grace);
    }

    /// Runs a fake helper whose connection drops once the view is set up.
    /// With `recovers`, the helper reconnects when told to let go of input.
    async fn dropped_connection_fixture(recovers: bool) {
        let (directory, mut state) = crate::ingestion::tests::fixture().await;
        let helper_log = directory.path().join("computer-helper.jsonl");
        let capabilities = serde_json::json!({"id": 1, "result": {"protocolVersion": 1}});
        let status = |id: u64| {
            serde_json::json!({"id": id, "result": {"status": {
                "state": "awaitingSource",
                "sessionID": "fake-session",
                "generation": 1,
                "sourceID": null,
                "geometryRevision": 0
            }}})
        };
        let peer = |event: &str, peer_state: Option<&str>| {
            let mut value = serde_json::json!({
                "event": event,
                "sessionID": "fake-session",
                "generation": 1,
                "peerRevision": "peer",
            });
            if let Some(peer_state) = peer_state {
                value["state"] = peer_state.into();
            }
            value
        };
        let on_release = if recovers {
            format!(
                "printf '%s\\n' '{}'",
                peer("signal.peerState", Some("connected"))
            )
        } else {
            ":".into()
        };
        let script = format!(
            "#!/bin/sh\nread first\nprintf '%s\\n' '{capabilities}'\nread second\nprintf '%s\\n' '{}'\nread third\nprintf '%s\\n' '{}'\nprintf '%s\\n' '{}'\nprintf '%s\\n' '{}'\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> '{log}'; case \"$line\" in *releaseAll*) {on_release};; esac; case \"$line\" in *stop*) exit 0;; esac; done\n",
            status(2),
            status(3),
            peer("signal.peerReady", None),
            peer("signal.peerState", Some("disconnected")),
            log = helper_log.display(),
        );
        let helper = directory.path().join("fake-computer-helper-drop.sh");
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        state.computer_use_enabled = true;
        state.computer_use_bin = Some(helper);
        // Control is granted only on a live view.
        let mut create = session();
        create.state = ComputerSessionState::Live;
        let stored = state.store.insert_computer_session(&create).await.unwrap();
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let lease = ComputerControlLeaseCreate {
            id: "drop-lease".into(),
            client_request_id: "drop-request".into(),
            session_id: stored.id.clone(),
            owner_device_id: stored.owner_device_id.clone(),
            host_installation_id: stored.host_installation_id.clone(),
            conversation_id: stored.conversation_id.clone(),
            session_generation: stored.generation,
            source_id: None,
            geometry_revision: 0,
            now,
            expires_at: (Utc::now() + chrono::Duration::seconds(120))
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        };
        assert!(matches!(
            state
                .store
                .acquire_computer_control_lease(&lease)
                .await
                .unwrap(),
            ComputerLeaseAcquireResult::Granted(_)
        ));
        let supervisor = Arc::clone(&state.computer_supervisor);
        let log = || std::fs::read_to_string(&helper_log).unwrap_or_default();
        // Time stands still unless advanced, so only the recovery grace moves.
        tokio::time::pause();
        let signaling = super::tests::observe_helper(async {
            supervisor
                .start(state.clone(), stored.clone())
                .await
                .unwrap();
            supervisor
                .signaling_handle(&stored.id, &stored.owner_device_id)
                .await
                .unwrap()
                .signaling
        })
        .await;
        super::tests::observe_helper(async {
            loop {
                let released = log().contains("control.releaseAll");
                let reconnected = signaling.lock().await.peer_state == "connected";
                if released && (!recovers || reconnected) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        // Store reads stay inside the observer: an idle paused clock would
        // otherwise jump ahead and time out the database pool.
        let lease_status = super::tests::observe_helper(async {
            let lease = state.store.computer_control_lease(&lease.id).await;
            lease.unwrap().unwrap().status
        })
        .await;
        // The drop ends control at once instead of waiting for the lease.
        assert_eq!(lease_status, "released");
        let settled_state = || async {
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
            durable(&state, &stored.id).await.state
        };
        tokio::time::advance(PEER_RECOVERY_GRACE - Duration::from_secs(1)).await;
        let inside_grace = super::tests::observe_helper(settled_state()).await;
        assert!(
            !log().contains("capture.stop"),
            "the view ended inside the grace"
        );
        assert_ne!(inside_grace, "failed");
        tokio::time::advance(Duration::from_secs(2)).await;
        if recovers {
            let after_grace = super::tests::observe_helper(settled_state()).await;
            assert!(!log().contains("capture.stop"), "a recovered view ended");
            assert_ne!(after_grace, "failed");
            assert!(supervisor.active.lock().await.is_some());
            tokio::time::resume();
            supervisor.stop(&state, &stored.id, "test_done").await;
        } else {
            let failed = super::tests::observe_helper(async {
                while !(log().contains("capture.stop") && supervisor.active.lock().await.is_none())
                {
                    tokio::task::yield_now().await;
                }
                durable(&state, &stored.id).await
            })
            .await;
            tokio::time::resume();
            assert_eq!(failed.state, "failed");
            assert_eq!(
                failed.failure_reason.as_deref(),
                Some("peer_connection_disconnected")
            );
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_connection_that_stays_down_ends_the_view_after_the_grace() {
        dropped_connection_fixture(false).await;
    }

    #[tokio::test]
    async fn a_connection_that_comes_back_keeps_the_view() {
        dropped_connection_fixture(true).await;
    }
}
