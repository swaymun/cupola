//! Windows pseudo-terminal backend: ConPTY.
//!
//! The shell starts suspended inside a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so everything it starts (and does not
//! explicitly break away) belongs to the job. The lifecycle owner mirrors the
//! Unix one: closing the pseudo console sends CTRL_CLOSE_EVENT to attached
//! console programs (the ConPTY analog of SIGHUP), and the job is terminated
//! when the grace period ends or when the shell exits leaving members behind.
//! ConPTY keeps the output pipe open after the shell exits until the pseudo
//! console is closed, so the owner closes it then; the reader sees EOF as on Unix.
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

pub(super) struct Spawned {
    pub pty: Pty,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub processes: Arc<ProcessOwner>,
}

/// Owned handle that is safe to share across threads.
struct Handle(HANDLE);
// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: owned, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

struct Console(Mutex<Option<HPCON>>);
impl Console {
    fn close(&self) {
        let hpc = self.0.lock().unwrap().take();
        if let Some(hpc) = hpc {
            // SAFETY: closed once; the reader thread keeps draining output,
            // which older ConPTY versions need for this call to return.
            unsafe { ClosePseudoConsole(hpc) };
        }
    }

    /// Runs `use_console` on the open pseudo console while holding the lock,
    /// so `close` cannot release the handle during the call.
    fn with_open<R>(&self, use_console: impl FnOnce(HPCON) -> R) -> Option<R> {
        let guard = self.0.lock().unwrap();
        guard.map(use_console)
    }
}

/// The pseudo console, kept for resizes.
pub(super) struct Pty(Arc<Console>);

impl Pty {
    pub fn resize(&self, cols: u16, rows: u16) -> bool {
        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        self.0
            // SAFETY: the console stays open until `with_open` returns.
            .with_open(|hpc| unsafe { ResizePseudoConsole(hpc, size) >= 0 })
            .unwrap_or(false)
    }
}

/// The only owner of the shell's process and job handles.
pub(super) struct ProcessOwner {
    process: Handle,
    job: Handle,
    console: Arc<Console>,
    state: Mutex<ProcessState>,
    changed: Condvar,
}

struct ProcessState {
    deadline: Option<Instant>,
    result: Option<Option<i32>>,
}

impl ProcessOwner {
    pub(super) fn close_after(&self, grace: Duration) {
        let mut state = self.state.lock().unwrap();
        let deadline = Instant::now() + grace;
        state.deadline = Some(state.deadline.map_or(deadline, |old| old.min(deadline)));
        self.changed.notify_all();
    }

    /// ConPTY repaints its own buffer on attach; nothing to signal.
    pub(super) fn redraw(&self) {}

    pub(super) fn force_kill(&self) {
        self.close_after(Duration::ZERO);
        // SAFETY: valid job handle; ends every member at once.
        unsafe { TerminateJobObject(self.job.0, 1) };
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

    /// The shell has exited and the session's cleanup has finished.
    pub(super) fn finished(&self) -> bool {
        self.state.lock().unwrap().result.is_some()
    }

    fn job_is_empty(&self) -> bool {
        // SAFETY: valid job handle and a correctly sized output buffer.
        unsafe {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            QueryInformationJobObject(
                self.job.0,
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            ) != 0
                && info.ActiveProcesses == 0
        }
    }

    pub(super) fn run(&self) {
        let mut hung_up = false;
        let mut shell_exited = false;
        loop {
            if shell_exited {
                std::thread::sleep(Duration::from_millis(50));
            // SAFETY: valid process handle.
            } else if unsafe { WaitForSingleObject(self.process.0, 50) } == WAIT_OBJECT_0 {
                shell_exited = true;
                // A shell that exits ahead of its jobs must not keep the
                // session open indefinitely just because a job holds the console.
                self.close_after(Duration::from_secs(2));
            }
            if shell_exited && self.job_is_empty() {
                break;
            }
            let deadline = self.state.lock().unwrap().deadline;
            if deadline.is_some() && !hung_up {
                self.console.close();
                hung_up = true;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                // SAFETY: valid job handle.
                unsafe { TerminateJobObject(self.job.0, 1) };
                if shell_exited {
                    break;
                }
            }
        }
        // The output pipe reaches EOF only once the pseudo console is closed.
        self.console.close();
        let mut code = 0u32;
        // SAFETY: valid process handle of an exited process.
        let code =
            (unsafe { GetExitCodeProcess(self.process.0, &mut code) } != 0).then_some(code as i32);
        self.state.lock().unwrap().result = Some(code);
        self.changed.notify_all();
    }
}

impl Drop for ProcessOwner {
    fn drop(&mut self) {
        // Closing the job handle would kill its members too; be explicit.
        // SAFETY: valid job handle.
        unsafe { TerminateJobObject(self.job.0, 1) };
        self.console.close();
    }
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// Quotes one argument for CommandLineToArgvW.
fn quote(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_owned();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in argument.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

fn environment_block(env: &[(String, String)]) -> Vec<u16> {
    let mut sorted: Vec<_> = env.iter().collect();
    sorted.sort_by_key(|(key, _)| key.to_uppercase());
    let mut block = Vec::new();
    for (key, value) in sorted {
        block.extend(format!("{key}={value}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

pub(super) fn spawn(
    shell: &Path,
    _shell_name: &str,
    cwd: &Path,
    env: Vec<(String, String)>,
    cols: u16,
    rows: u16,
) -> std::io::Result<Spawned> {
    // SAFETY: every handle created below is owned by exactly one wrapper.
    unsafe {
        let (mut in_read, mut in_write, mut out_read, mut out_write) = (
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if CreatePipe(&mut in_read, &mut in_write, std::ptr::null(), 0) == 0 {
            return Err(last_error());
        }
        let in_read = Handle(in_read);
        let in_write = OwnedHandle::from_raw_handle(in_write);
        if CreatePipe(&mut out_read, &mut out_write, std::ptr::null(), 0) == 0 {
            return Err(last_error());
        }
        let out_read = OwnedHandle::from_raw_handle(out_read);
        let out_write = Handle(out_write);

        let mut hpc: HPCON = 0;
        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        let result = CreatePseudoConsole(size, in_read.0, out_write.0, 0, &mut hpc);
        if result < 0 {
            return Err(std::io::Error::from_raw_os_error(result));
        }
        // The pseudo console holds its own duplicates.
        drop(in_read);
        drop(out_write);
        let console = Arc::new(Console(Mutex::new(Some(hpc))));

        let mut attribute_size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attribute_size);
        let mut attributes = vec![0u8; attribute_size];
        let list = attributes.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(list, 1, 0, &mut attribute_size) == 0 {
            console.close();
            return Err(last_error());
        }
        if UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
        {
            DeleteProcThreadAttributeList(list);
            console.close();
            return Err(last_error());
        }
        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // Without this a child of a process whose stdio is redirected (wonderd
        // writes to log files; tests run under SSH) uses those handles instead
        // of the pseudo console (measured: the prompt reached the parent's output).
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.lpAttributeList = list;

        let mut command_line: Vec<u16> = format!("{} -NoLogo", quote(&shell.to_string_lossy()))
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut block = environment_block(&env);
        let cwd = wide(cwd.as_os_str());
        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        let created = CreateProcessW(
            std::ptr::null(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            block.as_mut_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        );
        DeleteProcThreadAttributeList(list);
        if created == 0 {
            let error = last_error();
            console.close();
            return Err(error);
        }
        let process = Handle(info.hProcess);
        let thread = Handle(info.hThread);

        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            let error = last_error();
            windows_sys::Win32::System::Threading::TerminateProcess(process.0, 1);
            console.close();
            return Err(error);
        }
        let job = Handle(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
            || AssignProcessToJobObject(job.0, process.0) == 0
        {
            let error = last_error();
            windows_sys::Win32::System::Threading::TerminateProcess(process.0, 1);
            console.close();
            return Err(error);
        }
        ResumeThread(thread.0);
        drop(thread);

        Ok(Spawned {
            pty: Pty(Arc::clone(&console)),
            reader: Box::new(std::fs::File::from(out_read)),
            writer: Box::new(std::fs::File::from(in_write)),
            processes: Arc::new(ProcessOwner {
                process,
                job,
                console,
                state: Mutex::new(ProcessState {
                    deadline: None,
                    result: None,
                }),
                changed: Condvar::new(),
            }),
        })
    }
}

/// PowerShell 7 when installed, else Windows PowerShell, else `%ComSpec%`.
/// `WONDER_TERMINAL_SHELL` overrides.
pub(super) fn login_shell() -> Option<PathBuf> {
    let candidates = std::env::var_os("WONDER_TERMINAL_SHELL")
        .map(PathBuf::from)
        .into_iter()
        .chain(std::env::var_os("ProgramFiles").map(|dir| {
            PathBuf::from(dir)
                .join("PowerShell")
                .join("7")
                .join("pwsh.exe")
        }))
        .chain(std::env::var_os("SystemRoot").map(|dir| {
            PathBuf::from(dir)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        }))
        .chain(std::env::var_os("ComSpec").map(PathBuf::from));
    candidates
        .into_iter()
        .find(|path| path.is_absolute() && path.is_file())
}

/// Only what a user shell needs; Wonder's own capability, provider
/// credentials and daemon configuration never reach the terminal.
pub(super) fn sanitized_environment(shell: &Path) -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "USERPROFILE",
        "USERNAME",
        "USERDOMAIN",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "SystemRoot",
        "SystemDrive",
        "windir",
        "ComSpec",
        "PATH",
        "PATHEXT",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
        "CommonProgramFiles",
        "PUBLIC",
        "COMPUTERNAME",
        "PROCESSOR_ARCHITECTURE",
        "NUMBER_OF_PROCESSORS",
        "OS",
    ];
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| KEEP.iter().any(|keep| keep.eq_ignore_ascii_case(key)))
        .collect();
    env.extend([
        ("SHELL".into(), shell.to_string_lossy().into_owned()),
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        ("TERM_PROGRAM".into(), "Cupola".into()),
    ]);
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_like_command_line_to_argv() {
        assert_eq!(quote("C:\\Windows\\pwsh.exe"), "C:\\Windows\\pwsh.exe");
        assert_eq!(
            quote("C:\\Program Files\\PowerShell\\7\\pwsh.exe"),
            "\"C:\\Program Files\\PowerShell\\7\\pwsh.exe\""
        );
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("end\\ "), "\"end\\ \"");
        assert_eq!(quote("tail\\"), "tail\\");
        assert_eq!(quote(""), "\"\"");
    }

    #[test]
    fn environment_block_is_sorted_and_double_terminated() {
        let block = environment_block(&[("b".into(), "2".into()), ("A".into(), "1".into())]);
        let text = String::from_utf16(&block).unwrap();
        assert_eq!(text, "A=1\0b=2\0\0");
    }

    /// End to end through ConPTY: PowerShell runs in the folder, its output
    /// arrives, resize is accepted, and closing ends a child it started and
    /// then reports the shell's exit.
    #[test]
    fn powershell_runs_resizes_and_closing_ends_its_tree() {
        let shell = login_shell().expect("a Windows shell");
        let dir = tempfile::tempdir().unwrap();
        let Spawned {
            pty,
            mut reader,
            mut writer,
            processes,
        } = spawn(
            &shell,
            "powershell",
            dir.path(),
            sanitized_environment(&shell),
            100,
            30,
        )
        .unwrap();
        let owner = Arc::clone(&processes);
        std::thread::spawn(move || owner.run());
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 || tx.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut seen = String::new();
        let wait_for = |needle: &str, seen: &mut String| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !seen.contains(needle) {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !left.is_zero(),
                    "timed out waiting for {needle:?}; saw {seen:?}"
                );
                if let Ok(bytes) = rx.recv_timeout(left) {
                    seen.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
        };
        // A sleeping child whose PID we print. Markers are concatenated so the
        // echoed (and colorized) command line never contains them.
        writer
            .write_all(b"$p = Start-Process -PassThru -WindowStyle Hidden ping -ArgumentList '-n','120','127.0.0.1'; ('WPID' + '=' + $p.Id + '='); ('WCWD' + '=' + (Get-Location).Path + '=')\r")
            .unwrap();
        wait_for("WCWD=", &mut seen);
        let value = |marker: &str| {
            let at = seen.find(marker).unwrap() + marker.len();
            seen[at..].split('=').next().unwrap().trim().to_owned()
        };
        let folder = crate::canonical_path::canonicalize(dir.path()).unwrap();
        let folder = folder.to_string_lossy();
        assert_eq!(value("WCWD="), folder.trim_start_matches(r"\\?\"));
        let pid: i32 = value("WPID=").parse().unwrap();
        assert!(crate::platform::process_alive(pid));
        assert!(pty.resize(120, 40));
        assert!(!processes.finished());
        processes.close_after(Duration::from_millis(500));
        let started = Instant::now();
        processes.wait();
        assert!(processes.finished());
        assert!(started.elapsed() < Duration::from_secs(10));
        let deadline = Instant::now() + Duration::from_secs(10);
        while crate::platform::process_alive(pid) {
            assert!(Instant::now() < deadline, "child {pid} survived close");
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!pty.resize(80, 24), "the console is closed");
    }

    /// Closing waits for a resize that is using the pseudo console, and
    /// resizes racing the close either succeed on the open console or report
    /// it closed; none reaches a released handle.
    #[test]
    fn close_waits_for_a_resize_in_progress() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let shell = login_shell().expect("a Windows shell");
        let dir = tempfile::tempdir().unwrap();
        let Spawned {
            pty,
            mut reader,
            writer: _writer,
            processes,
        } = spawn(
            &shell,
            "powershell",
            dir.path(),
            sanitized_environment(&shell),
            80,
            24,
        )
        .unwrap();
        // ClosePseudoConsole can wait for the output to be drained.
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink);
        });
        let console = Arc::clone(&pty.0);
        let pty = Arc::new(pty);
        let resizers: Vec<_> = (0..4u16)
            .map(|index| {
                let pty = Arc::clone(&pty);
                std::thread::spawn(move || {
                    let mut resized = 0u16;
                    while pty.resize(80 + index, 24 + resized % 10) {
                        resized = resized.wrapping_add(1);
                    }
                })
            })
            .collect();
        let closed = Arc::new(AtomicBool::new(false));
        let (closer, resized) = console
            .with_open(|hpc| {
                let closer = {
                    let console = Arc::clone(&console);
                    let closed = Arc::clone(&closed);
                    std::thread::spawn(move || {
                        console.close();
                        closed.store(true, Ordering::SeqCst);
                    })
                };
                std::thread::sleep(Duration::from_millis(300));
                assert!(
                    !closed.load(Ordering::SeqCst),
                    "the console closed while a resize held it"
                );
                // SAFETY: still open; that is the contract under test.
                let resized = unsafe { ResizePseudoConsole(hpc, COORD { X: 90, Y: 30 }) } >= 0;
                (closer, resized)
            })
            .expect("the console starts open");
        assert!(resized, "a resize on the held console succeeds");
        closer.join().unwrap();
        assert!(closed.load(Ordering::SeqCst));
        for resizer in resizers {
            resizer.join().unwrap();
        }
        assert!(!pty.resize(80, 24), "the console is closed");
        processes.force_kill();
    }

    /// A shell that exits on its own is reported promptly with its exit code.
    #[test]
    fn shell_exit_is_reported_with_its_code() {
        let shell = login_shell().expect("a Windows shell");
        let dir = tempfile::tempdir().unwrap();
        let Spawned {
            mut reader,
            mut writer,
            processes,
            ..
        } = spawn(
            &shell,
            "powershell",
            dir.path(),
            sanitized_environment(&shell),
            80,
            24,
        )
        .unwrap();
        let owner = Arc::clone(&processes);
        std::thread::spawn(move || owner.run());
        let drained = std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink);
        });
        writer.write_all(b"exit 7\r").unwrap();
        assert_eq!(processes.wait(), Some(7));
        // The owner closed the console, so the output pipe reached EOF.
        drained.join().unwrap();
    }
}
