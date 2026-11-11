//! Git actions on the repositories a Project thread worked with: commit, push,
//! pull (fast-forward only), open a pull request, merge it and turn auto-merge
//! on or off. They run on the Mac with its own `git` and `gh` sign-in; Wonder
//! adds no credential and never sees one.
//!
//! Only a repository wholly inside one of the Project's folders, named by the
//! thread's repository index, is written. Every command is an argument vector
//! (no shell), bounded in time and output, with a reduced environment, hooks
//! and fsmonitor turned off, and no prompts. Git (and gh's Git) runs in a
//! write view that never loads the repository's own config files: the data
//! settings an action needs are copied from one snapshot, and a repository
//! whose result depends on its own program settings is refused from that same
//! snapshot. One action runs per repository at a time, none while the
//! thread's agent is working, and failures are explained in plain words.

use super::repo_records;
use super::thread_repos;
use super::*;
use serde_json::Value;
use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;

const VERSION: u32 = 1;
/// Local Git work: staging, committing, reading refs.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Talking to a remote or GitHub.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(90);
const OUTPUT_LIMIT: u64 = 256 * 1024;
const MAX_MESSAGE: usize = 10_000;
const MAX_TITLE: usize = 256;
const MAX_BODY: usize = 60_000;
/// Paths checked against .gitignore per `git check-ignore` call.
const IGNORE_CHUNK: usize = 500;
/// Newly staged files checked against .gitignore at most.
const MAX_IGNORE_CHECK: usize = 20_000;
const PR_VIEW_FIELDS: &str = "number,title,url,state,isDraft,mergeable,mergeStateStatus,statusCheckRollup,autoMergeRequest,headRefName,baseRefName,headRefOid";
const REPO_VIEW_FIELDS: &str =
    "squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed,viewerDefaultMergeMethod";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ActionRequest {
    repository_id: String,
    root: String,
    #[serde(default)]
    repo: String,
    action: String,
    message: Option<String>,
    include_unstaged: Option<bool>,
    confirm_default_branch: Option<bool>,
    title: Option<String>,
    body: Option<String>,
    base: Option<String>,
    draft: Option<bool>,
    pull_request_url: Option<String>,
    method: Option<String>,
    enabled: Option<bool>,
    /// Merge only while the pull request's head is still this commit.
    expected_head_oid: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Commit,
    Push,
    Pull,
    CreatePullRequest,
    Merge,
    AutoMerge,
}

impl Kind {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "commit" => Self::Commit,
            "push" => Self::Push,
            "pull" => Self::Pull,
            "createPullRequest" => Self::CreatePullRequest,
            "merge" => Self::Merge,
            "autoMerge" => Self::AutoMerge,
            _ => return None,
        })
    }
    fn name(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Push => "push",
            Self::Pull => "pull",
            Self::CreatePullRequest => "createPullRequest",
            Self::Merge => "merge",
            Self::AutoMerge => "autoMerge",
        }
    }
    fn needs_gh(self) -> bool {
        matches!(
            self,
            Self::CreatePullRequest | Self::Merge | Self::AutoMerge
        )
    }
    /// Git itself writes this repository (the rest only ask GitHub).
    fn writes_locally(self) -> bool {
        !self.needs_gh()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreatedPull {
    number: u64,
    url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Outcome {
    version: u32,
    action: &'static str,
    ok: bool,
    reason: Option<&'static str>,
    message: String,
    commit: Option<String>,
    pull_request: Option<CreatedPull>,
}

impl Outcome {
    fn done(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            version: VERSION,
            action: kind.name(),
            ok: true,
            reason: None,
            message: message.into(),
            commit: None,
            pull_request: None,
        }
    }
    fn refused(kind: Kind, reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: Some(reason),
            ..Self::done(kind, message)
        }
    }
    fn response(self) -> Response {
        let status = if self.ok {
            StatusCode::OK
        } else {
            StatusCode::CONFLICT
        };
        (status, Json(self)).into_response()
    }
}

type Refusal = (&'static str, String);

fn refuse<T>(reason: &'static str, message: impl Into<String>) -> Result<T, Refusal> {
    Err((reason, message.into()))
}

/// The `gh` an action may use, or why there is none.
#[derive(Clone, Debug)]
pub(super) enum Gh {
    Missing,
    SignedOut,
    Ready(PathBuf),
}

impl Gh {
    fn ready(&self) -> Result<&Path, Refusal> {
        match self {
            Self::Ready(gh) => Ok(gh),
            Self::Missing => refuse(
                "ghMissing",
                "Install the GitHub CLI (gh) on your computer to work with pull requests.",
            ),
            Self::SignedOut => refuse(
                "ghSignedOut",
                "Sign in to the GitHub CLI on your computer (gh auth login), then try again.",
            ),
        }
    }
}

// -------------------------------------------------------------------------
// Process boundary
// -------------------------------------------------------------------------

#[derive(Debug)]
pub(super) struct Run {
    pub(super) success: bool,
    stdout: String,
    stderr: String,
}

impl Run {
    fn text(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// What the Mac's own Git needs from the environment to find its settings.
const GIT_ENVIRONMENT: &[&str] = &["HOME", "USER", "LOGNAME", "TMPDIR", "XDG_CONFIG_HOME"];
/// What a network action also needs to sign in as the owner.
const SIGN_IN_ENVIRONMENT: &[&str] = &["SSH_AUTH_SOCK", "GH_CONFIG_DIR"];

/// Replaces a child's environment with the few variables Git or gh needs, so
/// nothing else Cupola's host holds (its loopback capability, tunnel and
/// provider keys, tokens) reaches a program the repository can influence.
/// `signed_in` also keeps the owner's SSH agent and gh settings, for actions
/// that reach a remote.
fn reduce_environment(command: &mut std::process::Command, signed_in: bool) {
    command.env_clear();
    let signing = if signed_in { SIGN_IN_ENVIRONMENT } else { &[] };
    for key in GIT_ENVIRONMENT.iter().chain(signing) {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("PATH", "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .env("GCM_INTERACTIVE", "never")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env("NO_COLOR", "1");
}

/// Runs a program with an argument vector, never a shell, in the repository's
/// write view and a reduced environment that keeps only what the Mac's own git
/// and gh sign-in need.
async fn run(
    program: &Path,
    view: &WriteView,
    args: &[&str],
    timeout: Duration,
) -> Result<Run, Refusal> {
    run_writing(program, view, args, timeout, &[]).await
}

/// `run`, where Git may also write beneath `also` (a local push destination).
/// Git always runs confined (see `confined_git`); gh runs only network
/// commands and is not.
async fn run_writing(
    program: &Path,
    view: &WriteView,
    args: &[&str],
    timeout: Duration,
    also: &[PathBuf],
) -> Result<Run, Refusal> {
    let (mut command, _confined) = if program == Path::new("git") {
        let confined = confined_git(view, also).map_err(|_| unconfined())?;
        (confined.command, Some(confined.guard))
    } else {
        (tokio::process::Command::new(program), None)
    };
    command.args(args).current_dir(&view.top);
    reduce_environment(command.as_std_mut(), true);
    command
        .envs(view.env.iter().map(|(key, value)| (key, value)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| {
        (
            "failed",
            format!("{} could not start on your computer.", tool(program)),
        )
    })?;
    let stdout = child.stdout.take().ok_or(("failed", "No output.".into()))?;
    let stderr = child.stderr.take().ok_or(("failed", "No output.".into()))?;
    let read = |stream: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>| async move {
        let mut bytes = Vec::new();
        stream
            .take(OUTPUT_LIMIT)
            .read_to_end(&mut bytes)
            .await
            .map(|_| bytes)
    };
    let result = tokio::time::timeout(timeout, async {
        tokio::join!(read(Box::pin(stdout)), read(Box::pin(stderr)), child.wait())
    })
    .await;
    match result {
        Ok((Ok(out), Ok(err), Ok(status))) => Ok(Run {
            success: status.success(),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        }),
        Ok(_) => refuse("failed", format!("{} stopped unexpectedly.", tool(program))),
        Err(_) => {
            let _ = child.kill().await;
            refuse(
                "timeout",
                format!(
                    "{} took longer than {} seconds on your computer and was stopped.",
                    tool(program),
                    timeout.as_secs()
                ),
            )
        }
    }
}

fn tool(program: &Path) -> &'static str {
    if program.file_name().is_some_and(|name| name == "gh") {
        "The GitHub CLI"
    } else {
        "Git"
    }
}

/// Git for an action, in the repository's write view.
async fn git(view: &WriteView, args: &[&str], timeout: Duration) -> Result<Run, Refusal> {
    run(Path::new("git"), view, args, timeout).await
}

// -------------------------------------------------------------------------
// Write confinement
// -------------------------------------------------------------------------

/// Path checks alone can't hold Git to one repository: a `.git`, `index` or
/// `FETCH_HEAD` replaced with a link after any check is followed. So the
/// operating system holds every Git process an action or checkpoint starts,
/// and every program Git starts in turn, to writing beneath the view's
/// `writable` folders: the checked repository, Cupola's private view and
/// temporary folders, and the owner's GnuPG folder for signing. A write
/// anywhere else, through any link, is denied. On macOS this is a Seatbelt
/// profile; on Linux, Landlock. Where neither is available, nothing is
/// written.
struct Confined {
    command: tokio::process::Command,
    guard: Guard,
}

#[cfg(target_os = "linux")]
type Guard = std::os::fd::OwnedFd;
/// Seatbelt holds nothing open for the process.
#[cfg(not(target_os = "linux"))]
struct Guard;

#[cfg(test)]
thread_local! {
    /// Lets a test take the path of a computer that can't confine Git.
    pub(super) static FORCED_UNCONFINED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

fn unconfined() -> Refusal {
    (
        "unavailable",
        "Cupola can't keep Git's changes inside this Project's folder on this computer, so it won't run Git actions here. Use Git on your computer.".to_owned(),
    )
}

/// Whether this computer can confine Git's writes.
pub(super) fn can_confine() -> bool {
    #[cfg(test)]
    if FORCED_UNCONFINED.with(std::cell::Cell::get) {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        Path::new(SANDBOX_EXEC).is_file()
    }
    #[cfg(target_os = "linux")]
    {
        landlock::abi() >= 2
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// Devices Git and the programs it starts write to.
const DEVICES: &[&str] = &["/dev/null", "/dev/zero", "/dev/tty", "/dev/dtracehelper"];

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Git, confined to writing beneath the view's folders and `also`.
fn confined_git(view: &WriteView, also: &[PathBuf]) -> io::Result<Confined> {
    if !can_confine() {
        return Err(io::Error::other("no confinement"));
    }
    #[cfg(target_os = "macos")]
    {
        let writable = view.writable.iter().chain(also);
        // The last matching rule wins, and Seatbelt matches the path a write
        // really reaches, after every link.
        let quote = |path: &str| path.replace('\\', "\\\\").replace('"', "\\\"");
        let mut profile =
            String::from("(version 1)(allow default)(deny file-write*)(allow file-write*");
        for folder in writable {
            let folder = folder.to_str().ok_or_else(|| io::Error::other("path"))?;
            profile.push_str(&format!(" (subpath \"{}\")", quote(folder)));
        }
        for device in DEVICES {
            profile.push_str(&format!(" (literal \"{device}\")"));
        }
        profile.push(')');
        let mut command = tokio::process::Command::new(SANDBOX_EXEC);
        command.arg("-p").arg(profile).arg("git");
        Ok(Confined {
            command,
            guard: Guard,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let also = also
            .iter()
            .map(|folder| landlock::anchor(folder))
            .collect::<io::Result<Vec<_>>>()?;
        let ruleset = landlock::ruleset(view.anchors.iter().chain(&also), DEVICES)?;
        let raw = std::os::fd::AsRawFd::as_raw_fd(&ruleset);
        let mut command = tokio::process::Command::new("git");
        // SAFETY: the closure runs between fork and exec and makes only two
        // async-signal-safe system calls, allocating nothing.
        unsafe {
            command.pre_exec(move || landlock::restrict(raw));
        }
        Ok(Confined {
            command,
            guard: ruleset,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (view, also);
        Err(io::Error::other("no confinement"))
    }
}

/// Landlock, through its three system calls. Reads and program starts stay
/// open; every kind of write is handled, so only the rules' folders allow it.
#[cfg(target_os = "linux")]
mod landlock {
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;
    const WRITE_FILE: u64 = 1 << 1;
    /// REMOVE_DIR through MAKE_SYM: removing, and making every kind of entry.
    const CHANGE_ENTRIES: u64 = 0b1_1111_1111 << 4;
    const REFER: u64 = 1 << 13;
    const TRUNCATE: u64 = 1 << 14;

    /// The kernel's Landlock version; below 1 when it has none. Version 2
    /// first lets a file move between folders (Git's lock files do).
    pub(super) fn abi() -> i64 {
        // SAFETY: asks only for the version; no memory is passed.
        unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        }
    }

    fn handled(abi: i64) -> u64 {
        let mut access = WRITE_FILE | CHANGE_ENTRIES | REFER;
        if abi >= 3 {
            access |= TRUNCATE;
        }
        access
    }

    fn check(result: libc::c_long) -> io::Result<libc::c_long> {
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    }

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    const RESOLVE_NO_SYMLINKS: u64 = 0x04;

    /// Binds a canonical folder as it is now: a rule made from the returned
    /// descriptor follows this folder, never what its path names later. Any
    /// link along the path (one swapped in since it was checked) fails.
    pub(super) fn anchor(folder: &Path) -> io::Result<OwnedFd> {
        let name = CString::new(folder.as_os_str().as_bytes())?;
        let how = OpenHow {
            flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: RESOLVE_NO_SYMLINKS,
        };
        // SAFETY: a valid C string and `how` outlive the call, whose size is
        // passed with it.
        let fd = check(unsafe {
            libc::syscall(
                libc::SYS_openat2,
                libc::AT_FDCWD,
                name.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        })?;
        // SAFETY: a new descriptor this call owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }

    /// A ruleset allowing writes beneath the anchored `folders` and to the
    /// device `files`; a device missing here is left out.
    pub(super) fn ruleset<'a>(
        folders: impl Iterator<Item = &'a OwnedFd>,
        files: &[&str],
    ) -> io::Result<OwnedFd> {
        let abi = abi();
        if abi < 2 {
            return Err(io::Error::other("Landlock is unavailable"));
        }
        let access = handled(abi);
        let attr = RulesetAttr {
            handled_access_fs: access,
            handled_access_net: 0,
            scoped: 0,
        };
        // SAFETY: `attr` outlives the call and its size is passed with it;
        // a kernel with a shorter struct accepts the zeroed remainder.
        let fd = check(unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        })?;
        // SAFETY: a new descriptor this call owns.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        let mut devices = Vec::new();
        for file in files {
            let name = CString::new(Path::new(file).as_os_str().as_bytes())?;
            // SAFETY: a valid C string; the descriptor is owned below.
            let fd = unsafe { libc::open(name.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if fd < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::NotFound {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: as above.
            devices.push(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let rules = folders.map(|folder| (folder, access)).chain(
            devices
                .iter()
                .map(|file| (file, WRITE_FILE | (access & TRUNCATE))),
        );
        for (parent, allowed) in rules {
            let beneath = PathBeneath {
                allowed_access: allowed,
                parent_fd: std::os::fd::AsRawFd::as_raw_fd(parent),
            };
            // SAFETY: both descriptors are open and `beneath` outlives the call.
            check(unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    std::os::fd::AsRawFd::as_raw_fd(&ruleset),
                    RULE_PATH_BENEATH,
                    &beneath as *const PathBeneath,
                    0u32,
                )
            })?;
        }
        Ok(ruleset)
    }

    /// Applies `ruleset` to the calling process, for good. Called between
    /// fork and exec: only async-signal-safe calls.
    pub(super) fn restrict(ruleset: RawFd) -> io::Result<()> {
        // SAFETY: plain system calls with integer arguments.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

impl WriteView {
    /// Copy-back may wait for another Git process and copy a large reflog.
    /// Finish it on a blocking worker and report retained recovery data.
    pub(super) async fn finish(self) -> Result<(), Refusal> {
        let recovery = self._private.common.path().to_path_buf();
        let result = tokio::task::spawn_blocking(move || {
            let mut view = self;
            view._private.finish()
        })
        .await;
        if matches!(result, Ok(Ok(()))) {
            Ok(())
        } else {
            refuse(
                "unavailable",
                format!("Git may already have finished, but Cupola couldn't save its recovery history. A copy is safe at {} on your computer. Recover that history before repeating the action.", recovery.display()),
            )
        }
    }

    /// The checked Git directory.
    pub(super) fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The view for a thread checkpoint (`thread_changes`): Git stages into
    /// `index`, a private file, never the repository's own index, and commits
    /// as Cupola so no identity or signing setting is needed.
    pub(super) fn for_checkpoint(mut self, index: &Path) -> Self {
        self.env
            .push(("GIT_INDEX_FILE".into(), index.as_os_str().to_owned()));
        // The private index's folder is Cupola's own.
        if let Some(folder) = index
            .parent()
            .and_then(|folder| crate::canonical_path::canonicalize(folder).ok())
        {
            #[cfg(target_os = "linux")]
            self.anchors.extend(landlock::anchor(&folder).ok());
            self.writable.push(folder);
        }
        for key in ["GIT_AUTHOR", "GIT_COMMITTER"] {
            self.env.push((format!("{key}_NAME"), "Cupola".into()));
            self.env
                .push((format!("{key}_EMAIL"), "checkpoints@cupola.invalid".into()));
        }
        self
    }
}

/// One checkpoint step, trimmed output; None on failure.
pub(super) async fn checkpoint_git(view: &WriteView, args: &[&str]) -> Option<String> {
    let run = git(view, args, LOCAL_TIMEOUT).await.ok()?;
    run.success.then(|| run.stdout.trim().to_owned())
}

/// A small read in the write view, trimmed; None on failure. Reads made for an
/// action see the same settings snapshot as its writes.
async fn read_line(view: &WriteView, args: &[&str]) -> Option<String> {
    let read = git(view, args, LOCAL_TIMEOUT).await.ok()?;
    let line = read.stdout.trim();
    (read.success && !line.is_empty()).then(|| line.to_owned())
}

// -------------------------------------------------------------------------
// Write view
// -------------------------------------------------------------------------

/// Fixed settings for every process an action starts, including gh's Git.
/// They outrank the copied repository settings and the owner's config.
const HARDENING: &[(&str, &str)] = &[
    ("core.hooksPath", "/dev/null"),
    ("core.fsmonitor", "false"),
    ("core.editor", "true"),
    ("core.pager", "cat"),
    ("protocol.ext.allow", "never"),
    // A submodule's own config is never checked, so never run Git in one.
    ("submodule.recurse", "false"),
    ("fetch.recurseSubmodules", "false"),
    ("push.recurseSubmodules", "no"),
    // No action needs repacking or pruning in the background.
    ("gc.auto", "0"),
    ("maintenance.auto", "false"),
];

/// Command-scope settings for `HARDENING`, as the environment Git reads them
/// from. Git's local receivers and submodule commands clear them along with
/// the view's repository variables.
fn hardening_environment() -> Vec<(String, OsString)> {
    let mut env = Vec::with_capacity(HARDENING.len() * 2 + 1);
    for (index, (key, value)) in HARDENING.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{index}"), key.into()));
        env.push((format!("GIT_CONFIG_VALUE_{index}"), value.into()));
    }
    env.push((
        "GIT_CONFIG_COUNT".into(),
        HARDENING.len().to_string().into(),
    ));
    env
}

/// Git for a folder the repository's own settings and hooks must not steer,
/// for the few local commands that run outside a write view (a worktree is
/// made beside the repository, which no view may write). It takes the same
/// reduced environment and fixed settings as an action, without the SSH agent
/// or gh sign-in, and never fetches missing objects from a promisor remote.
/// `GIT_NO_LAZY_FETCH` is ignored before Git 2.45, so no transport is allowed
/// either: a lazy fetch could otherwise run the repository's `core.sshCommand`
/// or `remote.*.uploadpack` program. None of these commands reaches a remote.
pub(crate) fn repository_git(program: &Path, folder: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.arg("-C").arg(folder);
    reduce_environment(&mut command, false);
    command
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "none")
        .envs(hardening_environment())
        .stdin(std::process::Stdio::null());
    command
}

/// A repository-local setting that names a program Git would run to check a
/// file out: a content filter's `clean`, `smudge` or `process` command.
/// Hooks and fsmonitor are fixed off by `HARDENING`; filters can't be, since
/// switching one off changes the bytes written, so they are refused instead.
fn checkout_program(key: &str, value: &[u8]) -> bool {
    let key = key.to_ascii_lowercase();
    !value.is_empty()
        && key.starts_with("filter.")
        && [".clean", ".smudge", ".process"]
            .iter()
            .any(|name| key.ends_with(name))
}

/// Refuses a repository whose own settings (not the owner's global ones)
/// name a content filter, which a checkout would run. The message names the
/// setting, not its value, which may hold a path or secret.
pub(crate) fn refuse_repository_programs(program: &Path, folder: &Path) -> Result<(), String> {
    let output = repository_git(program, folder)
        .args(["config", "--list", "-z", "--show-scope"])
        .output()
        .map_err(|_| "Git isn’t available on your computer.".to_owned())?;
    if !output.status.success() {
        return Err("Cupola couldn’t read this repository’s Git settings.".to_owned());
    }
    // With `-z --show-scope` every setting is `scope NUL key NL value NUL`.
    let mut fields = output.stdout.split(|b| *b == 0);
    while let (Some(scope), Some(entry)) = (fields.next(), fields.next()) {
        let mut parts = entry.splitn(2, |b| *b == b'\n');
        let key = String::from_utf8_lossy(parts.next().unwrap_or_default());
        let value = parts.next().unwrap_or_default();
        if matches!(scope, b"local" | b"worktree") && checkout_program(&key, value) {
            return Err(format!(
                "This repository sets a Git filter ({}) that would run a program on your computer. Use Git on your computer to make the worktree.",
                key.to_ascii_lowercase()
            ));
        }
    }
    Ok(())
}

/// Entries Git reads and writes live through the private Git directory. Git
/// only ever creates new files in them or renames a new file over an old one
/// (refs, packed-refs and shallow through lock files), so a name linked to
/// another repository's file is replaced, never written through; `lfs`
/// keeps the owner's LFS filter storing objects in the repository. `config`,
/// `hooks` and `worktrees` are never reached.
const LIVE_DIRECTORIES: &[&str] = &["refs", "info", "objects", "lfs"];
const LIVE_FILES: &[&str] = &["packed-refs", "shallow"];

/// Files at a Git directory's top that Git writes in place, where a name
/// linked to another repository's file would carry the write there. They
/// live only in the private Git directory: those an action reads are copied
/// in, and all of them are carried back afterwards (see `carry`). Reflogs,
/// which Git appends to in place, start empty there and their new entries
/// are carried back the same way.
const READ_STATE: &[&str] = &[
    "HEAD",
    "MERGE_HEAD",
    "MERGE_MSG",
    "MERGE_MODE",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "SQUASH_MSG",
    "AUTO_MERGE",
];
const WRITTEN_STATE: &[&str] = &["ORIG_HEAD", "FETCH_HEAD", "COMMIT_EDITMSG"];
const STATE_LIMIT: u64 = 1024 * 1024;
/// A larger reflog keeps the entries it had.
const REFLOG_LIMIT: u64 = 64 * 1024 * 1024;
/// Reflogs one action writes, at most.
const REFLOG_FILES: usize = 1024;

/// How a private view reaches the repository's live entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reach {
    /// Every entry is a symbolic link; lock files follow them.
    Links,
    /// For a computer that can't make a symbolic link (Windows without
    /// Developer Mode): directories are junctions and the two files are
    /// private copies. Cupola holds Git's own lock on each for the action,
    /// so no other Git process rewrites it meanwhile, and writes a changed
    /// copy back through that lock.
    Copies,
}

/// Best first. Where no kind of link can be made, the action is refused.
const REACHES: &[Reach] = &[Reach::Links, Reach::Copies];

#[cfg(test)]
thread_local! {
    /// Lets a test take the path of a computer without symbolic links, or
    /// without any private view.
    pub(super) static FORCED_REACHES: std::cell::Cell<Option<&'static [Reach]>> =
        const { std::cell::Cell::new(None) };
}

fn reaches() -> &'static [Reach] {
    #[cfg(test)]
    if let Some(forced) = FORCED_REACHES.with(std::cell::Cell::get) {
        return forced;
    }
    REACHES
}

/// How long to wait for a lock another Git process holds, as Git does.
const LOCK_WAIT: Duration = Duration::from_secs(1);
/// Copies are bounded: shallow boundaries as reads bound them, and refs far
/// beyond any real repository's.
const SHALLOW_LIMIT: u64 = 1024 * 1024;
const PACKED_REFS_LIMIT: u64 = 256 * 1024 * 1024;

/// The repository as an action's Git, and gh's Git, sees it: its real work
/// tree, index, objects and refs, in a private Git directory made for this
/// one action (see `Private`), so none of the repository's own config files
/// and none of the files Git writes in place are reached. The data settings
/// an action needs are copied into a private config file from one snapshot,
/// taken when the view is prepared; a later change to the repository's
/// config is never seen. The owner's own user and system config still apply
/// (their identity, signing program, sign-in helpers, global ignores and URL
/// rewrites), so attributes can select only the drivers that the owner's
/// config defines.
pub(super) struct WriteView {
    top: PathBuf,
    env: Vec<(String, OsString)>,
    /// The checked Git directory, which holds the repository's own index.
    git_dir: PathBuf,
    /// The repository's own config file, where a new branch's tracking goes.
    config: PathBuf,
    /// The only folders Git may write beneath (see `confined_git`).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    writable: Vec<PathBuf>,
    /// The same folders, bound when they were checked.
    #[cfg(target_os = "linux")]
    anchors: Vec<std::os::fd::OwnedFd>,
    /// Git's own temporary files, in place of the computer's.
    _temporary: tempfile::TempDir,
    _private: Private,
}

/// A daemon-owned Git directory, new for each action, that Git uses as both
/// its Git and common directory. It reaches the repository's live entries
/// and holds fresh copies of the rest, so a file in the repository's `.git`
/// linked (even hard-linked) elsewhere never takes one of Git's in-place
/// writes. Dropping it carries Git's changes back, releases held locks and
/// removes each link itself before the directory, so nothing is ever
/// deleted through a link.
struct Private {
    links: Vec<PathBuf>,
    held: Vec<Held>,
    /// The repository's own Git and common directories, canonical.
    git_dir: PathBuf,
    common_dir: PathBuf,
    /// Each state file as it was copied in; None when absent.
    state: Vec<(&'static str, Option<Vec<u8>>)>,
    /// Explicit finalization and cancellation teardown must never append twice.
    finished: bool,
    common: tempfile::TempDir,
}

#[derive(Debug)]
enum Build {
    /// This computer can't make this kind of link: try the next kind.
    Unlinkable,
    /// Another Git process holds this lock.
    Busy(PathBuf),
    /// Anything else, such as a repository file that can't be copied safely:
    /// refused rather than taking a weaker path.
    Failed,
}

impl Private {
    async fn build(
        layout: &workspace_git::GitLayout,
        config: &str,
        reach: Reach,
    ) -> Result<Self, Build> {
        let common =
            workspace_git::private_git_directory("wonder-git-write-").map_err(|_| Build::Failed)?;
        let mut private = Self {
            links: Vec::new(),
            held: Vec::new(),
            git_dir: PathBuf::from(&layout.git_dir),
            common_dir: PathBuf::from(&layout.common_dir),
            state: Vec::new(),
            finished: false,
            common,
        };
        let path = private.common.path().to_path_buf();
        // Written before Git starts: if copy-back cannot finish, this directory
        // survives with its destinations and only the still-pending reflogs.
        let recovery = serde_json::json!({
            "gitDirectory": private.git_dir,
            "commonDirectory": private.common_dir,
            "instructions": "Pending logs/HEAD belongs to gitDirectory; other logs belong to commonDirectory. Merge these entries with the existing recovery history while holding each Git ref lock; do not replace newer history. Other files hold the action's private Git state."
        });
        tokio::fs::write(path.join("RECOVERY.json"), recovery.to_string())
            .await
            .map_err(|_| Build::Failed)?;
        tokio::fs::write(path.join("config"), config)
            .await
            .map_err(|_| Build::Failed)?;
        tokio::fs::create_dir(path.join("logs"))
            .await
            .map_err(|_| Build::Failed)?;
        // Read through the checked folder, never a link swapped in since.
        let own = carry::folder(&private.git_dir).map_err(|_| Build::Failed)?;
        for name in READ_STATE {
            let bytes = carry::read(&own, name, STATE_LIMIT).map_err(|_| Build::Failed)?;
            if let Some(bytes) = &bytes {
                tokio::fs::write(path.join(name), bytes)
                    .await
                    .map_err(|_| Build::Failed)?;
            } else if *name == "HEAD" {
                return Err(Build::Failed);
            }
            private.state.push((name, bytes));
        }
        private
            .state
            .extend(WRITTEN_STATE.iter().map(|name| (*name, None)));
        let real = Path::new(&layout.common_dir);
        // An entry is reached whether present or not: Git reads a missing one
        // as absent and creates it through the link, in the repository.
        for name in LIVE_DIRECTORIES {
            let target = if *name == "objects" {
                PathBuf::from(&layout.objects)
            } else {
                real.join(name)
            };
            let link = path.join(name);
            match reach {
                Reach::Links => symlink(&target, &link, true),
                Reach::Copies => junction(&target, &link),
            }
            .map_err(|_| Build::Unlinkable)?;
            private.links.push(link);
        }
        for name in LIVE_FILES {
            let (file, copy) = (real.join(name), path.join(name));
            match (reach, *name) {
                (Reach::Links, _) => {
                    symlink(&file, &copy, false).map_err(|_| Build::Unlinkable)?;
                    private.links.push(copy);
                }
                (Reach::Copies, name) => {
                    let limit = if name == "shallow" {
                        SHALLOW_LIMIT
                    } else {
                        PACKED_REFS_LIMIT
                    };
                    private.held.push(Held::take(file, copy, limit).await?);
                }
            }
        }
        Ok(private)
    }
}

impl Private {
    /// Complete once, retaining the private, inode-fresh recovery data on
    /// failure. Drop also uses this path when an action/checkpoint is cancelled.
    fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let result = self.carry_back();
        if let Err(error) = &result {
            self.common.disable_cleanup(true);
            eprintln!(
                "Git recovery history could not be saved: {error}; pending data retained at {}",
                self.common.path().display()
            );
        }
        result
    }

    /// Replace changed state files with fresh inodes. Reflogs are merged
    /// under Git's ref locks; delete a private log only after that succeeds.
    /// A busy ref never causes its pending recovery entries to be deleted.
    fn carry_back(&mut self) -> io::Result<()> {
        let path = self.common.path();
        for held in &mut self.held {
            held.carry_back()?;
        }
        let own = carry::folder(&self.git_dir)?;
        for (name, original) in &self.state {
            let now = read_regular(&path.join(name), STATE_LIMIT)
                .map_err(|_| io::Error::other("could not read private Git state"))?;
            if now == *original {
                continue;
            }
            match now {
                Some(bytes) => carry::replace(&own, name, &bytes)?,
                None => carry::remove(&own, name)?,
            }
        }
        let mut reflogs = Vec::new();
        new_reflogs(&path.join("logs"), Path::new(""), &mut reflogs);
        let mut failure = None;
        for (relative, entries) in reflogs {
            let result = self
                .carry_reflog(&relative, &entries)
                .and_then(|()| fs::remove_file(path.join("logs").join(&relative)));
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Adds `entries` to the repository's reflog at `relative` under `logs`:
    /// the worktree's own for HEAD, the common directory's for every ref.
    /// Git appends to a ref's reflog, and rewrites it (`reflog expire`), only
    /// while holding that ref's own lock file; holding it too means another
    /// Git process either waits or reports the ref busy, and no entry it
    /// adds is replaced away.
    fn carry_reflog(&self, relative: &Path, entries: &[u8]) -> io::Result<()> {
        let base = if relative == Path::new("HEAD") {
            &self.git_dir
        } else {
            &self.common_dir
        };
        let (folder, file) = carry::parent(base, relative)?;
        let _lock = carry::lock(folder, file)?;
        let log = Path::new("logs").join(relative);
        let (folder, file) = carry::parent(base, &log)?;
        let mut bytes = carry::read(&folder, file, REFLOG_LIMIT)?.unwrap_or_default();
        #[cfg(test)]
        BETWEEN_REFLOG_READ_AND_REPLACE.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(relative);
            }
        });
        bytes.extend_from_slice(entries);
        carry::replace(&folder, file, &bytes)
    }
}

#[cfg(test)]
type ReflogHook = Box<dyn FnMut(&Path)>;

#[cfg(test)]
thread_local! {
    /// Runs another Git process at the moment a reflog's entries are being
    /// carried back, as one could run on the computer.
    pub(super) static BETWEEN_REFLOG_READ_AND_REPLACE: std::cell::RefCell<Option<ReflogHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Every reflog Git wrote beneath `folder` in a private Git directory, by
/// its path under `logs`.
fn new_reflogs(folder: &Path, relative: &Path, found: &mut Vec<(PathBuf, Vec<u8>)>) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        if found.len() >= REFLOG_FILES {
            return;
        }
        let (path, relative) = (entry.path(), relative.join(entry.file_name()));
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => new_reflogs(&path, &relative, found),
            Ok(kind) if kind.is_file() => {
                if let Ok(Some(bytes)) = read_regular(&path, REFLOG_LIMIT) {
                    found.push((relative, bytes));
                }
            }
            _ => {}
        }
    }
}

impl Drop for Private {
    fn drop(&mut self) {
        let _ = self.finish();
        for link in &self.links {
            // A directory junction is removed as a directory, never emptied.
            let _ = fs::remove_file(link).or_else(|_| fs::remove_dir(link));
        }
    }
}

/// Reading and replacing files in the repository's own Git folders for a
/// private Git directory. A folder is opened one name at a time from the
/// root without following a link, so it is reached only as its canonical
/// path names it, never through a link swapped in later; a file is replaced
/// by a new one renamed over its name, so whatever that name was linked to,
/// hard links included, is never written.
#[cfg(unix)]
mod carry {
    use std::ffi::{CString, OsStr};
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path};

    pub(super) type Folder = OwnedFd;

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| io::Error::other("name"))
    }

    fn check(result: libc::c_int) -> io::Result<libc::c_int> {
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    }

    fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
        // SAFETY: a descriptor just returned by the system, owned only here.
        check(fd).map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// The folder at canonical `path`.
    pub(super) fn folder(path: &Path) -> io::Result<Folder> {
        let root = c_name("/".as_ref())?;
        // SAFETY: a valid C string.
        let mut current = owned(unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(part) => current = child(&current, part, false)?,
                _ => return Err(io::Error::other("not canonical")),
            }
        }
        Ok(current)
    }

    /// The folder `part` in `parent`, made first with `create`.
    pub(super) fn child(parent: &Folder, part: &OsStr, create: bool) -> io::Result<Folder> {
        let part = c_name(part)?;
        if create {
            // SAFETY: a valid descriptor and C string.
            if let Err(error) =
                check(unsafe { libc::mkdirat(parent.as_raw_fd(), part.as_ptr(), 0o755) })
            {
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
        }
        // SAFETY: a valid descriptor and C string.
        owned(unsafe {
            libc::openat(
                parent.as_raw_fd(),
                part.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
    }

    /// A regular file's bytes in `folder`, at most `limit`; None when absent.
    pub(super) fn read(folder: &Folder, file: &str, limit: u64) -> io::Result<Option<Vec<u8>>> {
        let name = c_name(file.as_ref())?;
        // SAFETY: a valid descriptor and C string.
        let opened = owned(unsafe {
            libc::openat(
                folder.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        });
        let file = match opened {
            Ok(fd) => std::fs::File::from(fd),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(io::Error::other("too large"));
        }
        Ok(Some(bytes))
    }

    /// Replaces `file` in `folder` with a new file holding `bytes`, made
    /// under Git's own lock name, so a Git process updating it meanwhile
    /// fails rather than racing.
    pub(super) fn replace(folder: &Folder, file: &str, bytes: &[u8]) -> io::Result<()> {
        let lock = c_name(format!("{file}.lock").as_ref())?;
        let name = c_name(file.as_ref())?;
        // SAFETY: a valid descriptor and C string; the mode is promoted as
        // a variadic argument.
        let fd = owned(unsafe {
            libc::openat(
                folder.as_raw_fd(),
                lock.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o644 as libc::c_uint,
            )
        })?;
        let written = std::fs::File::from(fd).write_all(bytes).and_then(|()| {
            // SAFETY: valid descriptors and C strings.
            check(unsafe {
                libc::renameat(
                    folder.as_raw_fd(),
                    lock.as_ptr(),
                    folder.as_raw_fd(),
                    name.as_ptr(),
                )
            })
            .map(drop)
        });
        if written.is_err() {
            // SAFETY: a valid descriptor and C string.
            unsafe { libc::unlinkat(folder.as_raw_fd(), lock.as_ptr(), 0) };
        }
        written
    }

    /// The folder holding `relative` beneath canonical `base`, made as
    /// needed, and the file's name.
    pub(super) fn parent<'a>(base: &Path, relative: &'a Path) -> io::Result<(Folder, &'a str)> {
        let mut parts: Vec<_> = relative.iter().collect();
        let file = parts
            .pop()
            .and_then(|file| file.to_str())
            .ok_or_else(|| io::Error::other("name"))?;
        let mut current = folder(base)?;
        for part in parts {
            current = child(&current, part, true)?;
        }
        Ok((current, file))
    }

    /// Git's own lock file on `file`, waited for as Git waits; removed on drop.
    pub(super) struct Lock(Folder, CString, std::fs::File);

    pub(super) fn lock(folder: Folder, file: &str) -> io::Result<Lock> {
        let name = c_name(format!("{file}.lock").as_ref())?;
        let started = std::time::Instant::now();
        loop {
            // SAFETY: a valid descriptor and C string; the mode is promoted
            // as a variadic argument.
            let made = owned(unsafe {
                libc::openat(
                    folder.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o644 as libc::c_uint,
                )
            });
            match made {
                Ok(fd) => return Ok(Lock(folder, name, std::fs::File::from(fd))),
                Err(error)
                    if error.kind() == io::ErrorKind::AlreadyExists
                        && started.elapsed() < super::LOCK_WAIT =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }

    impl Lock {
        pub(super) fn folder(&self) -> &Folder {
            &self.0
        }

        /// Write the inode we created, never a reopened lock pathname. Move
        /// it into place relative to the same pinned repository directory.
        pub(super) fn replace(&mut self, file: &str, bytes: &[u8]) -> io::Result<()> {
            let name = c_name(file.as_ref())?;
            self.2.write_all(bytes)?;
            // SAFETY: valid descriptors and C strings.
            check(unsafe {
                libc::renameat(
                    self.0.as_raw_fd(),
                    self.1.as_ptr(),
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                )
            })
            .map(drop)
        }
    }

    impl Drop for Lock {
        fn drop(&mut self) {
            // SAFETY: a valid descriptor and C string.
            unsafe { libc::unlinkat(self.0.as_raw_fd(), self.1.as_ptr(), 0) };
        }
    }

    /// Removes `file`'s name from `folder`; absent is fine.
    pub(super) fn remove(folder: &Folder, file: &str) -> io::Result<()> {
        let name = c_name(file.as_ref())?;
        // SAFETY: a valid descriptor and C string.
        match check(unsafe { libc::unlinkat(folder.as_raw_fd(), name.as_ptr(), 0) }) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// Git actions refuse here (see `can_confine`), so nothing is ever carried.
#[cfg(not(unix))]
mod carry {
    use std::ffi::OsStr;
    use std::io;
    use std::path::Path;

    pub(super) struct Folder;

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::other("unsupported"))
    }

    pub(super) fn folder(_: &Path) -> io::Result<Folder> {
        unsupported()
    }

    pub(super) fn child(_: &Folder, _: &OsStr, _: bool) -> io::Result<Folder> {
        unsupported()
    }

    pub(super) fn read(_: &Folder, _: &str, _: u64) -> io::Result<Option<Vec<u8>>> {
        unsupported()
    }

    pub(super) fn replace(_: &Folder, _: &str, _: &[u8]) -> io::Result<()> {
        unsupported()
    }

    pub(super) fn remove(_: &Folder, _: &str) -> io::Result<()> {
        unsupported()
    }

    pub(super) fn parent<'a>(_: &Path, _: &'a Path) -> io::Result<(Folder, &'a str)> {
        unsupported()
    }

    pub(super) struct Lock;

    impl Lock {
        pub(super) fn folder(&self) -> &Folder {
            &Folder
        }

        pub(super) fn replace(&mut self, _: &str, _: &[u8]) -> io::Result<()> {
            unsupported()
        }
    }

    pub(super) fn lock(_: Folder, _: &str) -> io::Result<Lock> {
        unsupported()
    }
}

/// A live file the view reaches by copy, under Git's own lock in a pinned
/// directory. Both reading and carrying back avoid every mutable pathname.
struct Held {
    file: String,
    copy: PathBuf,
    original: Option<Vec<u8>>,
    limit: u64,
    lock: carry::Lock,
}

impl Held {
    async fn take(file: PathBuf, copy: PathBuf, limit: u64) -> Result<Self, Build> {
        let (name, original, lock) = tokio::task::spawn_blocking(move || {
            let parent = file.parent().ok_or(Build::Failed)?;
            let name = file
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(Build::Failed)?;
            let folder = carry::folder(parent).map_err(|_| Build::Failed)?;
            let lock = carry::lock(folder, name).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    Build::Busy(parent.join(format!("{name}.lock")))
                } else {
                    Build::Failed
                }
            })?;
            let original = carry::read(lock.folder(), name, limit).map_err(|_| Build::Failed)?;
            Ok((name.to_owned(), original, lock))
        })
        .await
        .map_err(|_| Build::Failed)??;
        if let Some(bytes) = &original {
            tokio::fs::write(&copy, bytes)
                .await
                .map_err(|_| Build::Failed)?;
        }
        Ok(Self {
            file: name,
            copy,
            original,
            limit,
            lock,
        })
    }

    fn carry_back(&mut self) -> io::Result<()> {
        let current = read_regular(&self.copy, self.limit)
            .map_err(|_| io::Error::other("could not read private Git state"))?;
        if current == self.original {
            return Ok(());
        }
        match current {
            Some(bytes) => self.lock.replace(&self.file, &bytes),
            None => carry::remove(self.lock.folder(), &self.file),
        }
    }
}

/// A regular file's bytes, at most `limit`, opened without following a final
/// link as reads copy `shallow`; None when absent. An agent can swap the
/// repository's file for a link to anything, even a device that never ends.
fn read_regular(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, Build> {
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
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Build::Failed),
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(Build::Failed);
    }
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut io::Read::take(file, limit + 1), &mut bytes)
        .map_err(|_| Build::Failed)?;
    if bytes.len() as u64 > limit {
        return Err(Build::Failed);
    }
    Ok(Some(bytes))
}

/// What the write view does with one repository-local setting.
#[derive(Debug, PartialEq)]
enum Local {
    Copy,
    Drop,
    Refuse,
}

/// Repository-local settings whose program or included file the result would
/// depend on: content filters, the repository's own sign-in or transport, its
/// own signing program, or included settings. The write view never loads
/// them, so a local write is refused rather than committing different bytes
/// or pushing and signing as someone else. Every other program setting (hooks,
/// fsmonitor, editors, pagers, diff and merge drivers) is simply not loaded.
pub(super) fn needs_own_program(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let exact = [
        "core.sshcommand",
        "core.askpass",
        "core.gitproxy",
        "credential.helper",
        "gpg.program",
        // Commit signing runs this when no signing key is set.
        "gpg.ssh.defaultkeycommand",
    ];
    exact.contains(&key.as_str())
        || key.starts_with("include.")
        || key.starts_with("includeif.")
        || key.starts_with("filter.")
        || (key.starts_with("credential.") && key.ends_with(".helper"))
        || (key.starts_with("gpg.") && key.ends_with(".program"))
        || (key.starts_with("remote.")
            && (key.ends_with(".uploadpack")
                || key.ends_with(".receivepack")
                || key.ends_with(".vcs")))
}

/// Data an action needs from the repository's config: identity and the choice
/// to sign, remotes, tracking and URL rewrites, and how files are compared.
fn local_setting(key: &str) -> Local {
    if needs_own_program(key) {
        return Local::Refuse;
    }
    let Some((section, rest)) = key.split_once('.') else {
        return Local::Drop;
    };
    let (sub, name) = match rest.rsplit_once('.') {
        Some((sub, name)) => (Some(sub), name),
        None => (None, rest),
    };
    let section = section.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let copy = matches!(
        (section.as_str(), sub, name.as_str()),
        (
            "core",
            None,
            "filemode"
                | "ignorecase"
                | "symlinks"
                | "quotepath"
                | "precomposeunicode"
                | "autocrlf"
                | "eol"
                | "safecrlf"
                | "logallrefupdates"
                | "checkstat"
                | "trustctime"
                | "sparsecheckout"
                | "sparsecheckoutcone"
                | "excludesfile",
        ) | (
            "user",
            None,
            "name" | "email" | "signingkey" | "useconfigonly"
        ) | ("author" | "committer", None, "name" | "email")
            | ("commit", None, "gpgsign")
            | ("gpg", None, "format")
            | ("remote", None, "pushdefault")
            | (
                "remote",
                Some(_),
                "url"
                    | "pushurl"
                    | "fetch"
                    | "push"
                    | "promisor"
                    | "partialclonefilter"
                    | "gh-resolved",
            )
            | ("branch", Some(_), "remote" | "merge" | "pushremote")
            | ("url", Some(_), "insteadof" | "pushinsteadof")
    );
    if copy {
        Local::Copy
    } else {
        Local::Drop
    }
}

impl WriteView {
    /// Prepares the view from one snapshot of the repository's settings.
    /// With `refuse`, a setting the result would depend on is refused.
    pub(super) async fn prepare(top: &Path, refuse: bool) -> Result<Self, Refusal> {
        Self::prepare_reaching(top, refuse, reaches()).await
    }

    /// The first of `reaches` this computer can make; refused without one.
    async fn prepare_reaching(
        top: &Path,
        refuse: bool,
        reaches: &[Reach],
    ) -> Result<Self, Refusal> {
        let unavailable = || {
            (
                "unavailable",
                "Cupola couldn't read this repository's Git setup. Use Git on your computer."
                    .to_owned(),
            )
        };
        let unsupported = || {
            (
                "unavailable",
                "This repository uses a Git format Cupola's Git actions don't support. Use Git on your computer.".to_owned(),
            )
        };
        if !can_confine() {
            return Err(unconfined());
        }
        let top = &crate::canonical_path::canonicalize_async(top)
            .await
            .map_err(|_| unavailable())?;
        let layout = workspace_git::contained_layout(top)
            .await
            .map_err(|uncontained| uncontained_refusal(&uncontained))?;
        let settings = workspace_git::contained_settings(top, &layout)
            .await
            .map_err(|_| unavailable())?;
        let mut copied = Vec::new();
        let mut partial_clone = None;
        for entry in settings.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let mut parts = entry.splitn(2, |b| *b == b'\n');
            let key = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
            // A bare boolean (`[commit] gpgsign`) is listed without a value.
            let value = parts.next().unwrap_or(b"true");
            let lower = key.to_ascii_lowercase();
            if let Some(extension) = lower.strip_prefix("extensions.") {
                match (extension, value) {
                    ("objectformat" | "worktreeconfig" | "noop" | "preciousobjects", _)
                    | ("refstorage", b"files") => {}
                    ("partialclone", remote)
                        if std::str::from_utf8(remote).is_ok_and(valid_remote) =>
                    {
                        partial_clone = Some(String::from_utf8_lossy(remote).into_owned());
                    }
                    _ => return Err(unsupported()),
                }
                continue;
            }
            match local_setting(&key) {
                Local::Refuse if refuse => {
                    return refuse_setting(&lower);
                }
                Local::Copy => {
                    let value = std::str::from_utf8(value).map_err(|_| unavailable())?;
                    copied.push((key, value.to_owned()));
                }
                Local::Refuse | Local::Drop => {}
            }
        }
        let mut config = format!(
            "[core]\n\trepositoryFormatVersion = {}\n\tbare = false\n",
            u8::from(layout.sha256 || partial_clone.is_some())
        );
        if layout.sha256 || partial_clone.is_some() {
            config.push_str("[extensions]\n");
        }
        if layout.sha256 {
            config.push_str("\tobjectFormat = sha256\n");
        }
        if let Some(remote) = &partial_clone {
            config.push_str(&format!("\tpartialClone = {remote}\n"));
        }
        for (key, value) in &copied {
            config.push_str(&config_entry(key, value).ok_or_else(unavailable)?);
        }
        let mut private = None;
        for reach in reaches {
            match Private::build(&layout, &config, *reach).await {
                Ok(built) => {
                    private = Some(built);
                    break;
                }
                Err(Build::Busy(lock)) => {
                    return Err((
                        "busy",
                        format!(
                            "Another Git process is updating this repository on your computer. Try again in a moment; if this keeps happening, delete {}.",
                            lock.display()
                        ),
                    ))
                }
                Err(Build::Failed) => return Err(unavailable()),
                Err(Build::Unlinkable) => {}
            }
        }
        let private = private.ok_or_else(unavailable)?;
        // Git is pinned to the private Git directory, the checked objects,
        // index and work tree, so a `.git` file or `commondir` changed after
        // the check is never followed.
        let mut env: Vec<(String, OsString)> = workspace_git::contained_env(top, &layout)
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        for (key, value) in &mut env {
            if key == "GIT_DIR" || key == "GIT_COMMON_DIR" {
                *value = private.common.path().into();
            }
        }
        env.push((
            "GIT_INDEX_FILE".into(),
            Path::new(&layout.git_dir).join("index").into(),
        ));
        // Command-scope settings; Git's local receivers and submodule
        // commands clear them along with the view's repository variables.
        env.extend(hardening_environment());
        let temporary =
            workspace_git::private_git_directory("wonder-git-tmp-").map_err(|_| unavailable())?;
        env.push(("TMPDIR".into(), temporary.path().into()));
        // Seatbelt matches the real path a write reaches (/private/var for
        // /var), so every folder is named as it really is.
        let mut writable = [
            top.as_path(),
            Path::new(&layout.git_dir),
            Path::new(&layout.common_dir),
            temporary.path(),
            private.common.path(),
        ]
        .into_iter()
        .map(crate::canonical_path::canonicalize)
        .collect::<io::Result<Vec<_>>>()
        .map_err(|_| unavailable())?;
        // Signing with GnuPG keeps its lock and seed files in the owner's
        // GnuPG folder.
        let gnupg = std::env::var_os("GNUPGHOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".gnupg")));
        writable.extend(gnupg.and_then(|folder| crate::canonical_path::canonicalize(folder).ok()));
        // Bind them now, so nothing swapped in later is ever writable.
        #[cfg(target_os = "linux")]
        let anchors = writable
            .iter()
            .map(|folder| landlock::anchor(folder))
            .collect::<io::Result<Vec<_>>>()
            .map_err(|_| unavailable())?;
        Ok(Self {
            top: top.to_path_buf(),
            env,
            git_dir: PathBuf::from(&layout.git_dir),
            config: Path::new(&layout.common_dir).join("config"),
            writable,
            #[cfg(target_os = "linux")]
            anchors,
            _temporary: temporary,
            _private: private,
        })
    }
}

/// One copied setting as config-file text that Git reads back exactly. The
/// section and name come from `local_setting`'s fixed list; a subsection or
/// value is quoted, so it can never start another section or setting.
fn config_entry(key: &str, value: &str) -> Option<String> {
    let (section, rest) = key.split_once('.')?;
    let (sub, name) = match rest.rsplit_once('.') {
        Some((sub, name)) => (Some(sub), name),
        None => (None, rest),
    };
    let plain = |text: &str| {
        !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if !plain(section)
        || !plain(name)
        || sub.is_some_and(|sub| sub.chars().any(char::is_control))
        || value
            .chars()
            .any(|c| c.is_control() && c != '\t' && c != '\n')
    {
        return None;
    }
    let quote = |text: &str| {
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\t', "\\t")
            .replace('\n', "\\n")
    };
    Some(match sub {
        Some(sub) => format!(
            "[{section} \"{}\"]\n\t{name} = \"{}\"\n",
            quote(sub),
            quote(value)
        ),
        None => format!("[{section}]\n\t{name} = \"{}\"\n", quote(value)),
    })
}

/// Why a repository whose Git data isn't its own is never written.
pub(super) fn uncontained_refusal(uncontained: &workspace_git::Uncontained) -> Refusal {
    match uncontained {
        workspace_git::Uncontained::Outside => (
            "outsideProject",
            "This repository's Git data reaches outside its folder, so Cupola won't change it. Use Git on your computer.".to_owned(),
        ),
        workspace_git::Uncontained::Unreadable => (
            "unavailable",
            "Cupola couldn't read this repository's Git setup. Use Git on your computer."
                .to_owned(),
        ),
    }
}

fn refuse_setting<T>(key: &str) -> Result<T, Refusal> {
    refuse(
        "unsafeConfig",
        format!("This repository's own Git settings run a program ({key}), so Cupola won't run Git in it. Use Git on your computer."),
    )
}

fn symlink(target: &Path, link: &Path, directory: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = directory;
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        if directory {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }
}

/// Unix has no junction and needs no privilege for a symbolic link, which
/// stands in here so the copy path runs (and is tested) everywhere.
#[cfg(unix)]
fn junction(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// A directory junction, which Windows lets any user make, unlike a symbolic
/// link.
#[cfg(windows)]
fn junction(target: &Path, link: &Path) -> io::Result<()> {
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    let wide: Vec<u16> = target.as_os_str().encode_wide().collect();
    let buffer = mount_point(&wide).ok_or(io::ErrorKind::Unsupported)?;
    let size = u32::try_from(buffer.len()).map_err(|_| io::ErrorKind::Unsupported)?;
    fs::create_dir(link)?;
    let set = fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(link)
        .and_then(|directory| {
            let mut returned = 0u32;
            // SAFETY: the handle stays open for the call, and the buffer is a
            // complete mount-point reparse buffer of `size` bytes.
            let done = unsafe {
                DeviceIoControl(
                    directory.as_raw_handle(),
                    FSCTL_SET_REPARSE_POINT,
                    buffer.as_ptr().cast(),
                    size,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if done == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    if set.is_err() {
        let _ = fs::remove_dir(link);
    }
    set
}

/// The mount-point reparse buffer (`REPARSE_DATA_BUFFER`) for a junction to
/// `target`, a UTF-16 path. A junction can only point at a folder on a local
/// drive, so anything else (a network share, a relative path) is None.
#[cfg(any(windows, test))]
fn mount_point(target: &[u16]) -> Option<Vec<u8>> {
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    let backslash = u16::from(b'\\');
    // Git for Windows prints forward slashes; an NT path needs backslashes.
    let target: Vec<u16> = target
        .iter()
        .map(|&unit| {
            if unit == u16::from(b'/') {
                backslash
            } else {
                unit
            }
        })
        .collect();
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    let print = target.strip_prefix(verbatim.as_slice()).unwrap_or(&target);
    let [letter, colon, slash, ..] = print else {
        return None;
    };
    if !u8::try_from(*letter).is_ok_and(|b| b.is_ascii_alphabetic())
        || *colon != u16::from(b':')
        || *slash != backslash
    {
        return None;
    }
    let substitute: Vec<u16> = r"\??\"
        .encode_utf16()
        .chain(print.iter().copied())
        .collect();
    let size = |units: &[u16]| u16::try_from(units.len() * 2).ok();
    let (substitute_size, print_size) = (size(&substitute)?, size(print)?);
    // The four name fields, then both names, each NUL-terminated.
    let data =
        u16::try_from(8 + usize::from(substitute_size) + 2 + usize::from(print_size) + 2).ok()?;
    let mut buffer = Vec::with_capacity(8 + usize::from(data));
    buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend(data.to_le_bytes());
    buffer.extend(0u16.to_le_bytes());
    buffer.extend(0u16.to_le_bytes());
    buffer.extend(substitute_size.to_le_bytes());
    buffer.extend((substitute_size + 2).to_le_bytes());
    buffer.extend(print_size.to_le_bytes());
    for unit in substitute.iter().chain([&0]).chain(print).chain([&0]) {
        buffer.extend(unit.to_le_bytes());
    }
    Some(buffer)
}

// -------------------------------------------------------------------------
// Failure wording
// -------------------------------------------------------------------------

/// Plain words for a failed git or gh run. Raw output never reaches the
/// phone except a remote's own short refusal reason.
fn explain(kind: Kind, run: &Run) -> Refusal {
    let text = run.text();
    let lower = text.to_ascii_lowercase();
    let has = |needle: &str| lower.contains(needle);
    if has("gh006") || has("protected branch") || has("branch protection") {
        return (
            "protectedBranch",
            "GitHub protects this branch, so it can't be pushed to directly. Open a pull request instead."
                .into(),
        );
    }
    if has("non-fast-forward") || has("fetch first") || has("not possible to fast-forward") {
        return (
            "nonFastForward",
            match kind {
                Kind::Pull => "This branch and its remote both have new commits. Pull only fast-forwards, so reconcile them on your computer (rebase or merge).".into(),
                _ => "The remote has commits this branch doesn't. Pull first, then push again.".into(),
            },
        );
    }
    if has("would be overwritten") {
        return (
            "localChanges",
            "Uncommitted changes would be overwritten. Commit them first, then pull.".into(),
        );
    }
    if has("conflict") && !has("no conflict") {
        return (
            "conflicts",
            "This has merge conflicts. Resolve them first (Resolve conflicts asks the agent)."
                .into(),
        );
    }
    if has("please tell me who you are") || has("unable to auto-detect email") {
        return (
            "identity",
            "Git on your computer doesn't know your name and email. Set them with git config --global user.name and user.email.".into(),
        );
    }
    if has("could not read username")
        || has("authentication failed")
        || has("permission denied")
        || has("403")
        || has("could not read from remote repository")
        || has("must have push access")
        || has("must have write access")
        || has("not authorized")
    {
        return (
            "auth",
            "Your computer's Git or GitHub sign-in can't do this in this repository. Check access on your computer.".into(),
        );
    }
    if has("could not resolve host")
        || has("unable to access")
        || has("connection timed out")
        || has("network is unreachable")
        || has("error connecting to api.github.com")
    {
        return (
            "network",
            "Your computer couldn't reach the remote. Check its connection and try again.".into(),
        );
    }
    if let Some(reason) = remote_rejection(&text) {
        return (
            "rejected",
            format!("The remote refused the push: {reason}."),
        );
    }
    (
        "failed",
        match kind {
            Kind::Commit => "Git couldn't make the commit. Check the repository on your computer.",
            Kind::Push => "Git couldn't push. Check the repository on your computer.",
            Kind::Pull => "Git couldn't pull. Check the repository on your computer.",
            Kind::CreatePullRequest => {
                "GitHub didn't open the pull request. Check it on your computer."
            }
            Kind::Merge => "GitHub didn't merge the pull request. Open it on GitHub for details.",
            Kind::AutoMerge => {
                "GitHub didn't change auto-merge. Open the pull request on GitHub for details."
            }
        }
        .into(),
    )
}

/// The reason in `! ... [remote rejected] (reason)`, kept short.
fn remote_rejection(text: &str) -> Option<String> {
    let start = text.find("[remote rejected] (")? + "[remote rejected] (".len();
    let reason = &text[start..];
    let reason = &reason[..reason.find(')')?];
    let reason: String = reason
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    (!reason.trim().is_empty()).then(|| reason.trim().to_owned())
}

// -------------------------------------------------------------------------
// Branch facts
// -------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Branch {
    name: Option<String>,
    has_commits: bool,
    remote: Option<String>,
    /// `refs/heads/x` on the remote.
    merge: Option<String>,
    ahead: Option<u64>,
    behind: Option<u64>,
    default: Option<String>,
}

impl Branch {
    fn upstream_name(&self) -> Option<&str> {
        self.merge.as_deref()?.strip_prefix("refs/heads/")
    }
    fn tracking(&self) -> Option<String> {
        Some(format!(
            "{}/{}",
            self.remote.as_deref()?,
            self.upstream_name()?
        ))
    }
}

fn valid_remote(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

async fn branch(view: &WriteView) -> Branch {
    let mut facts = Branch {
        name: read_line(view, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await
            .filter(|name| crate::pull_requests::valid_branch(name)),
        ..Default::default()
    };
    facts.has_commits = read_line(
        view,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            "HEAD^{commit}",
        ],
    )
    .await
    .is_some();
    facts.default = workspace_git::base(&view.top)
        .await
        .map(|base| thread_repos::default_branch_name(&base.label));
    if let Some(name) = facts.name.clone() {
        facts.remote = read_line(view, &["config", "--get", &format!("branch.{name}.remote")])
            .await
            .filter(|remote| valid_remote(remote));
        facts.merge = read_line(view, &["config", "--get", &format!("branch.{name}.merge")])
            .await
            .filter(|merge| {
                merge
                    .strip_prefix("refs/heads/")
                    .is_some_and(crate::pull_requests::valid_branch)
            });
        if facts.remote.is_some() && facts.merge.is_some() && facts.has_commits {
            if let Some(counts) = read_line(
                view,
                &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
            )
            .await
            {
                let mut counts = counts.split_whitespace().map(|value| value.parse().ok());
                facts.behind = counts.next().flatten();
                facts.ahead = counts.next().flatten();
            }
        }
    }
    facts
}

/// The remote a new branch is pushed to: origin, or the only remote.
async fn push_remote(view: &WriteView) -> Option<String> {
    let remotes = read_line(view, &["remote"]).await?;
    let remotes: Vec<&str> = remotes.lines().map(str::trim).collect();
    if remotes.contains(&"origin") {
        return Some("origin".into());
    }
    match remotes[..] {
        [only] if valid_remote(only) => Some(only.to_owned()),
        _ => None,
    }
}

fn require_branch(facts: &Branch) -> Result<&str, Refusal> {
    facts.name.as_deref().ok_or((
        "detached",
        "This repository isn't on a branch. Check out a branch on your computer first.".into(),
    ))
}

// -------------------------------------------------------------------------
// Actions
// -------------------------------------------------------------------------

/// Runs one validated action in an admitted repository.
pub(super) async fn perform(
    top: &Path,
    slugs: &[String],
    request: &ActionRequest,
    gh: &Gh,
) -> Outcome {
    let Some(kind) = Kind::parse(&request.action) else {
        return Outcome::refused(Kind::Commit, "invalid", "Choose a Git action.");
    };
    // The refusal and every process below use the same settings snapshot, so
    // a change to the repository's config after this point is never loaded.
    let view = match WriteView::prepare(top, kind.writes_locally()).await {
        Ok(view) => view,
        Err((reason, message)) => return Outcome::refused(kind, reason, message),
    };
    let result = match kind {
        Kind::Commit => commit(&view, request).await,
        Kind::Push => push(&view, request).await,
        Kind::Pull => pull(&view).await,
        Kind::CreatePullRequest => create_pull_request(&view, request, gh).await,
        Kind::Merge | Kind::AutoMerge => merge(&view, slugs, kind, request, gh).await,
    };
    let mut outcome = match result {
        Ok(outcome) => outcome,
        Err((reason, message)) => Outcome::refused(kind, reason, message),
    };
    if let Err((reason, message)) = view.finish().await {
        outcome.ok = false;
        outcome.reason = Some(reason);
        outcome.message = format!("{} {message}", outcome.message);
    }
    outcome
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

async fn commit(view: &WriteView, request: &ActionRequest) -> Result<Outcome, Refusal> {
    let message = request.message.as_deref().unwrap_or("").trim();
    if message.is_empty() {
        return refuse("invalid", "Write a commit message.");
    }
    if message.chars().count() > MAX_MESSAGE || message.contains('\0') {
        return refuse("invalid", "This commit message is too long.");
    }
    let facts = branch(view).await;
    let name = require_branch(&facts)?.to_owned();
    let include = request.include_unstaged == Some(true);
    if include {
        let added = add_parent_files(view, false).await?;
        if !added.success {
            return Err(explain(Kind::Commit, &added));
        }
    }
    let staged = git(
        view,
        &["diff", "--cached", "--name-only", "-z"],
        LOCAL_TIMEOUT,
    )
    .await?;
    if !staged.success {
        return Err(explain(Kind::Commit, &staged));
    }
    let count = staged.stdout.split('\0').filter(|p| !p.is_empty()).count();
    if count == 0 {
        return refuse(
            "nothingToCommit",
            if include {
                "There are no changes to commit."
            } else {
                "Nothing is staged. Turn on Include unstaged changes to commit everything."
            },
        );
    }
    if let Some(ignored) = staged_ignored(view).await? {
        return refuse(
            "ignoredFiles",
            format!(
                "{ignored} is staged but matches .gitignore. Unstage it on your computer, then commit."
            ),
        );
    }
    let committed = git(
        view,
        &["commit", "--quiet", "--no-verify", "-m", message],
        LOCAL_TIMEOUT,
    )
    .await?;
    if !committed.success {
        return Err(explain(Kind::Commit, &committed));
    }
    let short = read_line(view, &["rev-parse", "--short", "HEAD"]).await;
    let mut outcome = Outcome::done(
        Kind::Commit,
        format!(
            "Committed {} to {name}{}.",
            plural(count, "file"),
            short
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default()
        ),
    );
    outcome.commit = short.filter(|s| s.bytes().all(|b| b.is_ascii_hexdigit()));
    Ok(outcome)
}

/// Stage parent files only. Even with recursion off, `add` checks a gitlink's
/// dirty files and loads the submodule's unchecked config/attributes. Exclude
/// every indexed gitlink (including conflict stages), without entering it.
/// Already staged gitlink updates remain eligible for the commit. Commit and
/// the per-turn checkpoint share this, so neither runs a nested repository's
/// configured filter program; `ignore_errors` is the checkpoint's.
pub(super) async fn add_parent_files(
    view: &WriteView,
    ignore_errors: bool,
) -> Result<Run, Refusal> {
    let failed = || {
        (
            "failed",
            "Git couldn't make the commit. Check the repository on your computer.".to_owned(),
        )
    };
    // Keep raw NUL-delimited paths: lossy decoding or a truncated listing could
    // leave a gitlink unexcluded. The read boundary fails on oversized output.
    let index = read_only_git_bytes(
        view.top.to_str().ok_or_else(failed)?,
        &["ls-files", "--stage", "-z"],
    )
    .await
    .map_err(|_| failed())?;
    let mut exclusions = Vec::new();
    for entry in index.split(|byte| *byte == 0) {
        if entry.starts_with(b"160000 ") {
            let tab = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(failed)?;
            let path = std::str::from_utf8(&entry[tab + 1..]).map_err(|_| failed())?;
            exclusions.push(format!(":(top,exclude,literal){path}"));
        }
    }
    exclusions.sort_unstable();
    exclusions.dedup();
    let mut args = vec!["add", "--all"];
    if ignore_errors {
        args.push("--ignore-errors");
    }
    args.extend(["--", "."]);
    args.extend(exclusions.iter().map(String::as_str));
    git(view, &args, LOCAL_TIMEOUT).await
}

/// The first newly staged file that .gitignore excludes, if any. Files Git
/// already tracked stay committable even when a later rule matches them.
async fn staged_ignored(view: &WriteView) -> Result<Option<String>, Refusal> {
    let added = git(
        view,
        &["diff", "--cached", "--name-only", "-z", "--diff-filter=A"],
        LOCAL_TIMEOUT,
    )
    .await?;
    let paths: Vec<&str> = added
        .stdout
        .split('\0')
        .filter(|p| !p.is_empty())
        .take(MAX_IGNORE_CHECK)
        .collect();
    for chunk in paths.chunks(IGNORE_CHUNK) {
        let mut args = vec!["check-ignore", "--no-index", "--"];
        args.extend_from_slice(chunk);
        // Exit 0 names ignored paths, 1 means none; anything else failed.
        let checked = git(view, &args, LOCAL_TIMEOUT).await?;
        if let Some(first) = checked.stdout.lines().find(|p| !p.trim().is_empty()) {
            return Ok(Some(first.to_owned()));
        }
        if !checked.success && !checked.stderr.trim().is_empty() {
            return Err(explain(Kind::Commit, &checked));
        }
    }
    Ok(None)
}

async fn push(view: &WriteView, request: &ActionRequest) -> Result<Outcome, Refusal> {
    let facts = branch(view).await;
    let name = require_branch(&facts)?.to_owned();
    if !facts.has_commits {
        return refuse("noCommits", "This branch has no commits to push yet.");
    }
    let (remote, target, new_upstream) = match (&facts.remote, facts.upstream_name()) {
        (Some(remote), Some(target)) => (remote.clone(), target.to_owned(), false),
        _ => match push_remote(view).await {
            Some(remote) => (remote, name.clone(), true),
            None => {
                return refuse(
                    "noRemote",
                    "This repository has no remote to push to. Add one on your computer (git remote add origin …).",
                )
            }
        },
    };
    let default = facts.default.as_deref().unwrap_or("main");
    let to_default = target == default || (facts.default.is_none() && target == "master");
    if to_default && request.confirm_default_branch != Some(true) {
        return refuse(
            "confirmDefaultBranch",
            format!(
                "This pushes straight to {target}, the default branch, without a pull request."
            ),
        );
    }
    if !new_upstream && facts.ahead == Some(0) {
        return Ok(Outcome::done(
            Kind::Push,
            format!("Nothing to push; {remote}/{target} already has every commit."),
        ));
    }
    let refspec = format!("refs/heads/{name}:refs/heads/{target}");
    let destinations = local_push_destination(view, &remote).await?;
    let local = !destinations.is_empty();
    let mut args = if local {
        vec!["push", "--porcelain", LOCAL_RECEIVE_PACK]
    } else {
        // A changed URL/rewrite after inspection must not select an unhardened
        // local receiver. Keep network servers' normal receive-pack behavior.
        vec!["-c", "protocol.file.allow=never", "push", "--porcelain"]
    };
    args.extend([remote.as_str(), refspec.as_str()]);
    // A local receiver writes the destination repository, and only it.
    let pushed = run_writing(
        Path::new("git"),
        view,
        &args,
        NETWORK_TIMEOUT,
        &destinations,
    )
    .await?;
    if !pushed.success {
        return Err(explain(Kind::Push, &pushed));
    }
    if new_upstream {
        // `--set-upstream` would record tracking in the view's private config.
        // Record the same two settings in the repository's own config.
        let config = view
            .config
            .to_str()
            .ok_or(("failed", untracked(&name, &remote)))?;
        for (key, value) in [
            (format!("branch.{name}.remote"), remote.clone()),
            (format!("branch.{name}.merge"), format!("refs/heads/{name}")),
        ] {
            let set = git(
                view,
                &["config", "--file", config, "--replace-all", &key, &value],
                LOCAL_TIMEOUT,
            )
            .await?;
            if !set.success {
                return refuse("failed", untracked(&name, &remote));
            }
        }
    }
    let commits = facts.ahead.map(|n| plural(n as usize, "commit"));
    Ok(Outcome::done(
        Kind::Push,
        if new_upstream {
            format!("Pushed {name} to {remote} and set it to track {remote}/{name}.")
        } else {
            format!(
                "Pushed {} to {remote}/{target}.",
                commits.unwrap_or_else(|| name.clone())
            )
        },
    ))
}

fn untracked(name: &str, remote: &str) -> String {
    format!("Pushed {name} to {remote}, but Git couldn't set it to track {remote}/{name}. Set its upstream on your computer.")
}

/// Git clears the sender's command config before starting a local receiver.
/// Use a fixed receiver command for file transports: no repository hooks,
/// alternate-ref programs, background maintenance or checkout/filter work.
/// `true` advertises no alternate refs (only an optimization), so the receiver
/// never invokes the repository's own alternateRefsCommand.
const LOCAL_RECEIVE_PACK: &str = "--receive-pack=git -c core.hooksPath=/dev/null \
    -c core.fsmonitor=false -c core.alternateRefsCommand=true \
    -c receive.denyCurrentBranch=refuse -c gc.auto=0 -c maintenance.auto=false receive-pack";

/// Ask Git for all effective push URLs so pushurl and insteadOf/pushInsteadOf
/// rewrites get the same treatment as a plain path. Mixed transports require
/// different receivers; refuse them before any destination is changed.
/// Returns the local destinations' folders, canonical; none for a network
/// remote.
async fn local_push_destination(view: &WriteView, remote: &str) -> Result<Vec<PathBuf>, Refusal> {
    let urls = read_line(view, &["remote", "get-url", "--push", "--all", remote])
        .await
        .ok_or((
            "unsafeRemote",
            "This remote's destination could not be checked. Push it on your computer.".to_owned(),
        ))?;
    let local = |url: &str| {
        url.starts_with("file://")
            || Path::new(url).is_absolute()
            || url
                .split_once(':')
                .is_none_or(|(prefix, _)| prefix.contains('/'))
    };
    let any_local = urls.lines().any(local);
    if any_local && !urls.lines().all(local) {
        return refuse(
            "unsafeRemote",
            "This remote mixes local and network destinations. Push it on your computer.",
        );
    }
    if !any_local {
        return Ok(Vec::new());
    }
    urls.lines()
        .map(|url| {
            let path = url.strip_prefix("file://").unwrap_or(url);
            crate::canonical_path::canonicalize(view.top.join(path)).map_err(|_| {
                (
                    "unsafeRemote",
                    "This remote's folder could not be found. Push it on your computer.".to_owned(),
                )
            })
        })
        .collect()
}

async fn pull(view: &WriteView) -> Result<Outcome, Refusal> {
    let facts = branch(view).await;
    require_branch(&facts)?;
    let (Some(remote), Some(merge), Some(tracking)) =
        (facts.remote.clone(), facts.merge.clone(), facts.tracking())
    else {
        return refuse(
            "noUpstream",
            "This branch doesn't track a remote branch yet. Push it first.",
        );
    };
    let fetched = git(
        view,
        &[
            "fetch",
            "--no-tags",
            "--quiet",
            remote.as_str(),
            merge.as_str(),
        ],
        NETWORK_TIMEOUT,
    )
    .await?;
    if !fetched.success {
        return Err(explain(Kind::Pull, &fetched));
    }
    let before = read_line(view, &["rev-parse", "--verify", "--quiet", "HEAD"]).await;
    let merged = git(
        view,
        &["merge", "--ff-only", "--no-edit", "--quiet", "FETCH_HEAD"],
        LOCAL_TIMEOUT,
    )
    .await?;
    if !merged.success {
        return Err(explain(Kind::Pull, &merged));
    }
    let after = read_line(view, &["rev-parse", "--verify", "--quiet", "HEAD"]).await;
    Ok(Outcome::done(
        Kind::Pull,
        if before == after {
            format!("Already up to date with {tracking}.")
        } else {
            let count = match (&before, &after) {
                (Some(before), Some(after)) => {
                    read_line(
                        view,
                        &["rev-list", "--count", &format!("{before}..{after}")],
                    )
                    .await
                }
                _ => None,
            };
            match count.and_then(|c| c.parse::<usize>().ok()) {
                Some(n) => format!("Pulled {} from {tracking}.", plural(n, "commit")),
                None => format!("Pulled from {tracking}."),
            }
        },
    ))
}

async fn create_pull_request(
    view: &WriteView,
    request: &ActionRequest,
    gh: &Gh,
) -> Result<Outcome, Refusal> {
    let title = request.title.as_deref().unwrap_or("").trim();
    if title.is_empty() {
        return refuse("invalid", "Give the pull request a title.");
    }
    let body = request.body.as_deref().unwrap_or("");
    if title.chars().count() > MAX_TITLE || body.chars().count() > MAX_BODY {
        return refuse("invalid", "The title or description is too long.");
    }
    let gh = gh.ready()?;
    let facts = branch(view).await;
    let name = require_branch(&facts)?.to_owned();
    let default = facts.default.clone().unwrap_or_else(|| "main".into());
    let Some(head) = facts.upstream_name().map(str::to_owned) else {
        return refuse(
            "notPushed",
            "Push this branch first so GitHub has its commits.",
        );
    };
    if name == default || head == default {
        return refuse(
            "defaultBranch",
            format!("This is {default}, the default branch. Create a branch for this work first."),
        );
    }
    if facts.ahead.unwrap_or(0) > 0 {
        return refuse(
            "notPushed",
            "This branch has commits GitHub doesn't. Push them first.",
        );
    }
    let base = match request
        .base
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        Some(base) if crate::pull_requests::valid_branch(base) => base.to_owned(),
        Some(_) => return refuse("invalid", "Choose a valid base branch."),
        None => default,
    };
    let mut args = vec![
        "pr", "create", "--title", title, "--body", body, "--head", &head, "--base", &base,
    ];
    if request.draft == Some(true) {
        args.push("--draft");
    }
    let created = run(gh, view, &args, NETWORK_TIMEOUT).await?;
    let address = repo_records::pull_addresses(&created.text())
        .into_iter()
        .last();
    if !created.success {
        if created
            .text()
            .to_ascii_lowercase()
            .contains("already exists")
        {
            return refuse(
                "pullRequestExists",
                match address {
                    Some(address) => {
                        format!("This branch already has pull request #{}.", address.number)
                    }
                    None => "This branch already has a pull request.".into(),
                },
            );
        }
        if created.text().contains("No commits between") {
            return refuse(
                "noChanges",
                format!("{head} has no commits that {base} doesn't have."),
            );
        }
        return Err(explain(Kind::CreatePullRequest, &created));
    }
    let Some(address) = address else {
        return refuse(
            "failed",
            "GitHub didn't return the new pull request's address.",
        );
    };
    let mut outcome = Outcome::done(
        Kind::CreatePullRequest,
        format!(
            "Opened {}pull request #{} into {base}.",
            if request.draft == Some(true) {
                "draft "
            } else {
                ""
            },
            address.number
        ),
    );
    outcome.pull_request = Some(CreatedPull {
        number: address.number,
        url: address.url(),
    });
    Ok(outcome)
}

// -------------------------------------------------------------------------
// Merging
// -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MergeState {
    version: u32,
    number: u64,
    url: String,
    state: &'static str,
    head_ref: Option<String>,
    base_ref: Option<String>,
    /// The head commit these facts describe; a merge can be pinned to it.
    head_oid: Option<String>,
    checks: &'static str,
    conflicting: bool,
    auto_merge: bool,
    methods: Vec<&'static str>,
    default_method: Option<&'static str>,
    /// A stable code for why Merge is unavailable now.
    blocked_reason: Option<&'static str>,
    /// The same in plain words.
    blocked: Option<String>,
}

/// The pull request named by `url`, which must belong to one of the
/// repository's GitHub remotes.
fn owned_address(
    url: Option<&str>,
    slugs: &[String],
) -> Result<repo_records::PullAddress, Refusal> {
    let url = url.unwrap_or("");
    let address = repo_records::pull_addresses(url)
        .into_iter()
        .find(|address| address.url().eq_ignore_ascii_case(url))
        .ok_or(("invalid", "Choose a pull request.".to_owned()))?;
    if !slugs
        .iter()
        .any(|slug| slug.eq_ignore_ascii_case(&address.slug()))
    {
        return refuse(
            "otherRepository",
            "This pull request belongs to a repository this one doesn't push to.",
        );
    }
    Ok(address)
}

fn method_name(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "squash" => Some("squash"),
        "merge" => Some("merge"),
        "rebase" => Some("rebase"),
        _ => None,
    }
}

fn method_title(method: &str) -> &'static str {
    match method {
        "squash" => "Squash and merge",
        "rebase" => "Rebase and merge",
        _ => "Merge commit",
    }
}

/// Reads a pull request's merge readiness and its repository's methods.
pub(super) async fn merge_state(
    view: &WriteView,
    gh: &Path,
    address: &repo_records::PullAddress,
) -> Result<MergeState, Refusal> {
    let url = address.url();
    let viewed = run(
        gh,
        view,
        &["pr", "view", &url, "--json", PR_VIEW_FIELDS],
        NETWORK_TIMEOUT,
    )
    .await?;
    if !viewed.success {
        return Err(explain(Kind::Merge, &viewed));
    }
    let value: Value = serde_json::from_str(&viewed.stdout).map_err(|_| {
        (
            "failed",
            "GitHub returned an unreadable pull request.".to_owned(),
        )
    })?;
    let pr = crate::pull_requests::pull_request(&value)
        .filter(|pr| pr.url.eq_ignore_ascii_case(&url))
        .ok_or((
            "failed",
            "GitHub returned a different pull request.".to_owned(),
        ))?;
    let slug = address.slug();
    let repo = run(
        gh,
        view,
        &["repo", "view", &slug, "--json", REPO_VIEW_FIELDS],
        NETWORK_TIMEOUT,
    )
    .await?;
    let settings: Value = if repo.success {
        serde_json::from_str(&repo.stdout).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let allowed = |field: &str| settings.get(field).and_then(Value::as_bool);
    let mut methods = Vec::new();
    for (field, method) in [
        ("squashMergeAllowed", "squash"),
        ("mergeCommitAllowed", "merge"),
        ("rebaseMergeAllowed", "rebase"),
    ] {
        if allowed(field) != Some(false) {
            methods.push(method);
        }
    }
    let default_method = settings
        .get("viewerDefaultMergeMethod")
        .and_then(Value::as_str)
        .and_then(method_name)
        .filter(|m| methods.contains(m))
        .or_else(|| methods.first().copied());
    let text = |field: &str| value.get(field).and_then(Value::as_str).map(str::to_owned);
    let mergeable = text("mergeable").unwrap_or_default();
    let merge_status = text("mergeStateStatus").unwrap_or_default();
    let conflicting = mergeable == "CONFLICTING" || merge_status == "DIRTY";
    let auto_merge = value.get("autoMergeRequest").is_some_and(|v| !v.is_null());
    let base_ref = text("baseRefName");
    let base_name = base_ref.clone().unwrap_or_else(|| "its base".into());
    let checks = pr.checks.state;
    let blocked: Option<(&'static str, String)> = match pr.state {
        "merged" => Some(("merged", "This pull request is already merged.".into())),
        "closed" => Some(("closed", "This pull request is closed.".into())),
        "draft" => Some((
            "draft",
            "This is a draft. Mark it ready for review on GitHub before merging.".into(),
        )),
        _ if conflicting => Some((
            "conflicts",
            format!("This conflicts with {base_name}. Resolve the conflicts first."),
        )),
        _ if checks == "failing" => Some((
            "checksFailing",
            "Some checks are failing. Fix them first, or merge on GitHub if they aren't required.".into(),
        )),
        _ if checks == "pending" => Some((
            "checksPending",
            "Checks are still running. Turn on auto-merge to merge when they pass.".into(),
        )),
        _ if merge_status == "BEHIND" => Some((
            "behind",
            format!("This branch is behind {base_name}. Update it first."),
        )),
        _ if merge_status == "BLOCKED" => Some((
            "blocked",
            "GitHub is blocking this merge (a required review or check). Turn on auto-merge or open GitHub.".into(),
        )),
        _ if mergeable == "UNKNOWN" || mergeable.is_empty() => Some((
            "unknown",
            "GitHub is still working out whether this can merge. Try again in a moment.".into(),
        )),
        _ if methods.is_empty() => Some((
            "methodNotAllowed",
            "This repository allows no merge method Cupola can use.".into(),
        )),
        _ => None,
    };
    Ok(MergeState {
        version: VERSION,
        number: pr.number,
        url: pr.url,
        state: pr.state,
        head_ref: text("headRefName"),
        base_ref,
        head_oid: pr.head_oid.clone(),
        checks,
        conflicting,
        auto_merge,
        methods,
        default_method,
        blocked_reason: blocked.as_ref().map(|(code, _)| *code),
        blocked: blocked.map(|(_, message)| message),
    })
}

async fn merge(
    view: &WriteView,
    slugs: &[String],
    kind: Kind,
    request: &ActionRequest,
    gh: &Gh,
) -> Result<Outcome, Refusal> {
    let address = owned_address(request.pull_request_url.as_deref(), slugs)?;
    let gh = gh.ready()?;
    let url = address.url();
    let number = address.number;
    if kind == Kind::AutoMerge && request.enabled == Some(false) {
        let disabled = run(
            gh,
            view,
            &["pr", "merge", &url, "--disable-auto"],
            NETWORK_TIMEOUT,
        )
        .await?;
        if !disabled.success {
            return Err(explain(kind, &disabled));
        }
        return Ok(Outcome::done(
            kind,
            format!("Auto-merge is off for #{number}."),
        ));
    }
    if kind == Kind::AutoMerge && request.enabled != Some(true) {
        return refuse("invalid", "Choose whether auto-merge is on or off.");
    }
    let pinned = match request.expected_head_oid.as_deref() {
        Some(oid) if crate::pull_requests::valid_oid(oid) => Some(oid.to_ascii_lowercase()),
        Some(_) => return refuse("invalid", "Choose a valid commit."),
        None => None,
    };
    // Fresh state: refuse with a reason rather than let gh decide.
    let state = merge_state(view, gh, &address).await?;
    if let Some(oid) = &pinned {
        if state.head_oid.as_ref() != Some(oid) {
            return refuse(
                "headChanged",
                "New commits arrived since you opened this pull request. Review them, then merge.",
            );
        }
    }
    let method = match request.method.as_deref() {
        Some(value) => {
            method_name(value).ok_or(("invalid", "Choose a merge method.".to_owned()))?
        }
        None => state
            .default_method
            .ok_or(("methodNotAllowed", "Choose a merge method.".to_owned()))?,
    };
    if !state.methods.contains(&method) {
        return refuse(
            "methodNotAllowed",
            format!(
                "This repository doesn't allow {}.",
                method_title(method).to_lowercase()
            ),
        );
    }
    let flag = format!("--{method}");
    // GitHub itself refuses the merge if the head moves after this check.
    let mut merge_args = vec!["pr", "merge", url.as_str()];
    if kind == Kind::AutoMerge {
        merge_args.push("--auto");
    }
    merge_args.push(&flag);
    if let Some(oid) = &pinned {
        merge_args.extend(["--match-head-commit", oid.as_str()]);
    }
    if kind == Kind::Merge {
        if let (Some(code), Some(message)) = (state.blocked_reason, state.blocked.clone()) {
            return refuse(code, message);
        }
        let merged = run(gh, view, &merge_args, NETWORK_TIMEOUT).await?;
        if !merged.success {
            return Err(explain(kind, &merged));
        }
        return Ok(Outcome::done(
            kind,
            format!(
                "Merged #{number} ({}).",
                method_title(method).to_lowercase()
            ),
        ));
    }
    if let Some(code @ ("merged" | "closed" | "draft" | "conflicts")) = state.blocked_reason {
        return refuse(code, state.blocked.unwrap_or_default());
    }
    if state.auto_merge {
        return Ok(Outcome::done(
            kind,
            format!("Auto-merge is already on for #{number}."),
        ));
    }
    let enabled = run(gh, view, &merge_args, NETWORK_TIMEOUT).await?;
    if !enabled.success {
        let lower = enabled.text().to_ascii_lowercase();
        if lower.contains("auto merge is not allowed")
            || lower.contains("auto-merge is not allowed")
            || lower.contains("enablepullrequestautomerge")
        {
            return refuse(
                "autoMergeNotAllowed",
                "Auto-merge is turned off for this repository. Turn it on in its GitHub settings.",
            );
        }
        if lower.contains("clean status") {
            return refuse(
                "clean",
                "This pull request can merge now, so GitHub won't wait. Use Merge instead.",
            );
        }
        return Err(explain(kind, &enabled));
    }
    Ok(Outcome::done(
        kind,
        format!(
            "Auto-merge is on: #{number} will {} when its requirements pass.",
            match method {
                "squash" => "squash and merge",
                "rebase" => "rebase and merge",
                _ => "merge",
            }
        ),
    ))
}

// -------------------------------------------------------------------------
// HTTP
// -------------------------------------------------------------------------

/// One action at a time per repository.
static BUSY: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

pub(super) struct RepoLock(PathBuf);

impl RepoLock {
    pub(super) fn acquire(top: &Path) -> Option<Self> {
        BUSY.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(top.to_path_buf())
            .then(|| Self(top.to_path_buf()))
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        BUSY.lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// A repository the thread may write: named by its index id, resolved inside
/// a Project folder through the same checks as the Code changes readers.
pub(super) struct Admitted {
    top: PathBuf,
    slugs: Vec<String>,
}

/// Why a request was not admitted: an HTTP error (owner, conversation,
/// folder access) or a plain-language refusal.
enum NotAdmitted {
    Http(Box<Response>),
    Refused(Refusal),
}

async fn admit(
    state: &crate::AppState,
    conversation: &str,
    repository_id: &str,
    root_id: &str,
    repo: &str,
) -> Result<Admitted, NotAdmitted> {
    let http =
        |status, detail: String| NotAdmitted::Http(Box::new(workspace_error(status, detail)));
    let refused =
        |reason: &'static str, message: &str| NotAdmitted::Refused((reason, message.to_owned()));
    let (roots, _) = conversation_workspace_roots(state, conversation, false)
        .await
        .map_err(|(status, detail)| http(status, detail))?;
    let root = root_for(&roots, root_id)
        .map_err(|(status, detail)| http(status, detail.to_owned()))?
        .clone();
    let resolved = workspace_git::resolve(state, conversation, Some(root_id), Some(repo))
        .await
        .map_err(|response| NotAdmitted::Http(Box::new(response)))?;
    let context = match resolved {
        workspace_git::Resolved::Ready(context) => context,
        workspace_git::Resolved::NotRepository => {
            return Err(refused(
                "notRepository",
                "This folder is no longer a Git repository.",
            ))
        }
        workspace_git::Resolved::Unavailable(detail) => {
            return Err(refused("unavailable", &detail))
        }
    };
    let top = context.repository().to_path_buf();
    if !root.is_directory || !top.starts_with(&root.path) {
        return Err(refused(
            "outsideProject",
            "This repository reaches outside the Project's folders, so Cupola keeps it read only. Use Git on your computer.",
        ));
    }
    let denied = denied_paths(&state.denied_roots)
        .map_err(|(status, detail)| http(status, detail.to_owned()))?;
    if let Err(uncontained) = workspace_git::admit_write(&top, &root.path, &denied).await {
        return Err(NotAdmitted::Refused(uncontained_refusal(&uncontained)));
    }
    if format!("local:{}", thread_repos::short_hash(&top.to_string_lossy())) != repository_id {
        return Err(refused(
            "stale",
            "This repository changed on your computer. Refresh Code changes and try again.",
        ));
    }
    let slugs = remote_slugs(&top).await;
    Ok(Admitted { top, slugs })
}

async fn remote_slugs(top: &Path) -> Vec<String> {
    let Some(text) = top.to_str() else {
        return Vec::new();
    };
    let Ok(text) = read_only_git_bytes(text, &["config", "--get-regexp", r"^remote\..*\.url$"])
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    else {
        return Vec::new();
    };
    let mut slugs = Vec::new();
    for line in text.lines().take(32) {
        if let Some(slug) = line
            .split_once(' ')
            .and_then(|(_, url)| repo_records::remote_slug(url.trim()))
        {
            if !slugs.contains(&slug) {
                slugs.push(slug);
            }
        }
    }
    slugs
}

async fn gh_now() -> Gh {
    match crate::pull_requests::gh_for_action().await {
        Ok(gh) => Gh::Ready(gh),
        Err(false) => Gh::Missing,
        Err(true) => Gh::SignedOut,
    }
}

pub(crate) async fn repository_action(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Json(request): Json<ActionRequest>,
) -> Response {
    let Some(kind) = Kind::parse(&request.action) else {
        return Outcome::refused(Kind::Commit, "invalid", "Choose a Git action.").response();
    };
    let admitted = match admit(
        &state,
        &conversation_id,
        &request.repository_id,
        &request.root,
        &request.repo,
    )
    .await
    {
        Ok(admitted) => admitted,
        Err(NotAdmitted::Http(response)) => return *response,
        Err(NotAdmitted::Refused((reason, message))) => {
            return Outcome::refused(kind, reason, message).response()
        }
    };
    // The agent may be changing this repository right now.
    if state
        .store
        .conversation_has_active_turn(&conversation_id)
        .await
        .unwrap_or(true)
    {
        return Outcome::refused(
            kind,
            "agentRunning",
            "The agent is still working in this chat and may be changing this repository. Wait for its reply, or stop it, then try again.",
        )
        .response();
    }
    let Some(_lock) = RepoLock::acquire(&admitted.top) else {
        return Outcome::refused(
            kind,
            "busy",
            "Another Git action is still running in this repository. Wait for it to finish.",
        )
        .response();
    };
    let gh = if kind.needs_gh() {
        gh_now().await
    } else {
        Gh::Missing
    };
    let outcome = perform(&admitted.top, &admitted.slugs, &request, &gh).await;
    record(&state, &conversation_id, &admitted.top, &outcome).await;
    outcome.response()
}

/// After a successful action: a new pull request is linked to the chat, so
/// its index lists it as opened here, and the chat's cached reads are dropped.
async fn record(state: &crate::AppState, conversation: &str, top: &Path, outcome: &Outcome) {
    if !outcome.ok {
        return;
    }
    if let Some(created) = &outcome.pull_request {
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let _ = state
            .store
            .link_thread_pull_request(conversation, &created.url, &top.to_string_lossy(), &now)
            .await;
    }
    thread_repos::forget(conversation);
    crate::pull_requests::forget(conversation);
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MergeStateQuery {
    repository_id: String,
    root: String,
    #[serde(default)]
    repo: String,
    url: String,
}

pub(crate) async fn pull_request_merge_state(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<MergeStateQuery>,
) -> Response {
    let admitted = admit(
        &state,
        &conversation_id,
        &query.repository_id,
        &query.root,
        &query.repo,
    )
    .await;
    let refused = |(reason, message): Refusal| {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "reason": reason, "message": message })),
        )
            .into_response()
    };
    let admitted = match admitted {
        Ok(admitted) => admitted,
        Err(NotAdmitted::Http(response)) => return *response,
        Err(NotAdmitted::Refused(refusal)) => return refused(refusal),
    };
    let address = match owned_address(Some(&query.url), &admitted.slugs) {
        Ok(address) => address,
        Err(refusal) => return refused(refusal),
    };
    let gh = gh_now().await;
    let gh = match gh.ready() {
        Ok(gh) => gh.to_path_buf(),
        Err(refusal) => return refused(refusal),
    };
    let view = match WriteView::prepare(&admitted.top, false).await {
        Ok(view) => view,
        Err(refusal) => return refused(refusal),
    };
    match merge_state(&view, &gh, &address).await {
        Ok(state) => Json(state).into_response(),
        Err(refusal) => refused(refusal),
    }
}

#[cfg(all(test, unix))]
#[path = "git_actions_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
mod recovery_tests {
    use super::*;

    fn git_at(top: &Path, args: &[&str]) -> String {
        let output = crate::test_git::git()
            .arg("-C")
            .arg(top)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    async fn committed_view(top: &Path) -> WriteView {
        git_at(top, &["init", "-q", "-b", "main"]);
        git_at(top, &["config", "user.name", "Recovery Test"]);
        git_at(top, &["config", "user.email", "recovery@example.com"]);
        git_at(top, &["config", "commit.gpgsign", "false"]);
        fs::write(top.join("file"), "initial\n").unwrap();
        git_at(top, &["add", "file"]);
        git_at(top, &["commit", "-qm", "Initial"]);
        let view = WriteView::prepare(top, true).await.unwrap();
        fs::write(top.join("file"), "changed\n").unwrap();
        let request = ActionRequest {
            action: "commit".into(),
            message: Some("Recovery must survive".into()),
            include_unstaged: Some(true),
            ..Default::default()
        };
        assert!(commit(&view, &request).await.unwrap().ok);
        view
    }

    // Contract: even when another Git owns a ref lock beyond our wait,
    // dropping an action/checkpoint view must leave its recovery entries
    // either in the repository or durably in Cupola's private directory.
    #[tokio::test]
    async fn reflog_lock_contention_preserves_pending_entries() {
        for relative in ["HEAD", "refs/heads/main"] {
            for explicit in [false, true] {
                for log_lock in [false, true] {
                    let dir = tempfile::tempdir().unwrap();
                    let top = crate::canonical_path::canonicalize(dir.path()).unwrap();
                    let view = committed_view(&top).await;
                    let private = view._private.common.path().to_path_buf();
                    let pending = private.join("logs").join(relative);
                    let entries = fs::read(&pending).unwrap();
                    let existing = fs::read(top.join(".git/logs").join(relative)).unwrap();
                    let base = if log_lock { ".git/logs" } else { ".git" };
                    let lock = top.join(base).join(format!("{relative}.lock"));
                    let held = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&lock)
                        .unwrap();
                    if explicit {
                        let (reason, message) = view.finish().await.unwrap_err();
                        assert_eq!(reason, "unavailable");
                        assert!(message.contains(private.to_str().unwrap()), "{message}");
                        assert!(message.contains("before repeating the action"), "{message}");
                    } else {
                        drop(view);
                    }
                    assert_eq!(
                        fs::read(&pending).ok(),
                        Some(entries),
                        "pending {relative} recovery was deleted under lock contention"
                    );
                    assert_eq!(
                        fs::read(top.join(".git/logs").join(relative)).unwrap(),
                        existing
                    );
                    let other = if relative == "HEAD" {
                        "refs/heads/main"
                    } else {
                        "HEAD"
                    };
                    assert!(
                        !private.join("logs").join(other).exists(),
                        "already-merged entries stayed pending"
                    );
                    let destinations: Value =
                        serde_json::from_slice(&fs::read(private.join("RECOVERY.json")).unwrap())
                            .unwrap();
                    assert_eq!(
                        destinations["gitDirectory"],
                        top.join(".git").to_str().unwrap()
                    );
                    assert_eq!(
                        destinations["commonDirectory"],
                        top.join(".git").to_str().unwrap()
                    );
                    for entry in fs::read_dir(&private).unwrap() {
                        assert!(!entry.unwrap().file_type().unwrap().is_symlink());
                    }
                    // The other process's lock must survive our cleanup.
                    assert!(lock.exists());
                    drop(held);
                    fs::remove_file(lock).unwrap();
                    fs::remove_dir_all(private).unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn directory_replacement_preserves_recovery_without_writing_victim() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().join("project");
        let victim = dir.path().join("victim");
        fs::create_dir(&top).unwrap();
        fs::create_dir(&victim).unwrap();
        let top = crate::canonical_path::canonicalize(top).unwrap();
        let victim_view = committed_view(&victim).await;
        drop(victim_view);
        let before = fs::read(victim.join(".git/logs/HEAD")).unwrap();
        let view = committed_view(&top).await;
        let private = view._private.common.path().to_path_buf();
        let pending = private.join("logs/HEAD");
        let entries = fs::read(&pending).unwrap();
        fs::rename(top.join(".git"), top.join("aside")).unwrap();
        std::os::unix::fs::symlink(victim.join(".git"), top.join(".git")).unwrap();
        drop(view);
        assert_eq!(fs::read(victim.join(".git/logs/HEAD")).unwrap(), before);
        assert_eq!(
            fs::read(&pending).ok(),
            Some(entries),
            "replacement destroyed the pending recovery history"
        );
        fs::remove_dir_all(private).unwrap();
    }

    // The copy fallback's held files must use the same descriptor boundary
    // as reflogs; its destructor is also a Git write.
    #[tokio::test]
    async fn copied_metadata_never_writes_through_replaced_paths() {
        for replace_directory in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let top = dir.path().join("project");
            let victim = dir.path().join("victim");
            fs::create_dir(&top).unwrap();
            fs::create_dir(&victim).unwrap();
            let top = crate::canonical_path::canonicalize(top).unwrap();
            drop(committed_view(&top).await);
            let view = WriteView::prepare_reaching(&top, true, &[Reach::Copies])
                .await
                .unwrap();
            fs::write(
                view._private.common.path().join("shallow"),
                "new boundary\n",
            )
            .unwrap();
            fs::write(victim.join("shallow"), "other repository\n").unwrap();
            fs::write(victim.join("shallow.lock"), "other lock\n").unwrap();
            if replace_directory {
                fs::rename(top.join(".git"), top.join("aside")).unwrap();
                std::os::unix::fs::symlink(&victim, top.join(".git")).unwrap();
            } else {
                fs::remove_file(top.join(".git/shallow.lock")).unwrap();
                fs::hard_link(victim.join("shallow"), top.join(".git/shallow.lock")).unwrap();
            }
            drop(view);
            assert_eq!(
                fs::read_to_string(victim.join("shallow")).unwrap(),
                "other repository\n",
                "directory replacement: {replace_directory}"
            );
            assert_eq!(
                fs::read_to_string(victim.join("shallow.lock")).unwrap(),
                "other lock\n"
            );
        }
    }
}
