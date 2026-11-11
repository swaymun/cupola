//! EXPERIMENTAL Windows operating-system layer of the headless supervisor.
//!
//! - Data lives in `%LOCALAPPDATA%\Wonder` unless `WONDER_DATA_DIR` names
//!   another directory. Either way the directory is owned by this user with a
//!   protected ACL granting only the user, administrators and SYSTEM, and
//!   secrets are created with that ACL rather than inheriting a shared one.
//! - The supervisor lock is a file opened with no sharing: a second open
//!   fails until the owner exits, including after a crash.
//! - The stop channel is a per-data-directory named pipe created with
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE` (no squatting) and
//!   `PIPE_REJECT_REMOTE_CLIENTS`. Its default security grants write access
//!   only to the creating user, administrators and SYSTEM.
//! - Ctrl-C, Ctrl-Break and console close request a stop.
use super::{Paths, Result, HOST_RUNNING, STOPPING};
use std::{
    io::Write,
    os::windows::fs::OpenOptionsExt,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::Ordering,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetHandleInformation, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, HANDLE,
    HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    ReadFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OPEN_REPARSE_POINT, PIPE_ACCESS_INBOUND,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_NOWAIT, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
};

pub(super) fn default_data_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(base).join("Wonder"))
}

/// The current user's SID as SDDL text (`S-1-5-21-...`).
fn user_sid() -> Result<String> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: the token handle and the SID string are owned and released here;
    // the buffer is u64-aligned and sized by the first call.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut size = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size);
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let length = (0..).take_while(|&i| *text.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        windows_sys::Win32::Foundation::LocalFree(text.cast());
        Ok(sid)
    }
}

/// A security descriptor owned by this user whose protected DACL grants only
/// the user, administrators and SYSTEM; `inherit` passes it on to a
/// directory's children.
struct PrivateDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

impl PrivateDescriptor {
    fn new(inherit: bool) -> Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let sid = user_sid()?;
        let flags = if inherit { "OICI" } else { "" };
        let sddl = wide(&format!(
            "O:{sid}D:P(A;{flags};FA;;;{sid})(A;{flags};FA;;;SY)(A;{flags};FA;;;BA)"
        ));
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: the descriptor is released by Drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(descriptor))
    }
}

impl Drop for PrivateDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Takes ownership of `path` and replaces its ACL with the private one,
/// which its children inherit. A custom `WONDER_DATA_DIR` may sit somewhere
/// shared; this fails rather than leave secrets readable there.
pub(super) fn restrict_dir(path: &Path) -> Result<()> {
    use windows_sys::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    let descriptor = PrivateDescriptor::new(true)?;
    let name = wide_path(path);
    // SAFETY: the owner and DACL point into `descriptor`, alive for the call.
    let status = unsafe {
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl = std::ptr::null_mut();
        let mut owner = std::ptr::null_mut();
        GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted);
        GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted);
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            owner,
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if status != 0 {
        return Err(format!(
            "could not make {} private to this user ({}); choose a directory you own",
            path.display(),
            std::io::Error::from_raw_os_error(status as i32)
        )
        .into());
    }
    Ok(())
}

/// Opens `path` as std does; a file this call creates gets the private
/// ACL from the start instead of inheriting its directory's.
fn open_private(path: &Path, disposition: u32, access: u32) -> std::io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let descriptor =
        PrivateDescriptor::new(false).map_err(|error| std::io::Error::other(error.to_string()))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let name = wide_path(path);
    // SAFETY: the returned handle is owned by the File.
    unsafe {
        let handle = CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            disposition,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(std::fs::File::from_raw_handle(handle))
    }
}

pub(super) fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::CREATE_NEW;
    let tmp = path.with_extension("tmp");
    // A leftover (or planted) temporary file keeps its own ACL and any open
    // handle, so it is removed and the new one must be created fresh.
    match std::fs::remove_file(&tmp) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let mut file = open_private(&tmp, CREATE_NEW, GENERIC_WRITE)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Creates the QR with its private ACL, refusing an existing destination.
pub(super) fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::CREATE_NEW;
    open_private(path, CREATE_NEW, GENERIC_WRITE)
}

pub(super) fn append_private(path: &Path) -> std::io::Result<std::fs::File> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_APPEND_DATA, OPEN_ALWAYS};
    open_private(path, OPEN_ALWAYS, FILE_APPEND_DATA)
}

pub(super) struct SupervisorLock(#[allow(dead_code)] std::fs::File);

pub(super) fn supervisor_lock(paths: &Paths) -> Result<SupervisorLock> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(paths.lock())
        .map(SupervisorLock)
        .map_err(|_| HOST_RUNNING.into())
}

fn pipe_name(paths: &Paths) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(paths.state.to_string_lossy().to_lowercase().as_bytes());
    format!(r"\\.\pipe\wonder-host-{}", &hex::encode(digest)[..24])
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(super) struct ControlListener(HANDLE);

impl ControlListener {
    pub(super) fn bind(paths: &Paths) -> Result<Self> {
        let name = wide(&pipe_name(paths));
        // SAFETY: the handle is owned by the returned listener.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                0,
                64,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!(
                "the control pipe is in use ({}); is another Cupola host running?",
                std::io::Error::last_os_error()
            )
            .into());
        }
        Ok(Self(handle))
    }

    /// True once a stop was requested by the pipe or a console event.
    pub(super) fn stopping(&self) -> bool {
        // SAFETY: nonblocking pipe operations on our own handle and buffer.
        unsafe {
            // A client that already wrote and closed leaves ERROR_NO_DATA; its
            // bytes are still readable until DisconnectNamedPipe.
            let connected = ConnectNamedPipe(self.0, std::ptr::null_mut()) != 0
                || matches!(GetLastError(), ERROR_PIPE_CONNECTED | ERROR_NO_DATA);
            if connected {
                let mut input = [0u8; 5];
                let mut filled = 0usize;
                for _ in 0..20 {
                    let mut read = 0u32;
                    let ok = ReadFile(
                        self.0,
                        input[filled..].as_mut_ptr(),
                        (input.len() - filled) as u32,
                        &mut read,
                        std::ptr::null_mut(),
                    ) != 0;
                    filled += read as usize;
                    // ERROR_NO_DATA: nothing yet in nowait mode; anything else
                    // (ERROR_BROKEN_PIPE once a closed client is drained) ends.
                    if filled == input.len() || (!ok && GetLastError() != ERROR_NO_DATA) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                if filled == input.len() && input == *b"stop\n" {
                    STOPPING.store(true, Ordering::SeqCst);
                }
                DisconnectNamedPipe(self.0);
            }
        }
        STOPPING.load(Ordering::SeqCst)
    }
}

impl Drop for ControlListener {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

pub(super) fn send_stop(paths: &Paths) -> Result<()> {
    let mut pipe = std::fs::OpenOptions::new()
        .write(true)
        .open(pipe_name(paths))
        .map_err(|_| "host is starting; retry stop in a moment")?;
    pipe.write_all(b"stop\n")?;
    // FlushFileBuffers on a pipe waits until the supervisor has read the
    // request; bound it so a wedged supervisor cannot hang `stop` forever.
    let (done, flushed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(pipe.sync_all());
    });
    match flushed.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(result) => Ok(result?),
        Err(_) => Err("the host did not take the stop request within 5 seconds".into()),
    }
}

unsafe extern "system" fn on_console_event(_: u32) -> windows_sys::core::BOOL {
    STOPPING.store(true, Ordering::SeqCst);
    1
}

pub(super) fn install_stop_handlers() {
    // SAFETY: registers a handler that only stores an atomic.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console_event), 1);
    }
}

/// The console handler also sees the console closing (CTRL_CLOSE_EVENT),
/// Windows' hang-up.
pub(super) fn install_pair_stop_handlers() {
    install_stop_handlers();
}

/// Starts `wonder start --foreground` with a hidden console (children
/// share it, so the supervisor can send them CTRL_BREAK), breaking away from
/// an SSH session's job when that job allows it.
pub(super) fn spawn_background(log: std::fs::File) -> Result<u32> {
    // CreateProcess inherits every inheritable handle, not only the three it
    // is given. A caller's stdout pipe (PowerShell, SSH) would then stay open
    // in the long-lived host and `start` would never appear to finish
    // (measured). `start` exits right after this, so ours need not be shared.
    for std_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: a process's own standard handle; failure leaves it unchanged.
        unsafe {
            let handle = GetStdHandle(std_handle);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    let spawn = |flags: u32| -> std::io::Result<std::process::Child> {
        Command::new(std::env::current_exe()?)
            .args(["start", "--foreground"])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .creation_flags(flags)
            .spawn()
    };
    let base = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    let child = spawn(base | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(base))?;
    Ok(child.id())
}

/// Names to try for `name` on PATH: as given, then with each PATHEXT suffix.
pub(super) fn executable_candidates(name: &str) -> Vec<String> {
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let mut names: Vec<String> = extensions
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!("{name}{}", ext.to_ascii_lowercase()))
        .collect();
    if Path::new(name).extension().is_some() {
        names.insert(0, name.to_owned());
    }
    names
}

/// Resolves the native helper installed beside the binaries, or, when Python is
/// selected, writes a launcher for the Python helper shipped in
/// `share\wonder\computer-windows\` (or `computer-windows\` beside the
/// binaries), run by WONDER_COMPUTER_PYTHON (default `python.exe` on PATH).
/// wonderd owns the helper and its children through a Job Object.
pub(super) fn computer_helper_launcher(paths: &Paths) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("WONDER_COMPUTER_USE_BIN") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err("WONDER_COMPUTER_USE_BIN does not name a file".into());
        }
        return Ok(path);
    }
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or("no executable directory")?;
    if super::computer_backend(paths)? == super::ComputerBackend::Native {
        return super::native_computer_helper(dir);
    }
    let script = [
        dir.join(r"..\share\wonder\computer-windows\wonder_computer_windows.py"),
        dir.join(r"computer-windows\wonder_computer_windows.py"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or("the Windows computer helper is not installed (share\\wonder\\computer-windows)")?;
    let python = match std::env::var_os("WONDER_COMPUTER_PYTHON") {
        Some(path) => PathBuf::from(path),
        None => super::which("python").ok_or("Python was not found; set WONDER_COMPUTER_PYTHON")?,
    };
    let script = wonderd::canonical_path::canonicalize(script)?;
    for part in [python.to_string_lossy(), script.to_string_lossy()] {
        if part.contains(['"', '%', '\r', '\n']) {
            return Err("the computer helper path cannot be used in a launcher".into());
        }
    }
    let launcher = paths.state.join("computer-helper.cmd");
    let body = format!(
        "@echo off\r\n\"{}\" -X utf8 \"{}\" %*\r\n",
        python.display(),
        script.display()
    );
    write_private(&launcher, body.as_bytes())?;
    Ok(launcher)
}

pub(super) const SERVICE_TASK: &str = "Wonder Host";

/// The host's "service" on Windows is a logon task in the user's interactive
/// session, not a Windows service: a service runs in session 0, which has no
/// desktop for the screen helper to capture.
///
/// The trigger names this user. `schtasks /SC ONLOGON` would create an
/// any-user logon trigger, which only an elevated user may register, so a
/// normal sign-in got "Access is denied"; a task definition whose trigger is
/// scoped to the registering user needs no elevation.
///
/// A task cannot carry environment, so the data directory resolved at
/// install (the default or `WONDER_DATA_DIR`) is passed as `--data-dir`;
/// without it a sign-in would start the default directory's host.
fn service_task_xml(exe: &Path, data: &Path) -> Result<String> {
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    // An SSH shell can report USERDOMAIN=WORKGROUP, which is not the local
    // account's SAM domain. Task Scheduler accepts the current token's SID
    // directly, for both the interactive principal and its logon trigger.
    let user = escape(&user_sid()?);
    let arguments = escape(&format!(
        "start --data-dir {}",
        quote_argument(&data.display().to_string())
    ));
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
  <Actions Context="Author"><Exec><Command>{}</Command><Arguments>{arguments}</Arguments></Exec></Actions>
</Task>
"#,
        escape(&exe.display().to_string())
    ))
}

/// Quotes one argument for the command line a task passes to
/// `CommandLineToArgvW`: backslashes double only before a quote.
fn quote_argument(text: &str) -> String {
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for ch in text.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat('\\').take(backslashes));
                backslashes = 0;
            }
        }
        if ch != '\\' {
            quoted.push(ch);
        }
    }
    quoted.extend(std::iter::repeat('\\').take(backslashes * 2));
    quoted.push('"');
    quoted
}

/// Registers the logon task that starts the host for `data` when this user
/// signs in, or removes it when `data` is `None`.
pub(super) fn service(data: Option<&Path>) -> Result<()> {
    let mut command = Command::new("schtasks.exe");
    let mut definition = None;
    let enable = data.is_some();
    if let Some(data) = data {
        let exe = wonderd::canonical_path::canonicalize(std::env::current_exe()?)?;
        // Absolute already (Paths::resolve): the task starts in System32.
        let data = wonderd::canonical_path::simplified(data);
        // schtasks reads a task definition only from a file, as UTF-16; the
        // handle is closed first (schtasks can't open a file held open here).
        let file = tempfile::Builder::new()
            .prefix("wonder-task")
            .suffix(".xml")
            .tempfile()?
            .into_temp_path();
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(
            service_task_xml(&exe, &data)?
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        std::fs::write(&file, bytes)?;
        command.args(["/Create", "/F", "/TN", SERVICE_TASK, "/XML"]);
        command.arg(&file);
        definition = Some(file);
    } else {
        command.args(["/Delete", "/F", "/TN", SERVICE_TASK]);
    }
    let output = super::process::output(&mut command)?;
    drop(definition);
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        if !enable && detail.contains("cannot find") {
            return Ok(());
        }
        return Err(format!("Task Scheduler refused the change: {}", detail.trim()).into());
    }
    Ok(())
}

/// Runs the logon task now, so the host starts in the user's interactive
/// session exactly as it does at sign-in.
pub(super) fn service_run() -> Result<()> {
    let output =
        super::process::output(Command::new("schtasks.exe").args(["/Run", "/TN", SERVICE_TASK]))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Task Scheduler could not start the host: {}", detail.trim()).into());
    }
    Ok(())
}

/// Console echo turned off for a typed secret; restored on drop. `None` when
/// stdin is not a console (piped input is read as is).
pub(super) struct StdinEcho {
    handle: HANDLE,
    mode: u32,
}

impl StdinEcho {
    pub(super) fn disable() -> Option<Self> {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, SetConsoleMode, ENABLE_ECHO_INPUT,
        };
        // SAFETY: console-mode calls on this process's own stdin handle.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0u32;
            if handle.is_null()
                || handle == INVALID_HANDLE_VALUE
                || GetConsoleMode(handle, &mut mode) == 0
            {
                return None;
            }
            SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT);
            Some(Self { handle, mode })
        }
    }
}

impl Drop for StdinEcho {
    fn drop(&mut self) {
        // SAFETY: restores the mode read from the same handle.
        unsafe { windows_sys::Win32::System::Console::SetConsoleMode(self.handle, self.mode) };
    }
}

/// The program and arguments the logon task runs, when it is installed.
/// Read from the task's XML: the `/FO LIST` labels are translated.
pub(super) fn service_action() -> Result<Option<super::windows_task::TaskAction>> {
    let output = super::process::output(Command::new("schtasks.exe").args([
        "/Query",
        "/TN",
        SERVICE_TASK,
        "/XML",
    ]))?;
    if !output.status.success() {
        return Ok(None);
    }
    let xml = super::windows_task::decode_output(&output.stdout);
    Ok(Some(
        super::windows_task::parse_task_action(&xml)
            .ok_or("Cannot read the installed login task.")?,
    ))
}

/// The logon task's command line when it is installed.
pub(super) fn service_status() -> Result<Option<String>> {
    Ok(service_action()?.map(|action| action.command_line()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The update path reads the registered task back: the XML this file writes
    /// must give the same program and arguments, whatever the path contains.
    #[test]
    fn the_login_task_definition_reads_back_as_its_command_line() {
        let exe =
            Path::new(r"C:\Users\A & B\AppData\Local\Programs\Wonder\versions\1.0.0\cupola.exe");
        let data = Path::new(r"C:\Users\A & B\AppData\Local\Wonder");
        let xml = service_task_xml(exe, data).unwrap();
        let action = super::super::windows_task::parse_task_action(&xml).unwrap();
        assert_eq!(Path::new(&action.command), exe);
        assert_eq!(
            action.arguments,
            format!(
                "start --data-dir {}",
                quote_argument(&data.display().to_string())
            )
        );
        assert_eq!(SERVICE_TASK, "Wonder Host");
    }

    #[test]
    fn pipe_is_private_to_one_data_directory_and_carries_stop() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths {
            data: directory.path().into(),
            state: directory.path().join("headless"),
        };
        let other = Paths {
            data: directory.path().into(),
            state: directory.path().join("other"),
        };
        assert_ne!(pipe_name(&paths), pipe_name(&other));
        STOPPING.store(false, Ordering::SeqCst);
        let listener = ControlListener::bind(&paths).unwrap();
        assert!(
            ControlListener::bind(&paths).is_err(),
            "first instance only"
        );
        assert!(!listener.stopping());
        // `stop` runs in another process; its flush waits for this read.
        let sender = {
            let paths = Paths {
                data: paths.data.clone(),
                state: paths.state.clone(),
            };
            std::thread::spawn(move || send_stop(&paths).is_ok())
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !listener.stopping() {
            assert!(std::time::Instant::now() < deadline, "stop not received");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(sender.join().unwrap(), "stop reported failure");
        STOPPING.store(false, Ordering::SeqCst);
    }

    /// The DACL of `path` as SDDL.
    fn dacl(path: &Path) -> String {
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
        let name = wide_path(path);
        unsafe {
            let mut descriptor = std::ptr::null_mut();
            let status = GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            );
            assert_eq!(status, 0, "{}", path.display());
            let mut text: *mut u16 = std::ptr::null_mut();
            assert_ne!(
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    std::ptr::null_mut(),
                ),
                0
            );
            let length = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sddl = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
            windows_sys::Win32::Foundation::LocalFree(text.cast());
            windows_sys::Win32::Foundation::LocalFree(descriptor);
            sddl
        }
    }

    /// Grants Everyone full control of `path` (and, for a directory, what it
    /// will contain), as a shared folder would.
    fn share_with_everyone(path: &Path) {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION,
            UNPROTECTED_DACL_SECURITY_INFORMATION,
        };
        let sddl = wide(&format!(
            "D:(A;OICI;FA;;;WD)(A;OICI;FA;;;{})",
            user_sid().unwrap()
        ));
        let name = wide_path(path);
        unsafe {
            let mut descriptor = std::ptr::null_mut();
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut()
                ),
                0
            );
            let (mut present, mut defaulted) = (0, 0);
            let mut acl = std::ptr::null_mut();
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted);
            let status = SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null(),
            );
            windows_sys::Win32::Foundation::LocalFree(descriptor);
            assert_eq!(status, 0);
        }
    }

    #[test]
    fn secrets_in_a_shared_data_directory_are_private_to_this_user() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        share_with_everyone(&shared);
        // A secret written before the directory was claimed, and a planted
        // temporary file another user could hold open.
        let earlier = shared.join("earlier");
        std::fs::write(&earlier, b"x").unwrap();
        assert!(dacl(&earlier).contains(";;;WD)"), "{}", dacl(&earlier));
        let capability = shared.join("loopback-capability");
        std::fs::write(capability.with_extension("tmp"), b"planted").unwrap();
        share_with_everyone(&capability.with_extension("tmp"));

        restrict_dir(&shared).unwrap();
        write_private(&capability, b"secret").unwrap();
        drop(append_private(&shared.join("log")).unwrap());

        let me = user_sid().unwrap();
        for path in [&shared, &earlier, &capability, &shared.join("log")] {
            let sddl = dacl(path);
            assert!(sddl.starts_with("D:P") || path != &shared, "{sddl}");
            assert!(sddl.contains(&me), "{}: {sddl}", path.display());
            for other in [";;;WD)", ";;;BU)", ";;;AU)", ";;;IU)"] {
                assert!(!sddl.contains(other), "{}: {sddl}", path.display());
            }
        }
        assert_eq!(std::fs::read(&capability).unwrap(), b"secret");
    }

    #[test]
    fn starting_an_existing_shared_installation_makes_it_private() {
        // An installation from before init secured the directory: its
        // secrets inherited a shared ACL, and an upgrade only runs `start`.
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("shared");
        let paths = Paths::at(Some(data.clone().into_os_string())).unwrap();
        std::fs::create_dir_all(&paths.state).unwrap();
        share_with_everyone(&data);
        let config = r#"{"version":1,"tunnel":"none","origin":"https://host.example","listenAddr":"127.0.0.1:3777","httpsPort":443}"#;
        std::fs::write(paths.config(), config).unwrap();
        std::fs::write(paths.capability(), b"secret").unwrap();
        assert!(dacl(&paths.capability()).contains(";;;WD)"));

        crate::prepare_start(&paths).unwrap();

        let me = user_sid().unwrap();
        for path in [&data, &paths.state, &paths.config(), &paths.capability()] {
            let sddl = dacl(path);
            assert!(sddl.contains(&me), "{}: {sddl}", path.display());
            for other in [";;;WD)", ";;;BU)", ";;;AU)", ";;;IU)"] {
                assert!(!sddl.contains(other), "{}: {sddl}", path.display());
            }
        }
        assert!(dacl(&data).starts_with("D:P"), "{}", dacl(&data));
    }

    #[test]
    fn a_relative_data_directory_names_the_same_control_pipe() {
        let directory = tempfile::tempdir().unwrap();
        let absolute = directory.path().join("data");
        let cwd = std::env::current_dir().unwrap();
        let relative = pathdiff(&absolute, &cwd);
        let from_relative = Paths::at(Some(relative.clone().into())).unwrap();
        let from_absolute = Paths::at(Some(absolute.clone().into_os_string())).unwrap();
        let dotted = Paths::at(Some(absolute.join(r"..\data\.").into_os_string())).unwrap();
        assert!(from_relative.data.is_absolute(), "{relative:?}");
        assert_eq!(pipe_name(&from_relative), pipe_name(&from_absolute));
        assert_eq!(pipe_name(&dotted), pipe_name(&from_absolute));
    }

    /// `target` relative to `base` through `..`, as a user might type it.
    fn pathdiff(target: &Path, base: &Path) -> PathBuf {
        let target: Vec<_> = target.components().collect();
        let base: Vec<_> = base.components().collect();
        let common = target.iter().zip(&base).take_while(|(a, b)| a == b).count();
        assert!(common > 0, "temp and cwd on different drives");
        let mut relative = PathBuf::from(".");
        for _ in common..base.len() {
            relative.push("..");
        }
        relative.extend(&target[common..]);
        relative
    }

    #[test]
    fn logon_task_is_scoped_to_this_user_and_escapes_its_path() {
        let xml = service_task_xml(
            Path::new(r"C:\Users\A&B\cupola.exe"),
            Path::new(r"D:\Cupola Data\"),
        )
        .unwrap();
        // Independent OS query: the task must name this real account even in
        // an SSH environment whose USERDOMAIN is the workgroup's display name.
        let identity = Command::new("whoami.exe")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .unwrap();
        assert!(identity.status.success());
        let identity = String::from_utf8_lossy(&identity.stdout);
        let sid = identity
            .trim()
            .rsplit(',')
            .next()
            .unwrap()
            .trim_matches('"');
        assert!(sid.starts_with("S-1-"), "{identity}");
        assert_eq!(xml.matches(&format!("<UserId>{sid}</UserId>")).count(), 2);
        assert!(xml.contains(r"<Command>C:\Users\A&amp;B\cupola.exe</Command>"));
        // The configured directory survives sign-in; a trailing backslash
        // must not escape the closing quote.
        assert!(
            xml.contains(r"<Arguments>start --data-dir &quot;D:\Cupola Data\\&quot;</Arguments>")
        );
    }

    #[test]
    fn task_arguments_quote_by_command_line_to_argv_rules() {
        // Pairs from Microsoft's "Parsing C++ command-line arguments" table.
        for (raw, quoted) in [
            (r"C:\a b", r#""C:\a b""#),
            (r"C:\", r#""C:\\""#),
            (r#"a"b"#, r#""a\"b""#),
            (r#"a\\"b"#, r#""a\\\\\"b""#),
            (r"a\\b", r#""a\\b""#),
        ] {
            assert_eq!(quote_argument(raw), quoted, "{raw}");
        }
    }

    #[test]
    fn pathext_candidates_find_npm_shims() {
        let names = executable_candidates("codex");
        assert!(names.iter().any(|n| n == "codex.exe"));
        assert!(names.iter().any(|n| n == "codex.cmd"));
        assert_eq!(executable_candidates("node.exe")[0], "node.exe");
    }
}
