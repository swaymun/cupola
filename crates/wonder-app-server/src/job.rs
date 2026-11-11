//! EXPERIMENTAL Windows process ownership: a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. A process assigned to the job and
//! every process it later starts (unless it breaks away explicitly) ends when
//! `terminate` is called or the last job handle closes, including when the
//! owner crashes. This is the Windows counterpart of a Unix process group.
//!
//! A child assigned after it started can, in principle, start a grandchild
//! before assignment; Wonder's runtimes start their agents only after the
//! JSON-RPC handshake, which follows assignment.
use std::io;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

#[derive(Debug)]
pub struct Job(HANDLE);

// SAFETY: a job handle may be used and closed from any thread.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    /// A new anonymous job that kills its processes when its last handle closes.
    pub fn kill_on_close() -> io::Result<Self> {
        // SAFETY: creates and configures a handle owned by the returned value.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Job(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    /// Adds a running process (by its process handle) to the job.
    pub fn assign(&self, process: std::os::windows::io::RawHandle) -> io::Result<()> {
        // SAFETY: both handles are valid for the duration of the call.
        if unsafe { AssignProcessToJobObject(self.0, process as HANDLE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Adds a process spawned with [`CREATE_SUSPENDED`] to the job, then lets
    /// it run, so it cannot start anything outside the job first. On error the
    /// process has run none of its own code; the caller must kill it.
    pub fn adopt_suspended(
        &self,
        process: std::os::windows::io::RawHandle,
        pid: u32,
    ) -> io::Result<()> {
        self.assign(process)?;
        resume_threads(pid)
    }

    /// Ends every process in the job.
    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: valid job handle.
        if unsafe { TerminateJobObject(self.0, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Creation flag for children handed to [`Job::adopt_suspended`].
pub const CREATE_SUSPENDED: u32 = windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

/// Resumes the (single, primary) thread of a process created suspended.
fn resume_threads(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    // SAFETY: the snapshot and thread handles are owned here and closed once.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    if ResumeThread(thread) != u32::MAX {
                        resumed += 1;
                    }
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        if resumed == 0 {
            return Err(io::Error::other(
                "no thread of the suspended process could be resumed",
            ));
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once; KILL_ON_JOB_CLOSE ends the tree.
        unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;

    #[test]
    fn closing_the_job_ends_the_tree() {
        let job = Job::kill_on_close().unwrap();
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .spawn()
            .unwrap();
        job.assign(child.as_raw_handle()).unwrap();
        let started = std::time::Instant::now();
        drop(job);
        child.wait().unwrap();
        // ping alone would run ~29 s. (Kill-on-close may report exit code 0.)
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn a_suspended_child_runs_only_after_adoption() {
        use std::os::windows::process::CommandExt;
        let job = Job::kill_on_close().unwrap();
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 7"])
            .creation_flags(CREATE_SUSPENDED)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(child.try_wait().unwrap().is_none(), "ran before adoption");
        job.adopt_suspended(child.as_raw_handle(), child.id())
            .unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(7));
    }
}
