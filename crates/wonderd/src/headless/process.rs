//! Keep the supervisor's children owned until their process groups (Unix) or
//! Job Objects (Windows) are gone.
use super::{log_stamp, Result, STOPPING};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    io::{BufRead, Read, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

#[cfg(unix)]
pub(super) struct OwnedChild {
    child: Child,
    reaped: bool,
    /// Leads a session of its own rather than only a process group.
    session: bool,
}

#[cfg(unix)]
impl OwnedChild {
    pub(super) fn spawn(command: &mut Command) -> Result<Self> {
        Ok(Self {
            child: command.process_group(0).spawn()?,
            reaped: false,
            session: false,
        })
    }

    /// As `spawn`, leading a session of its own. Whatever it starts stays in
    /// that session even in a process group of its own, as provider bridges
    /// are, so cleanup reaches all of it.
    fn spawn_session(command: &mut Command) -> Result<Self> {
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Self {
            child: command.spawn()?,
            reaped: false,
            session: true,
        })
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    /// Do not reap the leader yet: its PID reserves the group identity.
    pub(super) fn exited(&self) -> Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // macOS also reports a stopped (SIGSTOP) child here, which is alive
        // and is the watchdog's to judge: only CLD_EXITED, CLD_KILLED and
        // CLD_DUMPED (1 to 3 on every Unix) are exits.
        Ok(unsafe { info.si_pid() } != 0 && (1..=3).contains(&info.si_code))
    }

    fn signal_group(&self, signal: i32) {
        // The child has never been reaped, including on crash and timeout.
        unsafe { libc::kill(-(self.id() as i32), signal) };
    }

    pub(super) fn terminate(&mut self) {
        self.signal_group(libc::SIGTERM);
    }

    /// SIGTERM to the leader alone. The daemon stops what it started in its
    /// group (Codex included) itself, in order; whatever it leaves when it
    /// exits or its deadline passes is killed on `finish` or drop.
    pub(super) fn terminate_leader(&mut self) {
        // The child has never been reaped, so its PID is still its own.
        unsafe { libc::kill(self.id() as i32, libc::SIGTERM) };
    }

    /// Stops everything left in the group, or in the session for a session
    /// leader. The unreaped leader's PID keeps both identities from reuse.
    fn kill_descendants(&self) {
        self.signal_group(libc::SIGKILL);
        if self.session {
            kill_session(self.id() as i32);
        }
    }

    /// After exit, stop remaining descendants before releasing the group PID.
    pub(super) fn finish(mut self) -> Result<ExitStatus> {
        self.kill_descendants();
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}

#[cfg(unix)]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill_descendants();
            let _ = self.child.wait();
        }
    }
}

/// SIGKILLs every process in the session `leader` started, repeating while
/// members remain so one forking at the time is caught too.
#[cfg(unix)]
fn kill_session(leader: i32) {
    for _ in 0..10 {
        let Ok(pids) = all_pids() else { return };
        let mut found = false;
        for pid in pids {
            if pid != leader && unsafe { libc::getsid(pid) } == leader {
                found = true;
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        if !found {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
fn all_pids() -> std::io::Result<Vec<i32>> {
    let mut pids = vec![0i32; 8192];
    loop {
        // SAFETY: the buffer length is passed in bytes.
        let count = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                (pids.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        if count <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        // A full buffer may have omitted some.
        if (count as usize) < pids.len() || pids.len() >= 1 << 20 {
            pids.truncate(count as usize);
            return Ok(pids);
        }
        pids.resize(pids.len() * 2, 0);
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn all_pids() -> std::io::Result<Vec<i32>> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        if let Some(pid) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        {
            pids.push(pid);
        }
    }
    Ok(pids)
}

impl OwnedChild {
    /// Spawns a host process whose output goes into the host log a line at
    /// a time, each line stamped with the time it arrived.
    pub(super) fn spawn_logged(command: &mut Command) -> Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(Self::relay_output(Self::spawn(command)?))
    }

    /// As `spawn_logged`, for the host daemon: cleanup also stops what it
    /// started outside its own process group (on Windows its Job Object
    /// already holds everything).
    pub(super) fn spawn_daemon(command: &mut Command) -> Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        let owned = Self::spawn_session(command)?;
        #[cfg(windows)]
        let owned = Self::spawn(command)?;
        Ok(Self::relay_output(owned))
    }

    fn relay_output(mut owned: Self) -> Self {
        if let Some(stdout) = owned.child.stdout.take() {
            relay_lines(stdout);
        }
        if let Some(stderr) = owned.child.stderr.take() {
            relay_lines(stderr);
        }
        owned
    }
}

/// Longest piece of one line copied at once; a longer line continues on the
/// next stamped line instead of growing without bound.
const LOG_LINE_LIMIT: u64 = 64 * 1024;

/// Copies until the process and everything it started close the pipe. The
/// thread is never joined, so a descendant holding the pipe cannot delay
/// shutdown.
fn relay_lines(source: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(source);
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut reader)
                .take(LOG_LINE_LIMIT)
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => return,
                Ok(_) => (),
            }
            if !line.ends_with(b"\n") {
                line.push(b'\n');
            }
            let mut stamped = log_stamp().into_bytes();
            stamped.push(b' ');
            stamped.extend_from_slice(&line);
            // One write per line, so lines from both pipes never interleave.
            let _ = std::io::stderr().lock().write_all(&stamped);
        }
    });
}

/// EXPERIMENTAL Windows: the child runs in its own console process group
/// (for CTRL_BREAK) inside a kill-on-close Job Object, which owns everything
/// it starts. The process handle keeps the identity; Windows never reuses a
/// process ID while a handle to it is open.
#[cfg(windows)]
pub(super) struct OwnedChild {
    child: Child,
    job: wonder_app_server::job::Job,
}

#[cfg(windows)]
impl OwnedChild {
    pub(super) fn spawn(command: &mut Command) -> Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        let job = wonder_app_server::job::Job::kill_on_close()?;
        let mut child = command
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP)
            .spawn()?;
        if let Err(error) = job.assign(child.as_raw_handle()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
        Ok(Self { child, job })
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn exited(&self) -> Result<bool> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: the process handle is owned by `self.child`.
        Ok(unsafe { WaitForSingleObject(self.child.as_raw_handle() as _, 0) } == WAIT_OBJECT_0)
    }

    /// CTRL_BREAK to the child's process group (the console analog of
    /// SIGTERM); without a shared console, end the job.
    pub(super) fn terminate(&mut self) {
        use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
        // SAFETY: plain call; the group ID is the child's process ID.
        let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.child.id()) } != 0;
        if !sent {
            let _ = self.job.terminate();
        }
    }

    /// CTRL_BREAK reaches only a whole console group, so this is
    /// `terminate`; children the daemon starts in its group receive it too.
    pub(super) fn terminate_leader(&mut self) {
        self.terminate();
    }

    pub(super) fn finish(mut self) -> Result<ExitStatus> {
        let status = self.child.wait()?;
        let _ = self.job.terminate();
        Ok(status)
    }
}

#[cfg(windows)]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.job.terminate();
        let _ = self.child.wait();
    }
}

pub(super) struct Output {
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

/// Bound setup commands as well as their output; no pipe can block shutdown.
pub(super) fn output(command: &mut Command) -> Result<Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let child = OwnedChild::spawn(command)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !child.exited()? {
        if STOPPING.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return Err("setup command interrupted or timed out after 15 seconds".into());
        }
        if stdout.metadata()?.len() > 1024 * 1024 || stderr.metadata()?.len() > 1024 * 1024 {
            return Err("setup command returned too much output".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = child.finish()?;
    use std::io::{Seek, SeekFrom};
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.take(1024 * 1024 + 1).read_to_end(&mut out)?;
    stderr.take(1024 * 1024 + 1).read_to_end(&mut err)?;
    if out.len() > 1024 * 1024 || err.len() > 1024 * 1024 {
        return Err("setup command returned too much output".into());
    }
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;

    // A host child that crashes may leave its own subprocess running. Its
    // process group must be stopped before the leader's PID can be reused.
    #[test]
    fn crash_cleanup_stops_descendants_without_touching_another_group() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("child.pid");
        let mut script = tempfile::NamedTempFile::new().unwrap();
        writeln!(script, "sleep 60 &\necho $! > \"$1\"\nexit 13").unwrap();
        let sentinel = OwnedChild::spawn(Command::new("sleep").arg("60")).unwrap();
        let child =
            OwnedChild::spawn(Command::new("sh").arg(script.path()).arg(&pid_file)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !child.exited().unwrap() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(child.exited().unwrap());
        let pid = std::fs::read_to_string(pid_file).unwrap();
        drop(child);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = Command::new("ps")
                .args(["-p", pid.trim(), "-o", "stat="])
                .output()
                .unwrap();
            let status = String::from_utf8_lossy(&status.stdout);
            if status.trim().is_empty() || status.trim().starts_with('Z') {
                break;
            }
            assert!(Instant::now() < deadline, "descendant survived cleanup");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!sentinel.exited().unwrap());
    }

    #[test]
    fn setup_output_is_bounded_and_preserves_failure() {
        let failure = output(Command::new("sh").args(["-c", "printf denied >&2; exit 9"])).unwrap();
        assert_eq!(failure.status.code(), Some(9));
        assert_eq!(failure.stderr, b"denied");
        assert!(output(Command::new("sh").args(["-c", "head -c 2097152 /dev/zero"])).is_err());
    }
}
