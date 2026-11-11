//! Unix (Linux) operating-system layer of the headless supervisor.
use super::{Paths, Result, HOST_RUNNING, STOPPING};
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    os::unix::net::{UnixListener, UnixStream},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

pub(super) fn default_data_dir() -> Result<PathBuf> {
    Ok(wonderd::data_home::default_dir(Path::new(&std::env::var(
        "HOME",
    )?)))
}

pub(super) fn restrict_dir(path: &Path) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub(super) fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    // `mode` applies only when the file is created; a temp file left behind
    // by an interrupted write keeps whatever mode it had.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Creates a private file without replacing an existing destination or sibling.
pub(super) fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

pub(super) fn append_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

// macOS runs its helper from the signed bundle (macos.rs).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(super) fn make_executable(path: &Path) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub(super) struct SupervisorLock(std::fs::File);

impl Drop for SupervisorLock {
    fn drop(&mut self) {
        // Explicitly release before close: a concurrent fork can temporarily
        // inherit the descriptor until exec closes it.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(super) fn supervisor_lock(paths: &Paths) -> Result<SupervisorLock> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(paths.lock())?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(HOST_RUNNING.into());
    }
    Ok(SupervisorLock(file))
}

/// The private stop channel: a Unix socket inside the 0700 state directory.
pub(super) struct ControlListener {
    listener: UnixListener,
    path: PathBuf,
}

impl ControlListener {
    pub(super) fn bind(paths: &Paths) -> Result<Self> {
        let path = paths.state.join("control.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener, path })
    }

    /// True once a stop was requested by the channel or a signal.
    pub(super) fn stopping(&self) -> bool {
        if self.stop_requested() {
            STOPPING.store(true, Ordering::SeqCst);
        }
        STOPPING.load(Ordering::SeqCst)
    }

    /// Whether a waiting connection asked to stop. The request may arrive a
    /// moment after the connection does.
    fn stop_requested(&self) -> bool {
        let Ok((mut stream, _)) = self.listener.accept() else {
            return false;
        };
        let mut input = [0u8; 5];
        read_exact_within(&mut stream, &mut input, Duration::from_millis(100)).is_ok()
            && input == *b"stop\n"
    }
}

/// Fills `buffer` from `stream` within `timeout`: `UnexpectedEof` when the
/// peer closes first, `TimedOut` when it is too slow. Darwin hands an accepted
/// socket the listener's O_NONBLOCK, where SO_RCVTIMEO does not apply, and
/// clearing it after a short-lived peer exits can fail, so this polls.
pub(super) fn read_exact_within(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    timeout: Duration,
) -> std::io::Result<()> {
    use std::io::ErrorKind;
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    let mut count = 0;
    while count < buffer.len() {
        match stream.read(&mut buffer[count..]) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(bytes) => count += bytes,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(ErrorKind::TimedOut.into());
                }
                let mut poll = libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut poll, 1, remaining.as_millis().max(1) as i32) } < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl Drop for ControlListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(super) fn send_stop(paths: &Paths) -> Result<()> {
    let mut stream = UnixStream::connect(paths.state.join("control.sock"))
        .map_err(|_| "host is starting; retry stop in a moment")?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"stop\n")?;
    Ok(())
}

extern "C" fn on_signal(_: libc::c_int) {
    STOPPING.store(true, Ordering::SeqCst);
}

pub(super) fn install_stop_handlers() {
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT] {
            libc::signal(signal, on_signal as *const () as libc::sighandler_t);
        }
    }
}

/// `pair` also stops when its terminal or SSH session hangs up, so it can
/// withdraw its offer.
pub(super) fn install_pair_stop_handlers() {
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(signal, on_signal as *const () as libc::sighandler_t);
        }
    }
}

/// Starts `wonder start --foreground` detached from this terminal.
/// macOS starts it through LaunchServices instead (macos.rs).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(super) fn spawn_background(log: std::fs::File) -> Result<u32> {
    let child = Command::new(std::env::current_exe()?)
        .args(["start", "--foreground"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    Ok(child.id())
}

/// This machine's host name, for the X authority record.
pub(super) fn host_name_bytes() -> Result<Vec<u8>> {
    let mut hostname = [0u8; 256];
    if unsafe { libc::gethostname(hostname.as_mut_ptr().cast(), hostname.len()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let length = hostname
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("hostname too long")?;
    Ok(hostname[..length].to_vec())
}

/// Terminal echo turned off for a typed secret; restored on drop. `None` when
/// stdin is not a terminal (piped input is read as is).
pub(super) struct StdinEcho(libc::termios);

impl StdinEcho {
    pub(super) fn disable() -> Option<Self> {
        let fd = libc::STDIN_FILENO;
        // SAFETY: plain termios calls on stdin with a zeroed, owned struct.
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::isatty(fd) != 1 || libc::tcgetattr(fd, &mut term) != 0 {
                return None;
            }
            let mut quiet = term;
            quiet.c_lflag &= !libc::ECHO;
            libc::tcsetattr(fd, libc::TCSANOW, &quiet);
            Some(Self(term))
        }
    }
}

impl Drop for StdinEcho {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read from stdin.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

pub(super) fn executable_candidates(name: &str) -> Vec<String> {
    vec![name.to_owned()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_that_arrives_after_the_connection_is_still_heard() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            data: directory.path().into(),
            state: directory.path().into(),
        };
        let listener = ControlListener::bind(&paths).unwrap();
        let mut stream = UnixStream::connect(directory.path().join("control.sock")).unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            stream.write_all(b"stop\n").unwrap();
            stream
        });
        assert!(listener.stop_requested());
        drop(writer.join().unwrap());
        assert!(!listener.stop_requested());
    }

    // Darwin's accepted sockets start non-blocking, where a plain timed read
    // returns at once before a late write.
    #[test]
    fn timed_read_waits_for_late_bytes_on_a_non_blocking_socket() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let late = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            writer.write_all(b"st").unwrap();
            std::thread::sleep(Duration::from_millis(30));
            writer.write_all(b"op\n").unwrap();
            writer
        });
        let mut input = [0u8; 5];
        read_exact_within(&mut reader, &mut input, Duration::from_secs(10)).unwrap();
        assert_eq!(&input, b"stop\n");
        let writer = late.join().unwrap();
        // Nothing more is coming: an expired wait reports the timeout.
        let error = read_exact_within(&mut reader, &mut input, Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(writer);
        let error =
            read_exact_within(&mut reader, &mut input, Duration::from_secs(10)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}
