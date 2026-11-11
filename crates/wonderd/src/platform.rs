//! Small operating-system seams shared by the daemon: file identity stamps,
//! owner-only files and folders, process liveness and signals, the host name.
//!
//! Unix keeps the exact behavior the daemon always had. Windows support is
//! EXPERIMENTAL (branch `experimental/windows-host`); each Windows choice is
//! documented where it differs.
use std::{fs, io, path::Path};

/// Identity and change stamps of a file, for caches and revision tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileStamp {
    pub device: u64,
    pub inode: u64,
    /// Content modification time (seconds, nanoseconds).
    pub modified: (i64, i64),
    /// Metadata change time (seconds, nanoseconds).
    pub changed: (i64, i64),
}

#[cfg(unix)]
pub(crate) fn file_stamp(metadata: &fs::Metadata) -> FileStamp {
    use std::os::unix::fs::MetadataExt;
    FileStamp {
        device: metadata.dev(),
        inode: metadata.ino(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    }
}

/// Windows: std exposes neither the volume serial nor the file index on
/// stable Rust, so the creation time stands in for the inode and the last
/// write time for the change time. NTFS name tunnelling gives a file renamed
/// over an existing name that name's creation time (measured in the test VM),
/// so a replacement is visible only through write time (clock-tick
/// resolution, ~15.6 ms) and length, which callers compare too.
#[cfg(windows)]
pub(crate) fn file_stamp(metadata: &fs::Metadata) -> FileStamp {
    use std::os::windows::fs::MetadataExt;
    let split = |ticks: u64| {
        // 100 ns ticks since 1601-01-01.
        let ticks = ticks as i64;
        (ticks / 10_000_000, (ticks % 10_000_000) * 100)
    };
    FileStamp {
        device: 0,
        inode: metadata.creation_time(),
        modified: split(metadata.last_write_time()),
        changed: split(metadata.last_write_time()),
    }
}

/// Creates `path` (and missing parents when `recursive`) readable only by
/// the owner. On Windows, folders under the user profile already inherit an
/// owner-only ACL (`%LOCALAPPDATA%`), so this is a plain create.
pub(crate) fn create_private_dir(path: &Path, recursive: bool) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Restricts an existing folder to its owner (Unix mode 0700).
pub(crate) fn restrict_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
}

/// Opens a new file that only the owner can read (Unix mode 0600); fails if
/// it exists.
pub(crate) fn create_private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Opens an existing file for reading without following a final symlink and
/// without blocking on a FIFO.
pub(crate) fn open_regular_nofollow(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(path)?;
        use std::os::windows::fs::MetadataExt;
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other("refusing a reparse point"));
        }
        return Ok(file);
    }
    #[allow(unreachable_code)]
    options.open(path)
}

/// Whether a process with this ID exists (a process we may not signal still
/// counts).
#[cfg(unix)]
pub(crate) fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // Signal 0 checks existence only.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub(crate) fn process_alive(pid: i32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: plain handle query; the handle is closed below.
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid as u32,
        );
        if handle.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let alive = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
        CloseHandle(handle);
        alive
    }
}

/// A request to end a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Signal {
    /// Ctrl-C (SIGINT). Unsupported for another console on Windows.
    Interrupt,
    /// SIGTERM; TerminateProcess on Windows.
    Terminate,
}

/// Sends `signal` to one process. False when it could not be delivered,
/// including an unsupported signal on this platform.
#[cfg(unix)]
pub(crate) fn signal(pid: i32, signal: Signal) -> bool {
    if pid <= 0 {
        return false;
    }
    let number = match signal {
        Signal::Interrupt => libc::SIGINT,
        Signal::Terminate => libc::SIGTERM,
    };
    unsafe { libc::kill(pid, number) == 0 }
}

#[cfg(windows)]
pub(crate) fn signal(pid: i32, signal: Signal) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    if pid <= 0 || signal == Signal::Interrupt {
        return false;
    }
    // SAFETY: the handle is closed below.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
        if handle.is_null() {
            return false;
        }
        let ok = TerminateProcess(handle, 1) != 0;
        CloseHandle(handle);
        ok
    }
}

/// This computer's network host name.
#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..end]).trim().to_owned()).filter(|name| !name.is_empty())
}

#[cfg(windows)]
pub(crate) fn host_name() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_tracks_replacement_and_private_paths_are_created() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        create_private_dir(&nested, true).unwrap();
        restrict_dir(&nested).unwrap();
        let file = nested.join("x");
        std::io::Write::write_all(&mut create_private_file(&file).unwrap(), b"one").unwrap();
        assert!(
            create_private_file(&file).is_err(),
            "create_new refuses an existing file"
        );
        let first = file_stamp(&fs::metadata(&file).unwrap());
        assert_eq!(first, file_stamp(&fs::metadata(&file).unwrap()));
        // Windows file times advance with the system clock tick (~15.6 ms).
        #[cfg(windows)]
        std::thread::sleep(std::time::Duration::from_millis(50));
        let replacement = nested.join("y");
        fs::write(&replacement, b"two!").unwrap();
        fs::rename(&replacement, &file).unwrap();
        let second = file_stamp(&fs::metadata(&file).unwrap());
        // Unix: a new inode. Windows: NTFS name tunnelling keeps the replaced
        // name's creation time (measured), so only the write time tells; callers
        // also compare length.
        #[cfg(unix)]
        assert_ne!((first.device, first.inode), (second.device, second.inode));
        #[cfg(windows)]
        assert_ne!(first, second);
        assert!(open_regular_nofollow(&file).is_ok());
    }

    #[test]
    fn own_process_is_alive_and_host_has_a_name() {
        assert!(process_alive(std::process::id() as i32));
        assert!(!process_alive(0));
        assert!(host_name().is_some());
    }
}
