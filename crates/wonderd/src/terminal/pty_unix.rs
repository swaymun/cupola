//! Unix pseudo-terminal backend: `openpty`, a login shell leading its own
//! session (`setsid` + controlling terminal), and a lifecycle owner that signals
//! every member of that session by identity.
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::{Duration, Instant};

pub(super) struct Spawned {
    pub pty: Pty,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub processes: Arc<ProcessOwner>,
}

/// The PTY master, kept for resizes.
pub(super) struct Pty(pub(super) OwnedFd);

impl Pty {
    pub fn resize(&self, cols: u16, rows: u16) -> bool {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads a winsize; the kernel signals the
        // foreground process group when the size changes.
        unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCSWINSZ, &size) == 0 }
    }
}

pub(super) fn spawn(
    shell: &FsPath,
    shell_name: &str,
    cwd: &FsPath,
    env: Vec<(String, String)>,
    cols: u16,
    rows: u16,
) -> std::io::Result<Spawned> {
    // The lifetime proof requires waitable children, including when this
    // daemon inherits its launcher's signal dispositions. No other owner
    // reaps these std::process::Children; Tokio's SIGCHLD handler may notify
    // waiters, but must not auto-reap them.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(std::io::Error::other(
            "Terminal cleanup requires waitable children",
        ));
    }
    // Kernels without pidfds cannot safely clean up independent job-control
    // groups. Refuse to open a terminal instead of falling back to PID kills.
    #[cfg(target_os = "linux")]
    let _identity_support = ProcessIdentity::capture(std::process::id() as i32)
        .ok_or_else(|| std::io::Error::other("Terminal cleanup requires pidfds"))?;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty writes two descriptors on success; both are owned below.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: freshly created descriptors with no other owner.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    set_cloexec(&master)?;
    set_cloexec(&slave)?;

    let mut command = std::process::Command::new(shell);
    // A leading dash makes the shell a login shell, as Terminal.app does.
    command
        .arg0(format!("-{shell_name}"))
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::from(slave.try_clone()?))
        .stdout(std::process::Stdio::from(slave.try_clone()?))
        .stderr(std::process::Stdio::from(slave));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    // The spawned command owned the slave copies; drop ours now so the
    // master sees EOF once the shell and its children exit.
    drop(command);
    let processes = Arc::new(ProcessOwner::new(child));
    let writer = std::fs::File::from(master.try_clone()?);
    Ok(Spawned {
        pty: Pty(master.try_clone()?),
        reader: Box::new(Reader {
            master: std::fs::File::from(master),
            processes: Arc::clone(&processes),
            finished_at: None,
        }),
        writer: Box::new(writer),
        processes,
    })
}

/// How long output is still drained after the shell has been reaped. A process
/// that left the session can keep the PTY open (and silent) indefinitely.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);
/// How often a silent reader checks whether the shell has been reaped.
const DRAIN_POLL_MS: i32 = 500;

/// The PTY master's read side. It reports end of output once the shell has
/// been reaped and the drain bound has passed, so the reader thread always
/// ends and releases the master instead of waiting on a process that escaped.
struct Reader {
    master: std::fs::File,
    processes: Arc<ProcessOwner>,
    finished_at: Option<Instant>,
}

impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.processes.finished() {
                let at = *self.finished_at.get_or_insert_with(Instant::now);
                if at.elapsed() >= DRAIN_AFTER_EXIT {
                    return Ok(0);
                }
            }
            let mut poll = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd on a descriptor this reader owns.
            match unsafe { libc::poll(&mut poll, 1, DRAIN_POLL_MS) } {
                0 => continue,
                ready if ready > 0 => return self.master.read(buffer),
                _ => {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
        }
    }
}

/// How long the lifecycle owner waits for a signalled shell to be reaped
/// before handing it to the late reaper.
const REAP_BOUND: Duration = Duration::from_secs(2);

/// Reaps `child` within `bound`. A child still exiting after that (macOS can
/// hold an exiting process while its terminal output drains) is kept by the
/// late reaper: its PID stays reserved until it is reaped, and nothing signals
/// it again.
pub(super) fn reap_within(mut child: std::process::Child, bound: Duration) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    let until = Instant::now() + bound;
    let mut pause = Duration::from_millis(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Some(
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
                )
            }
            Ok(None) if Instant::now() < until => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(50));
            }
            Ok(None) => {
                reap_later(child);
                return None;
            }
            // ECHILD: there is no waitable child left to own.
            Err(_) => return None,
        }
    }
}

struct LateReaps {
    children: Vec<std::process::Child>,
    running: bool,
}

static LATE_REAPS: StdMutex<LateReaps> = StdMutex::new(LateReaps {
    children: Vec::new(),
    running: false,
});

/// One shared thread polls slow exits until each is reaped, then ends.
fn reap_later(child: std::process::Child) {
    let mut late = LATE_REAPS.lock().unwrap();
    late.children.push(child);
    if late.running {
        return;
    }
    let started = std::thread::Builder::new()
        .name("wonder-terminal-reaper".into())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_millis(500));
            let mut late = LATE_REAPS.lock().unwrap();
            late.children
                .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
            if late.children.is_empty() {
                late.running = false;
                return;
            }
        });
    // Without a thread the child stays held (and its PID reserved); the next
    // slow exit retries starting the reaper.
    late.running = started.is_ok();
}

#[cfg(test)]
pub(super) fn late_reaps_pending(pid: u32) -> bool {
    LATE_REAPS
        .lock()
        .unwrap()
        .children
        .iter()
        .any(|child| child.id() == pid)
}

/// The only owner allowed to reap the shell. An unreaped direct child reserves
/// its PID, including while it is a zombie, so no replacement session can acquire
/// that SID during enumeration. This mutex covers validation, signalling and
/// transfer to wait; a retired owner never uses its old numeric SID again.
pub(super) struct ProcessOwner {
    pub(super) state: StdMutex<ProcessState>,
    pub(super) changed: Condvar,
}

pub(super) struct ProcessState {
    pub(super) leader: i32,
    pub(super) child: Option<std::process::Child>,
    pub(super) deadline: Option<Instant>,
    pub(super) hup_sent: bool,
    pub(super) redraw: bool,
    pub(super) result: Option<Option<i32>>,
}

impl ProcessOwner {
    pub(super) fn new(child: std::process::Child) -> Self {
        Self {
            state: StdMutex::new(ProcessState {
                leader: child.id() as i32,
                child: Some(child),
                deadline: None,
                hup_sent: false,
                redraw: false,
                result: None,
            }),
            changed: Condvar::new(),
        }
    }

    pub(super) fn close_after(&self, grace: Duration) {
        let mut state = self.state.lock().unwrap();
        let deadline = Instant::now() + grace;
        state.deadline = Some(state.deadline.map_or(deadline, |old| old.min(deadline)));
        self.changed.notify_all();
    }

    pub(super) fn signal(&self, signal: i32) {
        signal_session(&self.state.lock().unwrap(), signal);
    }

    pub(super) fn redraw(&self) {
        self.state.lock().unwrap().redraw = true;
        self.changed.notify_all();
    }

    pub(super) fn force_kill(&self) {
        self.close_after(Duration::ZERO);
        // Shutdown must deliver KILL before the daemon can leave, even if the
        // lifecycle thread has not been scheduled yet.
        self.signal(libc::SIGKILL);
    }

    pub(super) fn wait(&self) -> Option<i32> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(result) = state.result {
                return result;
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    /// The shell has been reaped (or its lifetime evidence is gone).
    pub(super) fn finished(&self) -> bool {
        self.state.lock().unwrap().result.is_some()
    }

    pub(super) fn run(&self) {
        let mut state = self.state.lock().unwrap();
        let child = loop {
            match state.exited_unreaped() {
                Ok(true) => {
                    // Keep the SID reserved through the complete membership
                    // scan. A plain exit needs no grace, but an unreadable scan
                    // or a member whose identity cannot be captured is never
                    // evidence that cleanup has finished.
                    if state.session_is_empty().unwrap_or(false) {
                        break state.child.take();
                    }
                    // A shell that exits ahead of children must not stay a
                    // zombie indefinitely just because a job holds the PTY.
                    state
                        .deadline
                        .get_or_insert_with(|| Instant::now() + Duration::from_secs(2));
                }
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    // Missing lifetime evidence never authorizes a signal.
                    state.child = None;
                    state.result = Some(None);
                    self.changed.notify_all();
                    return;
                }
                _ => {}
            }
            if state.redraw {
                signal_session(&state, libc::SIGWINCH);
                state.redraw = false;
            }
            if let Some(deadline) = state.deadline {
                if !state.hup_sent {
                    signal_session(&state, libc::SIGHUP);
                    state.hup_sent = true;
                }
                if Instant::now() >= deadline {
                    signal_session(&state, libc::SIGKILL);
                    break state.child.take();
                }
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        };
        // Remove lifetime authority under the mutex before either retirement
        // path reaps the leader. Pending requests then cannot reuse its SID.
        drop(state);
        // Bounded, so a shell stuck exiting cannot hold this owner (and the
        // reader waiting on it) indefinitely.
        let code = child.and_then(|child| reap_within(child, REAP_BOUND));
        self.state.lock().unwrap().result = Some(code);
        self.changed.notify_all();
    }
}

impl ProcessState {
    /// This is an absence check, never permission to signal a numeric PID.
    /// Count even members whose audit token/pidfd would be unreadable. Only
    /// ESRCH means there is no live process at the lookup (including macOS
    /// zombies); other lookup errors and incomplete enumeration stay unknown.
    pub(super) fn session_is_empty(&self) -> std::io::Result<bool> {
        let pids = all_pids()?;
        if !pids.contains(&self.leader) {
            return Err(std::io::Error::other("Terminal leader missing from scan"));
        }
        for pid in pids
            .into_iter()
            .filter(|pid| *pid > 0 && *pid != self.leader)
        {
            let session = unsafe { libc::getsid(pid) };
            if session == self.leader {
                return Ok(false);
            }
            if session < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        Ok(true)
    }

    pub(super) fn exited_unreaped(&self) -> std::io::Result<bool> {
        let child = self
            .child
            .as_ref()
            .filter(|child| child.id() as i32 == self.leader)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ECHILD))?;
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // WNOWAIT is essential: try_wait would release the SID before signals.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { info.si_pid() } != 0)
    }
}

impl Drop for ProcessOwner {
    fn drop(&mut self) {
        // Only used if starting the lifecycle thread failed. Normal completion
        // took the child before reaping it, and no retained numeric ID is used.
        let state = self.state.get_mut().unwrap();
        signal_session(state, libc::SIGKILL);
        if let Some(mut child) = state.child.take() {
            let _ = child.wait();
        }
    }
}

/// Job control creates separate process groups. Capture every session member
/// while the leader's SID is reserved, then signal each *identity*, never a
/// numeric group that could disappear and be reused between validation and kill.
pub(super) fn signal_session(owner: &ProcessState, signal: libc::c_int) {
    if owner.exited_unreaped().is_err() {
        return;
    }
    // Re-enumerate after KILL so a job that forks/execs while the first snapshot
    // is delivered is picked up under fresh identity evidence. This is bounded;
    // session cleanup is not a sandbox for intentionally escaping descendants.
    for _ in 0..if signal == libc::SIGKILL { 3 } else { 1 } {
        let members: Vec<_> = all_pids()
            .unwrap_or_else(|error| {
                eprintln!("Terminal process scan failed; background cleanup unverified: {error}");
                Vec::new()
            })
            .into_iter()
            .filter(|pid| *pid > 0 && *pid != owner.leader)
            // This first lookup is only a cheap filter; in_session must still bind
            // membership to a stable identity before any signal is authorized.
            .filter(|pid| unsafe { libc::getsid(*pid) } == owner.leader)
            .filter_map(|pid| ProcessIdentity::in_session(pid, owner.leader))
            .collect();
        for member in members {
            let _ = member.signal(signal);
            if signal == libc::SIGHUP {
                let _ = member.signal(libc::SIGCONT);
            }
        }
        // This PID cannot be reused: the retained Child is still waitable and no
        // other path reaps it. The same proof holds after the shell has exited.
        unsafe {
            libc::kill(owner.leader, signal);
            if signal == libc::SIGHUP {
                libc::kill(owner.leader, libc::SIGCONT);
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(PartialEq, Eq)]
pub(super) struct ProcessIdentity(pub(super) [u32; 8]);

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn task_name_for_pid(task: u32, pid: i32, name: *mut u32) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    fn proc_signal_with_audittoken(token: *const [u32; 8], signal: i32) -> i32;
}

#[cfg(target_os = "macos")]
impl ProcessIdentity {
    pub(super) fn capture(pid: i32) -> Option<Self> {
        let mut port = 0;
        #[allow(deprecated)]
        let task = unsafe { libc::mach_task_self_ };
        if unsafe { task_name_for_pid(task, pid, &mut port) } != 0 {
            return None;
        }
        let mut token = [0u32; 8];
        let mut count = 8;
        // TASK_AUDIT_TOKEN, from mach/task_info.h. A task-name right suffices;
        // no task-control entitlement is requested. Release it on every path.
        let result = unsafe { libc::task_info(port, 15, token.as_mut_ptr().cast(), &mut count) };
        unsafe {
            mach_port_deallocate(task, port);
        }
        (result == 0 && count == 8).then_some(Self(token))
    }

    pub(super) fn in_session(pid: i32, leader: i32) -> Option<Self> {
        let identity = Self::capture(pid)?;
        let session = unsafe { libc::getsid(pid) };
        // Sandwich the numeric lookup, so it cannot describe a replacement.
        (session == leader && Self::capture(pid).as_ref() == Some(&identity)).then_some(identity)
    }

    pub(super) fn signal(&self, signal: i32) -> std::io::Result<()> {
        // XNU validates PID + generation while holding a proc reference through
        // signal delivery. Exit/reap/reuse after capture returns ESRCH, not a
        // signal to a replacement. Never fall back to numeric kill on failure.
        let error = unsafe { proc_signal_with_audittoken(&self.0, signal) };
        if error == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(error))
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) struct ProcessIdentity(pub(super) OwnedFd);

#[cfg(target_os = "linux")]
impl ProcessIdentity {
    pub(super) fn capture(pid: i32) -> Option<Self> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        (fd >= 0).then(|| Self(unsafe { OwnedFd::from_raw_fd(fd as i32) }))
    }

    pub(super) fn in_session(pid: i32, leader: i32) -> Option<Self> {
        let identity = Self::capture(pid)?;
        let session = unsafe { libc::getsid(pid) };
        // A non-readable pidfd proves this exact task has not exited during
        // getsid; a replacement PID cannot pass this check for the old handle.
        let mut poll = libc::pollfd {
            fd: identity.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        (session == leader && unsafe { libc::poll(&mut poll, 1, 0) } == 0).then_some(identity)
    }

    pub(super) fn signal(&self, signal: i32) -> std::io::Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) fn all_pids() -> std::io::Result<Vec<i32>> {
    let mut pids = vec![0i32; 8192];
    for _ in 0..4 {
        // SAFETY: the buffer length is passed in bytes. A full buffer may have
        // omitted members and must never establish that the session is empty.
        let count = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                (pids.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        if count <= 0 {
            return Err(std::io::Error::other("Terminal process scan failed"));
        }
        if (count as usize) < pids.len() {
            pids.truncate(count as usize);
            return Ok(pids);
        }
        pids.resize(pids.len() * 2, 0);
    }
    Err(std::io::Error::other("Terminal process scan was truncated"))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn all_pids() -> std::io::Result<Vec<i32>> {
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

fn set_cloexec(fd: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The owner's login shell from the password database, then `$SHELL`. Only a
/// shell listed in `/etc/shells` is used.
pub(super) fn login_shell() -> Option<PathBuf> {
    let allowed = std::fs::read_to_string("/etc/shells").ok();
    let permitted = |path: &FsPath| {
        path.is_absolute()
            && path.is_file()
            && allowed.as_deref().is_none_or(|list| {
                list.lines()
                    .map(str::trim)
                    .any(|line| !line.starts_with('#') && FsPath::new(line) == path)
            })
    };
    passwd_shell()
        .into_iter()
        .chain(std::env::var_os("SHELL").map(PathBuf::from))
        .chain([PathBuf::from("/bin/zsh"), PathBuf::from("/bin/sh")])
        .find(|path| permitted(path))
}

fn passwd_shell() -> Option<PathBuf> {
    let mut record: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes into the buffers we pass and sets `result`.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut record,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || record.pw_shell.is_null() {
        return None;
    }
    // SAFETY: pw_shell points into `buffer`, NUL-terminated.
    let shell = unsafe { std::ffi::CStr::from_ptr(record.pw_shell) };
    Some(PathBuf::from(shell.to_str().ok()?))
}

/// Only the variables a login shell needs. Wonder's own capability, provider
/// credentials and the daemon's configuration never reach the terminal.
pub(super) fn sanitized_environment(shell: &FsPath) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .chain(std::env::vars().filter(|(key, _)| key.starts_with("LC_") && key.len() <= 16))
        .collect();
    if !env.iter().any(|(key, _)| key == "LANG") {
        env.push(("LANG".into(), "en_US.UTF-8".into()));
    }
    env.extend([
        ("SHELL".into(), shell.to_string_lossy().into_owned()),
        (
            "PATH".into(),
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
        ),
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        ("TERM_PROGRAM".into(), "Cupola".into()),
    ]);
    env
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    /// A silent slave held outside the shell's session cannot retain the output
    /// worker forever. Test the read owner directly: a controlling session's
    /// exit may revoke its slave on macOS even while masters remain open, so a
    /// slave read (or an escaped process doing that read) cannot prove this.
    #[test]
    fn silent_slave_does_not_hold_the_reader_after_shell_exit() {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty initializes two descriptors, owned immediately below.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        set_cloexec(&master).unwrap();
        set_cloexec(&slave).unwrap();
        // No process acquires this PTY as its controlling terminal. Its slave
        // stays open until this test explicitly releases it, on every Unix.
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .spawn()
            .unwrap();
        let owner = Arc::new(ProcessOwner::new(child));
        owner.run();
        assert_eq!(owner.wait(), Some(7));
        let mut reader = Reader {
            master: std::fs::File::from(master),
            processes: Arc::clone(&owner),
            finished_at: None,
        };
        let (done, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = reader.read(&mut [0; 1]);
            drop(reader);
            let _ = done.send(result);
        });
        let result = completed.recv_timeout(Duration::from_secs(30));
        // Also unblock and join a regressed reader before asserting failure.
        drop(slave);
        worker.join().unwrap();
        assert_eq!(
            result
                .expect("silent terminal retained the reader")
                .unwrap(),
            0,
            "the drain bound must report end of output"
        );
        assert_eq!(Arc::strong_count(&owner), 1, "reader owner retained");
    }
}
