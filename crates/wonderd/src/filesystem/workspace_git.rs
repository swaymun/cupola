//! Branch and change summaries for a conversation's workspace: which branch or
//! worktree it is on, how far it is from its base, and the files changed since
//! the last commit or since the branch left its base. Everything here is read
//! only (never fetches, never writes the index) and runs through the bounded
//! `read_only_git_bytes` boundary, inside the same verified root, deny list and
//! repository containment as Files' Modified view.
use super::*;
use std::collections::HashMap;

static GIT_READ_DATA: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Bind read views to daemon-owned data before serving requests. This directory
/// must be outside sandboxed agents' writable roots (as for the database and
/// secrets). Owner-only modes do not isolate processes with the same uid.
/// Never choose this location from a repository, request or TMPDIR.
pub fn configure_git_reads(data_dir: &Path) -> io::Result<()> {
    let path = crate::canonical_path::canonicalize(data_dir)?;
    if let Err(path) = GIT_READ_DATA.set(path) {
        if GIT_READ_DATA.get() != Some(&path) {
            return Err(io::Error::other(
                "The Git reader's data directory is already configured.",
            ));
        }
    }
    Ok(())
}

fn git_read_data() -> Result<PathBuf, String> {
    #[cfg(test)]
    {
        if let Ok(path) = TEST_DATA_DIR.try_with(Clone::clone) {
            return Ok(path);
        }
        // Library tests have no daemon startup or real owner data. Their
        // synthetic data root has the same lifetime as the test process.
        static DATA: std::sync::LazyLock<tempfile::TempDir> = std::sync::LazyLock::new(|| {
            tempfile::Builder::new()
                .prefix("wonder-test-data-")
                .tempdir()
                .unwrap()
        });
        Ok(DATA.path().to_path_buf())
    }
    #[cfg(not(test))]
    GIT_READ_DATA.get().cloned().ok_or_else(|| {
        "Cupola's Git reader is unavailable. Restart Cupola on your computer.".into()
    })
}

#[cfg(test)]
tokio::task_local! {
    static TEST_GIT: PathBuf;
    static TEST_DATA_DIR: PathBuf;
}

fn git_program() -> std::ffi::OsString {
    #[cfg(test)]
    if let Ok(path) = TEST_GIT.try_with(Clone::clone) {
        return path.into_os_string();
    }
    "git".into()
}

/// Keep the private common directory alive until the child has finished.
pub(crate) struct GitReadCommand {
    pub(crate) command: tokio::process::Command,
    _common: Option<tempfile::TempDir>,
}

fn attribute_read(args: &[&str]) -> bool {
    matches!(args.first(), Some(&("diff" | "status" | "log" | "show")))
}

/// Every Git reader shares these overrides, including the metadata reads used
/// to prepare the private view. No config pre-check is a security boundary.
fn git_command(path: &str, args: &[&str], attr_source: Option<&str>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(git_program());
    command
        .arg("--no-pager")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "diff.external=false",
            "-c",
            "diff.trustExitCode=false",
            "-c",
            "diff.submodule=short",
            "-c",
            "status.submoduleSummary=false",
            "-c",
            "submodule.recurse=false",
            "-c",
            "log.showSignature=false",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_ATTR_SOURCE")
        .env("GIT_ATTR_NOSYSTEM", "1")
        // Older Git lacks GIT_NO_LAZY_FETCH. No read may start a transport,
        // including one that repository config explicitly marks as allowed.
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(tree) = attr_source {
        command.arg(format!("--attr-source={tree}"));
    }
    if let Some((verb, rest)) = args.split_first() {
        command.arg(verb);
        if matches!(*verb, "diff" | "log" | "show") {
            command.args(["--no-ext-diff", "--no-textconv"]);
        }
        if matches!(*verb, "diff" | "status") {
            // Even short diffs/status otherwise inspect a submodule's dirty
            // files, loading its unchecked filters. A command-line option
            // also overrides submodule.<name>.ignore in the parent config.
            // Gitlink commit changes remain visible.
            command.arg("--ignore-submodules=dirty");
        }
        command.args(rest);
    }
    command
}

/// The caller supplies a verified repository root and trusted arguments. Only
/// built-ins used by these readers are admitted; a new alias/helper cannot
/// accidentally turn a read into execution.
pub(crate) async fn read_only_git_command(
    path: &str,
    args: &[&str],
) -> Result<GitReadCommand, String> {
    if !matches!(
        args.first(),
        Some(
            &("config"
                | "rev-parse"
                | "symbolic-ref"
                | "hash-object"
                | "ls-files"
                | "for-each-ref"
                | "rev-list"
                | "merge-base"
                | "remote"
                | "cat-file"
                | "diff"
                | "status"
                | "log"
                | "show")
        )
    ) {
        return Err("This Git operation is not available for workspace reads.".into());
    }
    if !attribute_read(args) {
        return Ok(GitReadCommand {
            command: git_command(path, args, None),
            _common: None,
        });
    }
    // Attribute names cannot bind a program without its config definition.
    // The private common config is the boundary on every Git version; modern
    // Git can additionally ignore worktree/index attributes with --attr-source.
    let version = run_metadata(path, &["--version"]).await?;
    let version = String::from_utf8_lossy(&version);
    let mut numbers = version
        .strip_prefix("git version ")
        .unwrap_or("")
        .split('.');
    let major = numbers
        .next()
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = numbers
        .next()
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0);
    let supports_attr_source = (major, minor) >= (2, 41);

    // --attr-source ignores worktree AND index attributes, but Git still reads
    // $GIT_COMMON_DIR/info/attributes. Give it a private common directory with
    // only data: a ref snapshot and a small allowlist of display settings.
    // Never copy includes, driver definitions, hooks, promisor remotes or info.
    // The original gitdir keeps HEAD and split-index files working; its config
    // and config.worktree are not loaded (worktreeConfig is never enabled).
    let layout = git_layout(path).await?;
    let (format_config, empty_tree, oid_len) = if layout.sha256 {
        (
            "[extensions]\nobjectFormat = sha256\n",
            "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321",
            64,
        )
    } else {
        ("", "4b825dc642cb6eb9a060e54bf8d69288fbee4904", 40)
    };
    let refs = run_metadata(path, &["for-each-ref", "--format=%(objectname) %(refname)"])
        .await
        .map_err(|error| {
            if error.contains("exceeded its limit") {
                "This repository's reference list is too large to read here. Inspect its changes on your computer.".into()
            } else {
                error
            }
        })?;
    let refs = String::from_utf8(refs).map_err(|_| "Git references are not valid text.")?;
    for line in refs.lines() {
        let valid = line.split_once(' ').is_some_and(|(oid, name)| {
            oid.len() == oid_len
                && oid.bytes().all(|b| b.is_ascii_hexdigit())
                && name.starts_with("refs/")
                && !name.chars().any(|c| c.is_whitespace() || c.is_control())
        });
        if !valid {
            return Err("Git references could not be read safely.".into());
        }
    }
    let settings = repository_settings(path).await?;
    let mut config = format!(
        "[core]\nrepositoryFormatVersion = {}\nbare = false\n{format_config}",
        u8::from(layout.sha256)
    );
    for entry in settings.split(|b| *b == 0) {
        let mut parts = entry.splitn(2, |b| *b == b'\n');
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let key = String::from_utf8_lossy(key).to_ascii_lowercase();
        // Reftable stores HEAD in its ref database rather than a usable HEAD
        // file. Refuse instead of returning a misleading empty changes list.
        if key == "extensions.refstorage" && value != b"files" {
            return Err("This repository's reference format is not available for workspace reads. Inspect it on your computer.".into());
        }
        let value = String::from_utf8_lossy(value).to_ascii_lowercase();
        let safe = match key.as_str() {
            "core.filemode"
            | "core.ignorecase"
            | "core.symlinks"
            | "core.quotepath"
            | "core.precomposeunicode" => matches!(
                value.as_str(),
                "true" | "false" | "yes" | "no" | "on" | "off" | "0" | "1"
            ),
            "core.autocrlf" => matches!(value.as_str(), "true" | "false" | "input"),
            "core.eol" => matches!(value.as_str(), "lf" | "crlf" | "native"),
            _ => false,
        };
        if safe {
            config.push_str(&format!(
                "[core]\n{} = {value}\n",
                key.strip_prefix("core.").unwrap()
            ));
        }
    }
    let common = private_git_directory("wonder-git-read-")?;
    for directory in ["refs", "objects"] {
        tokio::fs::create_dir(common.path().join(directory))
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::write(common.path().join("config"), config)
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::write(common.path().join("packed-refs"), refs)
        .await
        .map_err(|e| e.to_string())?;
    // --git-path with absolute formatting resolves the final symlink. Bind
    // this name ourselves so O_NOFOLLOW still applies to the source file.
    let shallow = Path::new(&layout.common_dir).join("shallow");
    // Shallow boundaries are data too. Preserve them without an unbounded read.
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    match options.open(shallow).await {
        Ok(file) => {
            use tokio::io::AsyncReadExt;
            if !file.metadata().await.map_err(|e| e.to_string())?.is_file() {
                return Err("This repository's Git history metadata is not a regular file. Inspect it on your computer.".into());
            }
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|e| e.to_string())?;
            if bytes.len() > 1024 * 1024 {
                return Err("Git shallow history exceeded its read limit.".into());
            }
            tokio::fs::write(common.path().join("shallow"), bytes)
                .await
                .map_err(|e| e.to_string())?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let mut command = git_command(path, args, supports_attr_source.then_some(empty_tree));
    command
        .env("GIT_DIR", &layout.git_dir)
        .env("GIT_COMMON_DIR", common.path())
        .env("GIT_OBJECT_DIRECTORY", &layout.objects)
        .env(
            "GIT_WORK_TREE",
            crate::canonical_path::canonicalize_async(path)
                .await
                .map_err(|e| e.to_string())?,
        );
    Ok(GitReadCommand {
        command,
        _common: Some(common),
    })
}

pub(crate) async fn read_only_git_bytes(path: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let prepared = read_only_git_command(path, args).await?;
    run_git_command(prepared.command).await
}

/// Where a repository keeps its Git data, read without loading a program.
pub(super) struct GitLayout {
    pub(super) git_dir: String,
    pub(super) objects: String,
    pub(super) common_dir: String,
    pub(super) sha256: bool,
}

pub(super) async fn git_layout(path: &str) -> Result<GitLayout, String> {
    let locations = run_metadata(
        path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-path",
            "objects",
            "--git-common-dir",
            "--show-object-format",
        ],
    )
    .await?;
    let locations = String::from_utf8(locations).map_err(|_| "Git paths are not valid text.")?;
    let locations: Vec<_> = locations.lines().collect();
    let [git_dir, objects, common_dir, format] = locations.as_slice() else {
        return Err("Git repository paths could not be read safely.".into());
    };
    let sha256 = match *format {
        "sha1" => false,
        "sha256" => true,
        _ => {
            return Err(
                "This repository's object format is not available for workspace reads.".into(),
            )
        }
    };
    Ok(GitLayout {
        git_dir: (*git_dir).to_owned(),
        objects: (*objects).to_owned(),
        common_dir: (*common_dir).to_owned(),
        sha256,
    })
}

/// Why a repository's Git data was not admitted for writing.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Uncontained {
    /// Git couldn't describe the repository.
    Unreadable,
    /// Its Git data, or a link inside it, reaches past the work tree's own.
    Outside,
}

/// Entries Git writes in a Git directory, with how many levels below each Git
/// creates files in a directory it finds there. Another repository's files
/// reached through a linked directory would take the writes. (A linked file is
/// replaced, not written through: Git renames a lock file over it.)
const WRITTEN_DIRECTORIES: &[(&str, usize)] = &[
    ("refs", usize::MAX),
    ("logs", usize::MAX),
    ("objects", 1),
    ("info", 0),
    ("lfs", 2),
    ("worktrees", 1),
];
/// Bounds the directories walked; no real repository nears it.
const WALK_LIMIT: usize = 100_000;

/// The layout of the repository whose work tree is `top`, canonical, admitted
/// for writing only when its Git data is its own. That is a `.git` directory
/// at the top; or a `.git` file naming a Git directory inside the work tree,
/// or one whose own setting names this work tree back (an absorbed submodule),
/// or a linked worktree whose back-link (which `git worktree add` writes in
/// the main repository) names this `.git` file. A `.git` file anyone can
/// write in the folder never admits another repository on its own. Objects
/// stay in the common directory, alternates inside the repository, and no
/// directory Git writes in is a link. Writers pin Git to these exact paths,
/// so a `.git` file or `commondir` changed after this check isn't followed.
pub(super) async fn contained_layout(top: &Path) -> Result<GitLayout, Uncontained> {
    let top = crate::canonical_path::canonicalize_async(top)
        .await
        .map_err(|_| Uncontained::Unreadable)?;
    let layout = git_layout(top.to_str().ok_or(Uncontained::Unreadable)?)
        .await
        .map_err(|_| Uncontained::Unreadable)?;
    let worktree = match git_setting(&layout.git_dir, "core.worktree").await {
        Ok(value) => Some(value),
        Err(None) => None,
        Err(Some(())) => return Err(Uncontained::Unreadable),
    };
    tokio::task::spawn_blocking(move || contain(&top, layout, worktree.as_deref()))
        .await
        .map_err(|_| Uncontained::Unreadable)?
}

/// Admits the repository at `top` for a write from the Project folder
/// `project`, for Git actions and checkpoints alike: the folder holds the
/// repository or is inside it, its Git data passes `contained_layout`, and
/// no Git directory it uses is protected by Cupola.
pub(super) async fn admit_write(
    top: &Path,
    project: &Path,
    denied: &[PathBuf],
) -> Result<(), Uncontained> {
    let canonical = |path: &Path| {
        crate::canonical_path::canonicalize(path).map_err(|_| Uncontained::Unreadable)
    };
    let (top, project) = (canonical(top)?, canonical(project)?);
    if !top.starts_with(&project) && !project.starts_with(&top) {
        return Err(Uncontained::Outside);
    }
    let layout = contained_layout(&top).await?;
    if [&layout.git_dir, &layout.common_dir]
        .into_iter()
        .any(|path| protected_by_denies(Path::new(path), denied, Some(&top)))
    {
        return Err(Uncontained::Outside);
    }
    Ok(())
}

/// One setting from a Git directory's own config file only. `Err(None)` when
/// unset, `Err(Some(()))` when unreadable.
async fn git_setting(git_dir: &str, key: &str) -> Result<String, Option<()>> {
    let file = Path::new(git_dir).join("config");
    let file = file.to_str().ok_or(Some(()))?;
    let mut command = git_command(git_dir, &["config", "--file", file, "--get", key], None);
    command.env("GIT_DIR", git_dir);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
        .await
        .map_err(|_| Some(()))?
        .map_err(|_| Some(()))?;
    match output.status.code() {
        Some(0) => String::from_utf8(output.stdout)
            .map(|value| value.trim_end_matches('\n').to_owned())
            .map_err(|_| Some(())),
        Some(1) => Err(None),
        _ => Err(Some(())),
    }
}

fn contain(
    top: &Path,
    layout: GitLayout,
    worktree: Option<&str>,
) -> Result<GitLayout, Uncontained> {
    let canonical = |path: &Path| {
        crate::canonical_path::canonicalize(path).map_err(|_| Uncontained::Unreadable)
    };
    let git_dir = canonical(Path::new(&layout.git_dir))?;
    let common = canonical(Path::new(&layout.common_dir))?;
    let objects = canonical(Path::new(&layout.objects))?;
    let dot = top.join(".git");
    let kind = fs::symlink_metadata(&dot)
        .map_err(|_| Uncontained::Outside)?
        .file_type();
    let own = if kind.is_dir() {
        git_dir == dot && common == dot
    } else if !kind.is_file() {
        false
    } else if git_dir == common {
        git_dir.starts_with(top)
            || worktree.is_some_and(|named| {
                crate::canonical_path::canonicalize(git_dir.join(named))
                    .is_ok_and(|named| named == top)
            })
    } else {
        git_dir.parent() == Some(common.join("worktrees").as_path())
            && read_small(&git_dir.join("gitdir"), 4096)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .is_some_and(|back| {
                    let back = Path::new(back.trim_end_matches(['\n', '\r']));
                    crate::canonical_path::canonicalize(git_dir.join(back))
                        .is_ok_and(|back| back == dot)
                })
    };
    if !own || objects != common.join("objects") {
        return Err(Uncontained::Outside);
    }
    let mut budget = WALK_LIMIT;
    let own_directory = (git_dir != common).then_some(&git_dir);
    for directory in std::iter::once(&common).chain(own_directory) {
        for (name, depth) in WRITTEN_DIRECTORIES {
            unlinked(&directory.join(name), *depth, &mut budget)?;
        }
    }
    let mut seen = Vec::new();
    alternates_inside(&objects, &[top, &common], &mut seen)?;
    Ok(GitLayout {
        git_dir: git_dir.to_str().ok_or(Uncontained::Unreadable)?.to_owned(),
        objects: objects.to_str().ok_or(Uncontained::Unreadable)?.to_owned(),
        common_dir: common.to_str().ok_or(Uncontained::Unreadable)?.to_owned(),
        sha256: layout.sha256,
    })
}

/// Refuses a link at `path` or at a directory up to `depth` levels below it;
/// absent is fine.
fn unlinked(path: &Path, depth: usize, budget: &mut usize) -> Result<(), Uncontained> {
    let kind = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.file_type(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(Uncontained::Unreadable),
    };
    if kind.is_symlink() {
        return Err(Uncontained::Outside);
    }
    if !kind.is_dir() || depth == 0 {
        return Ok(());
    }
    *budget = budget.checked_sub(1).ok_or(Uncontained::Unreadable)?;
    for entry in fs::read_dir(path).map_err(|_| Uncontained::Unreadable)? {
        let entry = entry.map_err(|_| Uncontained::Unreadable)?;
        let kind = entry.file_type().map_err(|_| Uncontained::Unreadable)?;
        if kind.is_symlink() && fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()) {
            return Err(Uncontained::Outside);
        }
        if kind.is_dir() {
            unlinked(&entry.path(), depth - 1, budget)?;
        }
    }
    Ok(())
}

/// Every alternate object directory, and theirs in turn, resolves inside one
/// of `inside`. Git reads alternates as written, relative to `objects`.
fn alternates_inside(
    objects: &Path,
    inside: &[&Path],
    seen: &mut Vec<PathBuf>,
) -> Result<(), Uncontained> {
    // Git itself stops following alternates past this depth.
    if seen.len() > 5 || seen.iter().any(|s| s == objects) {
        return Err(Uncontained::Outside);
    }
    seen.push(objects.to_path_buf());
    let bytes = match read_small(&objects.join("info").join("alternates"), 64 * 1024) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(Uncontained::Unreadable),
    };
    let text = String::from_utf8(bytes).map_err(|_| Uncontained::Outside)?;
    for line in text.lines().map(str::trim_end) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // A quoted entry may hold any byte; none of Cupola's needs one.
        if line.starts_with('"') {
            return Err(Uncontained::Outside);
        }
        let alternate = crate::canonical_path::canonicalize(objects.join(line))
            .map_err(|_| Uncontained::Outside)?;
        if !inside.iter().any(|root| alternate.starts_with(root)) {
            return Err(Uncontained::Outside);
        }
        alternates_inside(&alternate, inside, seen)?;
    }
    Ok(())
}

/// A small regular file, opened without following a link and bounded.
fn read_small(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    #[cfg(windows)]
    use std::os::windows::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("too large"));
    }
    Ok(bytes)
}

/// The repository's own settings (local and worktree scopes only: user and
/// system config are off, and the read boundary's own `-c` settings are
/// left out), as `key\nvalue` entries separated by NUL. Listing settings
/// never runs a program.
pub(super) async fn repository_settings(path: &str) -> Result<Vec<u8>, String> {
    settings_from(git_command(path, SETTINGS, None)).await
}

/// The settings of a `contained_layout`, read from its exact Git directory
/// and work tree rather than found again from `top`.
pub(super) async fn contained_settings(top: &Path, layout: &GitLayout) -> Result<Vec<u8>, String> {
    let path = top.to_str().ok_or("This repository path cannot be read.")?;
    let mut command = git_command(path, SETTINGS, None);
    command.envs(contained_env(top, layout));
    settings_from(command).await
}

/// Pins Git to a `contained_layout`: its Git, common and object directories
/// and the work tree at `top`, whatever the folder's `.git` says later.
pub(super) fn contained_env(
    top: &Path,
    layout: &GitLayout,
) -> [(&'static str, std::ffi::OsString); 4] {
    [
        ("GIT_DIR", (&layout.git_dir).into()),
        ("GIT_COMMON_DIR", (&layout.common_dir).into()),
        ("GIT_OBJECT_DIRECTORY", (&layout.objects).into()),
        ("GIT_WORK_TREE", top.into()),
    ]
}

const SETTINGS: &[&str] = &["config", "--null", "--list", "--show-scope"];

async fn settings_from(command: tokio::process::Command) -> Result<Vec<u8>, String> {
    let listed = run_git_command(command).await?;
    // Each entry follows its scope, both NUL-terminated.
    let mut own = Vec::new();
    let mut fields = listed.split(|b| *b == 0);
    while let (Some(scope), Some(entry)) = (fields.next(), fields.next()) {
        if matches!(scope, b"local" | b"worktree") {
            own.extend_from_slice(entry);
            own.push(0);
        }
    }
    Ok(own)
}

/// A new private common directory for one Git view.
pub(super) fn private_git_directory(prefix: &str) -> Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix(prefix)
        // Atomic creation with 0700 on Unix, under the daemon's protected
        // data root. A same-uid agent can replace a 0700 view in TMPDIR.
        .tempdir_in(git_read_data()?)
        .map_err(|e| e.to_string())
}

async fn run_metadata(path: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    debug_assert!(!attribute_read(args));
    run_git_command(git_command(path, args, None)).await
}

async fn run_git_command(mut command: tokio::process::Command) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("Git output unavailable")?;
    let stderr = child.stderr.take().ok_or("Git error output unavailable")?;
    let read = |stream: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>| async move {
        let mut bytes = Vec::new();
        stream
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(bytes)
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let (out, err, status) =
            tokio::try_join!(read(Box::pin(stdout)), read(Box::pin(stderr)), async {
                child.wait().await.map_err(|e| e.to_string())
            })?;
        if out.len() > 1024 * 1024 || err.len() > 1024 * 1024 {
            return Err(
                "Git output exceeded its limit. Inspect this change on the computer.".to_owned(),
            );
        }
        if !status.success() {
            return Err(git_failure(&String::from_utf8_lossy(&err)).to_owned());
        }
        Ok(out)
    })
    .await
    .map_err(|_| "Git operation timed out; inspect this workspace on the computer.".to_owned())?;
    if result.is_err() {
        let _ = child.kill().await;
    }
    result
}

/// What the phone is told when a Git read fails. Git's own text names paths
/// on the Mac and can carry remote URLs with credentials, so it stays in the
/// local log, redacted.
fn git_failure(stderr: &str) -> &'static str {
    eprintln!("git read failed: {}", redacted_git_detail(stderr));
    let lowered = stderr.to_ascii_lowercase();
    if lowered.contains("not a git repository") {
        "This folder is no longer a Git repository."
    } else if [
        "unknown revision",
        "bad revision",
        "bad object",
        "invalid object",
        "not a valid object",
        "ambiguous argument",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        "That branch or commit is no longer available. Reload the changes."
    } else if lowered.contains(".lock") {
        "Git is busy in this repository. Try again in a moment."
    } else if lowered.contains("permission denied") {
        "Your computer does not allow Cupola to read this repository."
    } else {
        "Git could not read these changes. Try again, or review them on your computer."
    }
}

/// Git's error text for the local log: bounded, without terminal escapes,
/// known token shapes or the user part of any URL. Credentials are removed
/// from the whole text before it is cut short: a URL split at the bound would
/// lose its `@` and keep its password.
fn redacted_git_detail(stderr: &str) -> String {
    let mut text = stderr.trim().to_owned();
    let mut offset = 0;
    while let Some(found) = text[offset..].find("://") {
        let start = offset + found + 3;
        // Userinfo ends at the host's `@`, before the path's first `/`. An
        // unencoded password may hold quotes or spaces, so only `/` or the end
        // of the line bounds the search; a stray `@` later on that line only
        // removes more text from a local log line.
        let end = text[start..]
            .find(['/', '\n'])
            .map_or(text.len(), |index| start + index);
        if let Some(at) = text[start..end].rfind('@') {
            text.replace_range(start..start + at, "[redacted]");
        }
        offset = start;
    }
    crate::history::sanitize_ansi_and_secrets(&text, 1500)
}

/// Rows returned in one change list; totals still cover every change.
const MAX_CHANGED_FILES: usize = 2_000;
/// Untracked files are counted by reading them. Larger files, or files past the
/// shared budget, are listed without line counts.
const MAX_UNTRACKED_COUNT_BYTES: u64 = 1024 * 1024;
const MAX_UNTRACKED_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
const TOO_LARGE_DIFF: &str = "This diff is too large to show here. Open it on your computer.";

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkspaceGitRootQuery {
    root: Option<String>,
    /// A repository inside the root, relative to it, from the thread's
    /// repository index. Without it, the repository holding the root.
    repo: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkspaceGitChangesQuery {
    root: Option<String>,
    repo: Option<String>,
    scope: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChangeScope {
    /// The working tree (staged and unstaged, plus untracked) against HEAD.
    Uncommitted,
    /// The working tree, including uncommitted work, against the merge base
    /// with the base branch, as Claude's diff view shows a branch.
    Branch,
}

impl ChangeScope {
    pub(super) fn parse(value: Option<&str>) -> Result<Self, BrowseError> {
        match value.unwrap_or("uncommitted") {
            "uncommitted" => Ok(Self::Uncommitted),
            "branch" => Ok(Self::Branch),
            _ => Err((
                StatusCode::BAD_REQUEST,
                "Choose uncommitted or branch changes.",
            )),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Uncommitted => "uncommitted",
            Self::Branch => "branch",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct GitTotals {
    files: usize,
    additions: u64,
    deletions: u64,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceGitSummary {
    available: bool,
    detail: Option<String>,
    /// False when the folder is not in a Git repository; the phone hides the chip.
    repository: bool,
    branch: Option<String>,
    detached: bool,
    head: Option<String>,
    worktree: bool,
    base_ref: Option<String>,
    merge_base: Option<String>,
    ahead: Option<u64>,
    behind: Option<u64>,
    uncommitted: Option<GitTotals>,
    branch_changes: Option<GitTotals>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChangedFile {
    path: String,
    original_path: Option<String>,
    status: &'static str,
    binary: bool,
    additions: Option<u64>,
    deletions: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceGitChanges {
    scope: &'static str,
    available: bool,
    detail: Option<String>,
    branch: Option<String>,
    base_ref: Option<String>,
    files: Vec<ChangedFile>,
    totals: GitTotals,
    truncated: bool,
}

#[derive(Clone)]
pub(super) struct RepoContext {
    root: WorkspaceRoot,
    denied: Vec<PathBuf>,
    directory: PathBuf,
    repository: PathBuf,
    pathspec: String,
}

impl RepoContext {
    /// The repository's top folder, verified inside the deny list.
    pub(super) fn repository(&self) -> &Path {
        &self.repository
    }
}

pub(super) enum Resolved {
    Ready(RepoContext),
    NotRepository,
    Unavailable(String),
}

#[allow(clippy::result_large_err)]
pub(super) async fn resolve(
    state: &crate::AppState,
    conversation_id: &str,
    root_id: Option<&str>,
    repo: Option<&str>,
) -> Result<Resolved, Response> {
    let (roots, _) = conversation_workspace_roots(state, conversation_id, false)
        .await
        .map_err(|(status, detail)| workspace_error(status, detail))?;
    let root_id = root_id
        .ok_or_else(|| workspace_error(StatusCode::BAD_REQUEST, "Choose a workspace location."))?;
    let mut root = root_for(&roots, root_id)
        .map_err(|(status, detail)| workspace_error(status, detail))?
        .clone();
    let denied = denied_paths(&state.denied_roots)
        .map_err(|(status, detail)| workspace_error(status, detail))?;
    // A nested repository is read as a root of its own: the same no-symlink,
    // deny-aware resolution as Files, then Git scoped to that folder.
    let relative =
        relative_path(repo).map_err(|(status, detail)| workspace_error(status, detail))?;
    if !relative.as_os_str().is_empty() {
        if !root.is_directory {
            return Ok(Resolved::NotRepository);
        }
        let own = Some(root.path.as_path());
        let folder = resolve_workspace_path(&root, &relative, &denied, own, false)
            .map_err(|(status, detail)| workspace_error(status, detail))?;
        if !fs::metadata(&folder).is_ok_and(|metadata| metadata.is_dir()) {
            return Ok(Resolved::NotRepository);
        }
        root = WorkspaceRoot {
            path: folder,
            ..root
        };
    }
    Ok(
        match verified_git_scope(&root, &denied, Some(root.path.as_path())).await {
            GitScope::NotRepository => Resolved::NotRepository,
            GitScope::Unavailable(detail) => Resolved::Unavailable(detail.to_owned()),
            GitScope::Ready {
                directory,
                repository,
                pathspec,
            } => {
                if repository.to_str().is_none() {
                    return Ok(Resolved::Unavailable(
                        "Git status is temporarily unavailable for this workspace.".into(),
                    ));
                }
                Resolved::Ready(RepoContext {
                    root,
                    denied,
                    directory,
                    repository,
                    pathspec,
                })
            }
        },
    )
}

async fn git(repository: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let path = repository
        .to_str()
        .ok_or_else(|| "This repository path cannot be read.".to_owned())?;
    crate::filesystem::read_only_git_bytes(path, args).await
}

async fn git_line(repository: &Path, args: &[&str]) -> Option<String> {
    let bytes = git(repository, args).await.ok()?;
    let line = String::from_utf8(bytes).ok()?.trim().to_owned();
    (!line.is_empty()).then_some(line)
}

async fn commit(repository: &Path, revision: &str) -> Option<String> {
    if revision.is_empty() || revision.starts_with('-') {
        return None;
    }
    let peeled = format!("{revision}^{{commit}}");
    git_line(
        repository,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &peeled,
        ],
    )
    .await
}

pub(super) struct Base {
    pub(super) label: String,
    pub(super) revision: String,
}

/// Compare a branch with the remote default, then main or master (local first).
pub(super) async fn base(repository: &Path) -> Option<Base> {
    let mut candidates = Vec::new();
    if let Some(remote_head) = git_line(
        repository,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .await
    {
        if let Some(label) = remote_head.strip_prefix("refs/remotes/") {
            candidates.push((label.to_owned(), remote_head.clone()));
        }
    }
    for (label, reference) in [
        ("main", "refs/heads/main"),
        ("master", "refs/heads/master"),
        ("origin/main", "refs/remotes/origin/main"),
        ("origin/master", "refs/remotes/origin/master"),
    ] {
        candidates.push((label.to_owned(), reference.to_owned()));
    }
    for (label, reference) in candidates {
        if let Some(revision) = commit(repository, &reference).await {
            return Some(Base { label, revision });
        }
    }
    None
}

async fn head(repository: &Path) -> Option<String> {
    commit(repository, "HEAD").await
}

/// The empty tree, for diffs in a repository with no commits yet. Asked of Git
/// so SHA-256 repositories get their own value.
async fn empty_tree(repository: &Path) -> Option<String> {
    git_line(repository, &["hash-object", "-t", "tree", "/dev/null"]).await
}

/// What a scope's diff is taken against, with the base for Branch.
pub(super) async fn scope_target(
    context: &RepoContext,
    scope: ChangeScope,
) -> Result<(String, Option<Base>), String> {
    let repository = context.repository.as_path();
    let head = head(repository).await;
    match scope {
        ChangeScope::Uncommitted => match head {
            Some(head) => Ok((head, None)),
            None => empty_tree(repository)
                .await
                .map(|tree| (tree, None))
                .ok_or_else(|| "Git status is temporarily unavailable for this workspace.".into()),
        },
        ChangeScope::Branch => {
            let Some(head) = head else {
                return Err("This branch has no commits yet.".into());
            };
            let Some(base) = base(repository).await else {
                return Err("There is no main branch to compare this branch with.".into());
            };
            let merge_base = git_line(repository, &["merge-base", &base.revision, &head])
                .await
                .ok_or_else(|| format!("This branch shares no history with {}.", base.label))?;
            Ok((merge_base, Some(base)))
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DiffEntry {
    path: String,
    original: Option<String>,
    status: u8,
    binary: bool,
    additions: Option<u64>,
    deletions: Option<u64>,
}

/// Parses `git diff --raw --numstat -z -M`: raw records (`:meta\0path\0`, or
/// `:meta\0from\0to\0` for renames and copies) come first, then numstat
/// records (`added\tdeleted\tpath\0`, or `added\tdeleted\t\0from\0to\0`).
fn parse_raw_numstat(bytes: &[u8]) -> Vec<DiffEntry> {
    let text = |value: &[u8]| String::from_utf8_lossy(value).into_owned();
    let mut fields = bytes.split(|byte| *byte == 0);
    let mut entries: Vec<DiffEntry> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        if field[0] == b':' {
            let status = field
                .rsplit(|byte| *byte == b' ')
                .next()
                .and_then(|status| status.first().copied())
                .unwrap_or(b'M');
            let Some(first) = fields.next().map(text) else {
                break;
            };
            let (path, original) = if status == b'R' || status == b'C' {
                let Some(second) = fields.next().map(text) else {
                    break;
                };
                (second, Some(first))
            } else {
                (first, None)
            };
            index.insert(path.clone(), entries.len());
            entries.push(DiffEntry {
                path,
                original,
                status,
                binary: false,
                additions: None,
                deletions: None,
            });
            continue;
        }
        let mut parts = field.splitn(3, |byte| *byte == b'\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let _from = fields.next();
            match fields.next() {
                Some(to) => text(to),
                None => break,
            }
        } else {
            text(path)
        };
        let Some(&position) = index.get(&path) else {
            continue;
        };
        let entry = &mut entries[position];
        if added == b"-" || deleted == b"-" {
            entry.binary = true;
        } else {
            entry.additions = std::str::from_utf8(added).ok().and_then(|v| v.parse().ok());
            entry.deletions = std::str::from_utf8(deleted)
                .ok()
                .and_then(|v| v.parse().ok());
        }
    }
    entries
}

fn status_name(status: u8) -> &'static str {
    match status {
        b'A' | b'C' => "added",
        b'D' => "deleted",
        b'R' => "renamed",
        _ => "modified",
    }
}

/// A repository-relative path as the root-relative path the phone may see, or
/// None when it is outside the root or protected. Existing paths get the full
/// no-symlink check Files uses; a deleted path is checked lexically.
fn visible_path(context: &RepoContext, repo_relative: &str) -> Option<String> {
    let relative = relative_path(Some(repo_relative)).ok()?;
    let full = context.repository.join(relative);
    let relative = full.strip_prefix(&context.directory).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    let own = Some(context.root.path.as_path());
    let lexical = context.root.path.join(relative);
    if protected_by_denies(&lexical, &context.denied, own) {
        return None;
    }
    if fs::symlink_metadata(&lexical).is_ok()
        && validated_git_path(&context.root, relative, &context.denied, own).is_err()
    {
        return None;
    }
    Some(relative.to_string_lossy().into_owned())
}

/// Counts an untracked file's lines through the same no-follow open as Files.
fn untracked_counts(
    context: &RepoContext,
    relative: &str,
    budget: &mut u64,
) -> (bool, Option<u64>) {
    let Ok(file) = open_workspace_file(&context.root, Path::new(relative)) else {
        return (false, None);
    };
    let Ok(metadata) = file.metadata() else {
        return (false, None);
    };
    if !metadata.is_file() || metadata.len() > MAX_UNTRACKED_COUNT_BYTES || metadata.len() > *budget
    {
        return (false, None);
    }
    *budget -= metadata.len();
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    if io::Read::take(file, MAX_UNTRACKED_COUNT_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return (false, None);
    }
    if bytes.iter().take(8000).any(|byte| *byte == 0) {
        return (true, None);
    }
    let mut lines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    if bytes.last().is_some_and(|byte| *byte != b'\n') {
        lines += 1;
    }
    (false, Some(lines))
}

fn list_error(error: &str) -> String {
    if error.contains("timed out") {
        "Git took too long to list these changes. Try again, or review them on your computer."
            .into()
    } else if error.contains("exceeded its limit") {
        "There are too many changes to list here. Review them on your computer.".into()
    } else {
        "Git status is temporarily unavailable for this workspace.".into()
    }
}

/// Every visible change in a scope, sorted by path, with totals over all of them.
async fn collect_changes(
    context: &RepoContext,
    target: &str,
) -> Result<(Vec<ChangedFile>, GitTotals), String> {
    let repository = context.repository.as_path();
    let diff = git(
        repository,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--raw",
            "--numstat",
            "-z",
            "-M",
            target,
            "--",
            &context.pathspec,
        ],
    )
    .await
    .map_err(|error| list_error(&error))?;
    let untracked = git(
        repository,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            &context.pathspec,
        ],
    )
    .await
    .map_err(|error| list_error(&error))?;
    let context = context.clone();
    tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        for entry in parse_raw_numstat(&diff) {
            let Some(path) = visible_path(&context, &entry.path) else {
                continue;
            };
            let original_path = match entry.original.as_deref() {
                Some(original) => match visible_path(&context, original) {
                    Some(original) => Some(original),
                    None => continue,
                },
                None => None,
            };
            files.push(ChangedFile {
                path,
                original_path,
                status: status_name(entry.status),
                binary: entry.binary,
                additions: entry.additions,
                deletions: entry.deletions,
            });
        }
        let mut budget = MAX_UNTRACKED_TOTAL_BYTES;
        for path in untracked
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let Some(path) = visible_path(&context, &String::from_utf8_lossy(path)) else {
                continue;
            };
            let (binary, additions) = untracked_counts(&context, &path, &mut budget);
            files.push(ChangedFile {
                path,
                original_path: None,
                status: "untracked",
                binary,
                additions,
                deletions: additions.map(|_| 0),
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let totals = GitTotals {
            files: files.len(),
            additions: files.iter().filter_map(|file| file.additions).sum(),
            deletions: files.iter().filter_map(|file| file.deletions).sum(),
        };
        (files, totals)
    })
    .await
    .map_err(|_| "Git status is temporarily unavailable for this workspace.".to_owned())
}

pub(crate) async fn workspace_git_summary(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<WorkspaceGitRootQuery>,
) -> Response {
    let context = match resolve(
        &state,
        &conversation_id,
        query.root.as_deref(),
        query.repo.as_deref(),
    )
    .await
    {
        Ok(Resolved::Ready(context)) => context,
        Ok(Resolved::NotRepository) => {
            return Json(WorkspaceGitSummary {
                available: true,
                ..Default::default()
            })
            .into_response()
        }
        Ok(Resolved::Unavailable(detail)) => {
            return Json(WorkspaceGitSummary {
                detail: Some(detail),
                ..Default::default()
            })
            .into_response()
        }
        Err(response) => return response,
    };
    let repository = context.repository.as_path();
    let directories = git(
        repository,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ],
    )
    .await
    .ok()
    .map(|bytes| {
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(|line| {
                crate::canonical_path::canonicalize(line).unwrap_or_else(|_| PathBuf::from(line))
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let worktree = directories.len() == 2 && directories[0] != directories[1];
    let branch = git_line(repository, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await;
    let head = head(repository).await;
    let base = match head {
        Some(_) => base(repository).await,
        None => None,
    };
    let mut summary = WorkspaceGitSummary {
        available: true,
        repository: true,
        detached: branch.is_none(),
        branch,
        head: head.clone(),
        worktree,
        base_ref: base.as_ref().map(|base| base.label.clone()),
        ..Default::default()
    };
    if let (Some(head), Some(base)) = (head.as_deref(), base.as_ref()) {
        summary.merge_base = git_line(repository, &["merge-base", &base.revision, head]).await;
        let range = format!("{}...{head}", base.revision);
        if let Some(counts) =
            git_line(repository, &["rev-list", "--left-right", "--count", &range]).await
        {
            let mut counts = counts.split_whitespace().map(|value| value.parse().ok());
            summary.behind = counts.next().flatten();
            summary.ahead = counts.next().flatten();
        }
    }
    let uncommitted_target = match head.clone() {
        Some(head) => Some(head),
        None => empty_tree(repository).await,
    };
    if let Some(target) = uncommitted_target {
        match collect_changes(&context, &target).await {
            Ok((_, totals)) => summary.uncommitted = Some(totals),
            Err(detail) => summary.detail = Some(detail),
        }
    }
    if let Some(merge_base) = summary.merge_base.clone() {
        if let Ok((_, totals)) = collect_changes(&context, &merge_base).await {
            summary.branch_changes = Some(totals);
        }
    }
    Json(summary).into_response()
}

pub(crate) async fn workspace_git_changes(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<WorkspaceGitChangesQuery>,
) -> Response {
    let scope = match ChangeScope::parse(query.scope.as_deref()) {
        Ok(scope) => scope,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let unavailable = |detail: String, base_ref: Option<String>| {
        Json(WorkspaceGitChanges {
            scope: scope.name(),
            available: false,
            detail: Some(detail),
            branch: None,
            base_ref,
            files: Vec::new(),
            totals: GitTotals::default(),
            truncated: false,
        })
        .into_response()
    };
    let context = match resolve(
        &state,
        &conversation_id,
        query.root.as_deref(),
        query.repo.as_deref(),
    )
    .await
    {
        Ok(Resolved::Ready(context)) => context,
        Ok(Resolved::NotRepository) => {
            return unavailable("This folder is not a Git repository.".into(), None)
        }
        Ok(Resolved::Unavailable(detail)) => return unavailable(detail, None),
        Err(response) => return response,
    };
    let (target, base) = match scope_target(&context, scope).await {
        Ok(value) => value,
        Err(detail) => return unavailable(detail, None),
    };
    let branch = git_line(
        &context.repository,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .await;
    let base_ref = base.map(|base| base.label);
    let (mut files, totals) = match collect_changes(&context, &target).await {
        Ok(value) => value,
        Err(detail) => return unavailable(detail, base_ref),
    };
    let truncated = files.len() > MAX_CHANGED_FILES;
    files.truncate(MAX_CHANGED_FILES);
    Json(WorkspaceGitChanges {
        scope: scope.name(),
        available: true,
        detail: None,
        branch,
        base_ref,
        files,
        totals,
        truncated,
    })
    .into_response()
}

/// A file's thread diff is sent only up to this size, and all of them up to
/// the total; past either the row says the diff is too large to show here.
const MAX_THREAD_FILE_DIFF: usize = 256 * 1024;
const MAX_THREAD_DIFF_TOTAL: usize = 1024 * 1024;

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkspaceThreadChangesQuery {
    root: Option<String>,
    repo: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadChangedFile {
    path: String,
    status: &'static str,
    binary: bool,
    additions: Option<u64>,
    deletions: Option<u64>,
    /// The thread's hunks for this file, turn by turn; null when too large.
    diff: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceThreadChanges {
    scope: &'static str,
    available: bool,
    detail: Option<String>,
    branch: Option<String>,
    files: Vec<ThreadChangedFile>,
    totals: GitTotals,
    truncated: bool,
}

/// "This thread": the files this thread itself changed in the repository,
/// over all its turns (`thread_changes`). Uncommitted and Branch stay views
/// of the whole checkout.
pub(crate) async fn workspace_thread_changes(
    State(state): State<crate::AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<WorkspaceThreadChangesQuery>,
) -> Response {
    let unavailable = |detail: String| {
        Json(WorkspaceThreadChanges {
            scope: "thread",
            available: false,
            detail: Some(detail),
            branch: None,
            files: Vec::new(),
            totals: GitTotals::default(),
            truncated: false,
        })
        .into_response()
    };
    let context = match resolve(
        &state,
        &conversation_id,
        query.root.as_deref(),
        query.repo.as_deref(),
    )
    .await
    {
        Ok(Resolved::Ready(context)) => context,
        Ok(Resolved::NotRepository) => {
            return unavailable("This folder is not a Git repository.".into())
        }
        Ok(Resolved::Unavailable(detail)) => return unavailable(detail),
        Err(response) => return response,
    };
    let Ok(read) = super::thread_changes::read(&state, &conversation_id, &context.repository).await
    else {
        return unavailable("This thread's changes are temporarily unavailable. Try again.".into());
    };
    let (partial, earlier) = (read.partial, read.earlier);
    let branch = git_line(
        &context.repository,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .await;
    // Checking each path against the root and deny list reads the disk.
    let rows = {
        let context = context.clone();
        tokio::task::spawn_blocking(move || thread_rows(&context, read.changes)).await
    };
    let Ok((files, totals, truncated)) = rows else {
        return unavailable("This thread's changes are temporarily unavailable. Try again.".into());
    };
    Json(WorkspaceThreadChanges {
        scope: "thread",
        available: true,
        detail: if partial {
            Some(
                "Some of this thread's changes couldn't be read, so this list may be incomplete."
                    .to_owned(),
            )
        } else if earlier {
            Some(
                "This thread is long, so changes from its earliest turns may be missing."
                    .to_owned(),
            )
        } else {
            None
        },
        branch,
        files,
        totals,
        truncated,
    })
    .into_response()
}

/// The visible rows (inside the root, outside the deny list), with diffs
/// within the size limits and totals over every visible change.
fn thread_rows(
    context: &RepoContext,
    changes: Vec<super::thread_changes::ThreadChange>,
) -> (Vec<ThreadChangedFile>, GitTotals, bool) {
    let mut budget = MAX_THREAD_DIFF_TOTAL;
    let mut files = changes
        .into_iter()
        .filter_map(|change| {
            let path = visible_path(context, &change.path)?;
            let diff = change.diff.filter(|diff| {
                let fits = diff.len() <= MAX_THREAD_FILE_DIFF && diff.len() <= budget;
                if fits {
                    budget -= diff.len();
                }
                fits
            });
            Some(ThreadChangedFile {
                path,
                status: change.status,
                binary: change.binary,
                additions: change.additions,
                deletions: change.deletions,
                diff,
            })
        })
        .collect::<Vec<_>>();
    let totals = GitTotals {
        files: files.len(),
        additions: files.iter().filter_map(|file| file.additions).sum(),
        deletions: files.iter().filter_map(|file| file.deletions).sum(),
    };
    let truncated = files.len() > MAX_CHANGED_FILES;
    files.truncate(MAX_CHANGED_FILES);
    (files, totals, truncated)
}

/// One file's diff for a scope: against HEAD (Uncommitted) or the branch's
/// merge base (Branch), working tree included. Untracked files get the same
/// synthetic "new file" diff as Files.
pub(super) async fn scoped_diff(
    state: &crate::AppState,
    conversation_id: &str,
    query: WorkspaceDiffQuery,
) -> Response {
    let scope = match ChangeScope::parse(query.scope.as_deref()) {
        Ok(scope) => scope,
        Err((status, detail)) => return workspace_error(status, detail),
    };
    let context = match resolve(
        state,
        conversation_id,
        query.root.as_deref(),
        query.repo.as_deref(),
    )
    .await
    {
        Ok(Resolved::Ready(context)) => context,
        Ok(Resolved::NotRepository) => {
            return workspace_error(
                StatusCode::NOT_FOUND,
                "This workspace location is not a Git repository.",
            )
        }
        Ok(Resolved::Unavailable(detail)) => return workspace_error(StatusCode::NOT_FOUND, detail),
        Err(response) => return response,
    };
    context_diff(&context, scope, query).await
}

/// A requested diff path, root-relative, after the lexical deny check: the
/// root-relative path and the same path relative to the repository's top.
fn diff_request_path(
    context: &RepoContext,
    value: Option<&str>,
) -> Result<(PathBuf, String), BrowseError> {
    let relative = relative_path(value)?;
    if relative.as_os_str().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Choose a file inside this workspace.",
        ));
    }
    let own = Some(context.root.path.as_path());
    if protected_by_denies(&context.root.path.join(&relative), &context.denied, own) {
        return Err((
            StatusCode::FORBIDDEN,
            "This path is outside the conversation's verified workspace.",
        ));
    }
    let in_repository = context
        .directory
        .join(&relative)
        .strip_prefix(&context.repository)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|_| {
            (
                StatusCode::FORBIDDEN,
                "This file is outside the verified repository.",
            )
        })?;
    Ok((relative, in_repository))
}

/// A diff names exactly one file: a regular file on disk (through the same
/// no-symlink, deny-aware check as Files), or a file the compared tree holds
/// when it is gone from disk. Folders are refused, since Git would include
/// every file beneath them, protected descendants included.
async fn require_single_file(
    context: &RepoContext,
    target: &str,
    relative: &Path,
    in_repository: &str,
) -> Result<(), BrowseError> {
    const NOT_A_FILE: BrowseError = (StatusCode::BAD_REQUEST, "Choose a file, not a folder.");
    let own = Some(context.root.path.as_path());
    let lexical = context.root.path.join(relative);
    if fs::symlink_metadata(&lexical).is_ok() {
        let resolved = validated_git_path(&context.root, relative, &context.denied, own)?;
        return match fs::symlink_metadata(resolved) {
            Ok(metadata) if metadata.file_type().is_file() => Ok(()),
            _ => Err(NOT_A_FILE),
        };
    }
    let object = format!("{target}:{in_repository}");
    match git(&context.repository, &["cat-file", "-t", &object]).await {
        Ok(kind) if kind.trim_ascii() == b"blob" => Ok(()),
        _ => Err(NOT_A_FILE),
    }
}

async fn context_diff(
    context: &RepoContext,
    scope: ChangeScope,
    query: WorkspaceDiffQuery,
) -> Response {
    match context_diff_value(context, scope, query).await {
        Ok(value) => Json(value).into_response(),
        Err((status, detail)) => workspace_error(status, detail),
    }
}

/// Annotation acceptance uses the exact same path, deny-list and bounded diff
/// reader as the preview. The caller freezes these bytes only after hashing.
pub(super) async fn verified_diff(
    state: &crate::AppState,
    conversation_id: &str,
    root: &str,
    path: &str,
    scope: &str,
    original_path: Option<&str>,
) -> Result<Vec<u8>, (StatusCode, String)> {
    let scope_value =
        ChangeScope::parse(Some(scope)).map_err(|(status, detail)| (status, detail.to_owned()))?;
    let context = match resolve(state, conversation_id, Some(root), None).await {
        Ok(Resolved::Ready(context)) => context,
        _ => {
            return Err((
                StatusCode::FORBIDDEN,
                "The diff's Project folder is no longer available. Reopen Code changes.".into(),
            ))
        }
    };
    let response = context_diff_value(
        &context,
        scope_value,
        WorkspaceDiffQuery {
            root: Some(root.into()),
            path: Some(path.into()),
            scope: Some(scope.into()),
            original_path: original_path.map(Into::into),
            ..Default::default()
        },
    )
    .await?;
    if response.annotation_version != Some(1) {
        return Err((StatusCode::CONFLICT, "This diff no longer describes one file. Reopen Code changes and select its lines again.".into()));
    }
    Ok(response.diff.into_bytes())
}

async fn context_diff_value(
    context: &RepoContext,
    scope: ChangeScope,
    query: WorkspaceDiffQuery,
) -> Result<WorkspaceDiffResponse, (StatusCode, String)> {
    let (relative, path_in_repository) = match diff_request_path(context, query.path.as_deref()) {
        Ok(value) => value,
        Err((status, detail)) => return Err((status, detail.into())),
    };
    let original = match query.original_path.as_deref() {
        Some(original) => match diff_request_path(context, Some(original)) {
            Ok(value) => Some(value),
            Err((status, detail)) => return Err((status, detail.into())),
        },
        None => None,
    };
    let (target, _) = match scope_target(context, scope).await {
        Ok(value) => value,
        Err(detail) => return Err((StatusCode::CONFLICT, detail)),
    };
    for (relative, in_repository) in std::iter::once((&relative, &path_in_repository))
        .chain(original.iter().map(|(a, b)| (a, b)))
    {
        if let Err((status, detail)) =
            require_single_file(context, &target, relative, in_repository).await
        {
            return Err((status, detail.into()));
        }
    }
    let original_in_repository = original.as_ref().map(|(_, path)| path.clone());
    let original = original_in_repository
        .as_deref()
        .map(|original| format!(":(literal,top){original}"));
    let pathspec = format!(":(literal,top){path_in_repository}");
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "-M",
        target.as_str(),
        "--",
        pathspec.as_str(),
    ];
    if let Some(original) = original.as_deref() {
        args.push(original);
    }
    let output = match git(&context.repository, &args).await {
        Ok(bytes) => bytes,
        Err(error) if error.contains("exceeded its limit") => {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE_DIFF.into()))
        }
        Err(error) if error.contains("timed out") => {
            return Err((StatusCode::GATEWAY_TIMEOUT, error))
        }
        Err(error) => return Err((StatusCode::SERVICE_UNAVAILABLE, error)),
    };
    let mut can_annotate = false;
    if !output.is_empty() {
        // Every file the patch covers must be one of the requested files and
        // visible to the phone, whatever Git made of the pathspecs.
        let mut raw = args.clone();
        raw.insert(1, "--raw");
        raw.insert(2, "-z");
        let allowed = [Some(&path_in_repository), original_in_repository.as_ref()];
        let covered = match git(&context.repository, &raw).await {
            Ok(bytes) => parse_raw_numstat(&bytes),
            Err(error) => return Err((StatusCode::SERVICE_UNAVAILABLE, error)),
        };
        // A note's displayed filename and old path must describe Git's one
        // actual file change, not two unrelated pathspecs supplied by a client.
        can_annotate = covered.len() == 1
            && covered[0].path == path_in_repository
            && covered[0].original.as_ref() == original_in_repository.as_ref();
        let hidden = covered.iter().any(|entry| {
            std::iter::once(&entry.path)
                .chain(entry.original.as_ref())
                .any(|path| !allowed.contains(&Some(path)) || visible_path(context, path).is_none())
        });
        if hidden {
            return Err((
                StatusCode::FORBIDDEN,
                "This path is outside the conversation's verified workspace.".into(),
            ));
        }
    }
    let mut diff = String::from_utf8_lossy(&output).into_owned();
    if diff.is_empty() {
        let untracked = git(
            &context.repository,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                &pathspec,
            ],
        )
        .await
        .is_ok_and(|bytes| !bytes.is_empty());
        if untracked {
            can_annotate = original_in_repository.is_none();
            let (root, denied, relative, name) = (
                context.root.clone(),
                context.denied.clone(),
                relative.to_path_buf(),
                path_in_repository.clone(),
            );
            diff = match workspace_disk(move || {
                read_untracked_for_diff(&root, &relative, &denied)
                    .and_then(|bytes| synthetic_untracked_diff(&name, bytes))
            })
            .await
            {
                Ok(diff) => diff,
                Err((StatusCode::PAYLOAD_TOO_LARGE, _)) => {
                    return Err((StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE_DIFF.into()))
                }
                Err((status, detail)) => return Err((status, detail.into())),
            };
        }
    }
    if diff.len() > MAX_DIFF_BYTES {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE_DIFF.into()));
    }
    Ok(WorkspaceDiffResponse {
        path: query.path.unwrap_or_default(),
        staged: false,
        annotation_version: can_annotate.then_some(1),
        diff,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn driver_repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]);
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/file"), "first\n").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-qm", "initial"]);
        fs::write(dir.path().join("nested/file"), "second\n").unwrap();
        dir
    }

    /// A failed read tells the phone what happened in plain words; Git's text,
    /// with the Mac's paths, stays out of the reply.
    #[tokio::test]
    async fn failed_reads_return_plain_errors_without_mac_paths() {
        let dir = driver_repository();
        let repo = dir.path().to_str().unwrap();
        for (args, expected) in [
            (
                &["diff", "no-such-branch", "--", "nested/file"][..],
                "That branch or commit is no longer available. Reload the changes.",
            ),
            (
                &["show", "HEAD:nested/missing"][..],
                "Git could not read these changes. Try again, or review them on your computer.",
            ),
        ] {
            let error = read_only_git_bytes(repo, args).await.unwrap_err();
            assert_eq!(error, expected, "{args:?}");
        }
        let outside = tempfile::tempdir().unwrap();
        let error = read_only_git_bytes(outside.path().to_str().unwrap(), &["status"])
            .await
            .unwrap_err();
        assert_eq!(error, "This folder is no longer a Git repository.");
        let logged = redacted_git_detail(
            "\u{1b}[31mfatal: unable to access 'https://owner:ghs_secret@github.com/o/r.git/': 403\nssh://deploy:pw@host:22/r",
        );
        assert!(
            !logged.contains("ghs_secret") && !logged.contains("pw@"),
            "{logged}"
        );
        assert!(
            logged.contains("https://[redacted]@github.com/o/r.git"),
            "{logged}"
        );
        assert!(!logged.contains('\u{1b}'), "{logged}");
        // A URL that straddles the length bound keeps no part of its password.
        let padding = "x".repeat(1500 - "https://owner:ghs_".len() + 3);
        let logged = redacted_git_detail(&format!(
            "{padding} https://owner:ghs_cut_secret@github.com/o/r.git"
        ));
        assert!(logged.len() <= 1500);
        assert!(
            !logged.contains("owner") && !logged.contains("ghs_"),
            "{logged}"
        );
        // A password may hold quotes or spaces Git prints unencoded: everything
        // before the host's `@` is removed, whatever it contains.
        for password in [
            "plain'secret",
            "plain\"secret",
            "plain secret",
            "plain'sec ret\"",
        ] {
            let logged = redacted_git_detail(&format!(
                "fatal: unable to access 'https://owner:{password}@example.invalid/r.git/': 403"
            ));
            assert!(
                !logged.contains("owner") && !logged.contains("secret") && !logged.contains("sec"),
                "{password}: {logged}"
            );
            assert!(logged.contains("@example.invalid/r.git"), "{logged}");
        }
    }

    /// Signature formatting can execute gpg.program even with
    /// log.showSignature=false. Prove the fixture executes with the old flags,
    /// then exercise the actual boundary with the same config and commit.
    #[tokio::test]
    async fn reads_ignore_config_only_programs_including_signature_formats() {
        use std::os::unix::fs::PermissionsExt;
        let dir = driver_repository();
        let repo = dir.path();
        let marker = repo.join("program-ran");
        let script = repo.join("verify-signature");
        fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let tree = run(repo, &["rev-parse", "HEAD^{tree}"]);
        let commit = format!("tree {tree}\nauthor Test <test@example.invalid> 1700000000 +0000\ncommitter Test <test@example.invalid> 1700000000 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n ZmFrZQo=\n -----END PGP SIGNATURE-----\n\nSynthetic signature fixture\n");
        let mut hash = crate::test_git::git()
            .arg("-C")
            .arg(repo)
            .args(["hash-object", "-t", "commit", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        io::Write::write_all(&mut hash.stdin.take().unwrap(), commit.as_bytes()).unwrap();
        let hashed = hash.wait_with_output().unwrap();
        assert!(hashed.status.success());
        run(
            repo,
            &[
                "update-ref",
                "HEAD",
                String::from_utf8_lossy(&hashed.stdout).trim(),
            ],
        );
        run(repo, &["config", "gpg.program", script.to_str().unwrap()]);
        run(repo, &["config", "format.pretty", "%G?"]);
        let baseline = git_command(repo.to_str().unwrap(), &["log", "-1"], None)
            .output()
            .await
            .unwrap();
        assert!(baseline.status.success());
        assert!(
            marker.exists(),
            "signature fixture did not exercise the program"
        );
        fs::remove_file(&marker).unwrap();
        let included = repo.join("included.config");
        fs::write(&included, format!("[core]\nhooksPath = {}\nfsmonitor = {}\n[diff]\nexternal = {}\n[filter \"unused\"]\nclean = {}\n", script.display(), script.display(), script.display(), script.display())).unwrap();
        run(
            repo,
            &["config", "include.path", included.to_str().unwrap()],
        );
        run(repo, &["config", "gpg.format", "ssh"]);
        run(
            repo,
            &["config", "gpg.ssh.program", script.to_str().unwrap()],
        );
        for args in [
            &["diff"][..],
            &["status", "--porcelain=v2"],
            &["log", "-1"],
            &["ls-files", "--cached"],
        ] {
            let read = read_only_git_bytes(repo.to_str().unwrap(), args).await;
            assert!(!marker.exists(), "git {args:?} ran a config-only program");
            assert!(read.is_ok(), "git {args:?}: {read:?}");
        }
    }

    #[tokio::test]
    async fn a_prepared_read_cannot_load_later_config_or_info_attributes() {
        let dir = driver_repository();
        let repo = dir.path();
        let marker = repo.join("program-ran");
        let mut prepared = read_only_git_command(repo.to_str().unwrap(), &["diff"])
            .await
            .unwrap();
        run(
            repo,
            &[
                "config",
                "filter.later.clean",
                &format!("touch '{}'; cat", marker.display()),
            ],
        );
        run(repo, &["config", "extensions.worktreeConfig", "true"]);
        fs::write(
            repo.join(".git/config.worktree"),
            format!(
                "[filter \"later\"]\nclean = touch '{}'; cat\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::write(repo.join(".git/info/attributes"), "* filter=later\n").unwrap();
        let result = prepared.command.output().await.unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!marker.exists());
        assert!(String::from_utf8_lossy(&result.stdout).contains("+second"));
    }

    /// Copying shallow boundaries into the private view must not create an
    /// unbounded filesystem wait or follow an agent-controlled link.
    #[tokio::test]
    async fn private_reads_refuse_non_regular_shallow_metadata() {
        use std::os::unix::fs::{symlink, OpenOptionsExt};
        let dir = driver_repository();
        let repo = dir.path();
        let shallow = repo.join(".git/shallow");
        assert!(std::process::Command::new("mkfifo")
            .arg(&shallow)
            .status()
            .unwrap()
            .success());
        let result = tokio::time::timeout(
            Duration::from_secs(60),
            read_only_git_bytes(repo.to_str().unwrap(), &["diff"]),
        )
        .await;
        // Release a pre-fix blocking open even after the timeout cancels its
        // future, so the failing regression does not hang the test runtime.
        let release = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&shallow)
            .unwrap();
        assert!(
            result.is_ok(),
            "a shallow FIFO left the read waiting indefinitely"
        );
        assert!(result.unwrap().is_err());
        drop(release);
        fs::remove_file(&shallow).unwrap();
        let target = repo.join("outside-shallow");
        fs::write(&target, format!("{}\n", run(repo, &["rev-parse", "HEAD"]))).unwrap();
        symlink(&target, &shallow).unwrap();
        assert!(read_only_git_bytes(repo.to_str().unwrap(), &["diff"])
            .await
            .is_err());
    }

    /// The snapshot bound must identify references, so a caller does not
    /// misreport a large repository as a single oversized file diff.
    #[tokio::test]
    async fn oversized_reference_snapshot_names_the_reference_limit() {
        let dir = driver_repository();
        let repo = dir.path();
        let oid = run(repo, &["rev-parse", "HEAD"]);
        let mut refs = String::from("# pack-refs with: sorted\n");
        for index in 0..20_000 {
            refs.push_str(&format!("{oid} refs/heads/fixture-{index:05}\n"));
        }
        fs::write(repo.join(".git/packed-refs"), refs).unwrap();
        let error = read_only_git_bytes(repo.to_str().unwrap(), &["diff"])
            .await
            .unwrap_err();
        assert!(error.contains("reference"), "wrong limit reported: {error}");
        assert!(
            !error.contains("exceeded its limit"),
            "must not be mistaken for an oversized patch"
        );
    }

    /// The private common directory must preserve real staged/unstaged data,
    /// including split indexes, linked worktrees, SHA-256 and shallow history.
    #[tokio::test]
    async fn private_reads_preserve_index_and_history_formats_without_writing() {
        for object_format in ["sha1", "sha256"] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            fs::create_dir(&repo).unwrap();
            run(
                &repo,
                &["init", "-q", &format!("--object-format={object_format}")],
            );
            fs::write(repo.join("file"), "first\n").unwrap();
            run(&repo, &["add", "."]);
            run(&repo, &["commit", "-qm", "first"]);
            // A real local shallow clone (the file transport honors depth).
            let shallow = dir.path().join("shallow");
            run(
                &repo,
                &[
                    "clone",
                    "-q",
                    "--depth=1",
                    &format!("file://{}", repo.display()),
                    shallow.to_str().unwrap(),
                ],
            );
            for root in [&repo, &shallow] {
                run(root, &["update-index", "--split-index"]);
                let before = fs::read(root.join(".git/index")).unwrap();
                fs::write(root.join("file"), "second\n").unwrap();
                let bytes = read_only_git_bytes(root.to_str().unwrap(), &["diff"])
                    .await
                    .unwrap();
                assert!(String::from_utf8_lossy(&bytes).contains("+second"));
                let log = read_only_git_bytes(root.to_str().unwrap(), &["log", "--oneline"])
                    .await
                    .unwrap();
                assert!(String::from_utf8_lossy(&log).contains("first"));
                assert_eq!(fs::read(root.join(".git/index")).unwrap(), before);
            }
            let linked = dir.path().join("linked");
            run(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "linked",
                    linked.to_str().unwrap(),
                ],
            );
            fs::write(linked.join("file"), "linked\n").unwrap();
            let bytes = read_only_git_bytes(linked.to_str().unwrap(), &["diff"])
                .await
                .unwrap();
            assert!(String::from_utf8_lossy(&bytes).contains("+linked"));
        }
    }

    /// Owns the config TOCTOU contract from review a5a55360: a filter's
    /// presence may change between any two Git processes. Exercise the real
    /// read boundary 300 times, with atomic config replacements, not a mock.
    #[tokio::test]
    async fn reads_never_run_a_filter_during_300_config_races() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let dir = driver_repository();
        let repo = dir.path();
        let marker = repo.join("filter-ran");
        fs::write(repo.join(".gitattributes"), "* filter=agent\n").unwrap();
        let config = repo.join(".git/config");
        let safe = fs::read_to_string(&config).unwrap();
        let armed = format!(
            "{safe}\n[filter \"agent\"]\nclean = printf x >> '{}'; cat\n",
            marker.display()
        );
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let writer = std::thread::spawn(move || {
            let next = config.with_extension("next");
            while !stopped.load(Ordering::Relaxed) {
                for bytes in [&safe, &armed] {
                    fs::write(&next, bytes).unwrap();
                    fs::rename(&next, &config).unwrap();
                    std::thread::sleep(Duration::from_micros(300));
                }
            }
        });
        let mut refused = 0;
        for _ in 0..300 {
            refused += usize::from(
                read_only_git_bytes(
                    repo.to_str().unwrap(),
                    &["diff", "--no-ext-diff", "--", "nested/file"],
                )
                .await
                .is_err(),
            );
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        let executions = fs::read(&marker).unwrap_or_default().len();
        eprintln!("config race: 300 reads, {refused} refused, {executions} filter executions");
        assert_eq!(
            executions, 0,
            "the repository's racing filter ran as the owner"
        );
        assert_eq!(
            refused, 0,
            "modern Git must read without a racy config gate"
        );
    }

    /// Worktree, nested, index-only and info attributes all select drivers.
    /// Their configured programs must be inert while real changes stay visible.
    #[tokio::test]
    async fn reads_cannot_bind_a_driver_from_any_attribute_source() {
        for location in [
            ".gitattributes",
            "nested/.gitattributes",
            "index",
            ".git/info/attributes",
        ] {
            let dir = driver_repository();
            let repo = dir.path();
            let attr = repo.join(if location == "index" {
                ".gitattributes"
            } else {
                location
            });
            fs::write(&attr, "* filter=agent diff=agent merge=agent\n").unwrap();
            if location == "index" {
                run(repo, &["add", ".gitattributes"]);
                fs::remove_file(attr).unwrap();
            }
            let marker = repo.join("program-ran");
            let program = format!("touch '{}'; cat", marker.display());
            for key in [
                "filter.agent.clean",
                "filter.agent.process",
                "diff.agent.command",
                "diff.agent.textconv",
                "merge.agent.driver",
            ] {
                run(repo, &["config", key, &program]);
            }
            for args in [
                &["diff", "--no-ext-diff"][..],
                &["status", "--porcelain=v2"],
                &["log", "-1", "--oneline", "--patch"],
                &["ls-files", "--cached"],
            ] {
                let result = read_only_git_bytes(repo.to_str().unwrap(), args).await;
                assert!(
                    !marker.exists(),
                    "{location}: git {args:?} executed a program"
                );
                let bytes = result.unwrap_or_else(|e| panic!("{location}: git {args:?}: {e}"));
                assert!(String::from_utf8_lossy(&bytes).contains("nested/file"));
            }
        }
    }

    /// Reproduce the review's temp-folder attack at the exact pre-exec
    /// interleaving. The attacker has the owner's uid, can write temp/repo,
    /// and is denied writes to the fixture's daemon data by the OS sandbox.
    /// Neither 0700 alone nor another check of the view stops this attacker.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn temp_directory_writer_cannot_rebind_private_reads() {
        use std::os::unix::fs::PermissionsExt;
        let dir = driver_repository();
        let repo = dir.path();
        let data = tempfile::tempdir().unwrap();
        let data = crate::canonical_path::canonicalize(data.path()).unwrap();
        let marker = repo.join("filter-ran");
        let attempts = repo.join("attempts");
        let driver = repo.join("driver");
        fs::write(
            &driver,
            format!("#!/bin/sh\nprintf x >> '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&driver, fs::Permissions::from_mode(0o755)).unwrap();
        let writer = repo.join("writer");
        fs::write(&writer, "#!/bin/sh\nmkdir -p \"$1/info\" || exit 1\nprintf '[filter \"agent\"]\\nclean = %s\\n' \"$2\" >> \"$1/config\" || exit 1\nprintf '* filter=agent\\n' > \"$1/info/attributes\"\n").unwrap();
        let profile = format!(
            "(version 1)(allow default)(deny file-write* (subpath \"{}\"))",
            data.display()
        );
        // Positive control: the same sandboxed writer can alter temp views.
        let writable = repo.join("writable-view");
        assert!(std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/bin/sh"])
            .arg(&writer)
            .arg(&writable)
            .arg(&driver)
            .status()
            .unwrap()
            .success());
        assert!(writable.join("info/attributes").is_file());
        // And the protected fixture really is denied, despite matching uid.
        assert!(!std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile, "/bin/sh"])
            .arg(&writer)
            .arg(data.join("forbidden"))
            .arg(&driver)
            .output()
            .unwrap()
            .status
            .success());
        let git = std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        let git = String::from_utf8(git.stdout).unwrap();
        let wrapper = repo.join("git-race");
        fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
if [ -n "$GIT_COMMON_DIR" ]; then
 /usr/bin/sandbox-exec -p '{}' /bin/sh '{}' "$GIT_COMMON_DIR" '{}' >/dev/null 2>&1
 printf '%s\n' "$?" >> '{}'
fi
exec '{}' "$@"
"#,
                profile,
                writer.display(),
                driver.display(),
                attempts.display(),
                git.trim()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        TEST_DATA_DIR.scope(data.clone(), TEST_GIT.scope(wrapper, async {
            let mut refused = 0;
            for _ in 0..300 {
                refused += usize::from(read_only_git_bytes(repo.to_str().unwrap(), &["diff", "--", "nested/file"]).await.is_err());
            }
            let executions = fs::read(&marker).unwrap_or_default().len();
            let attempts = fs::read_to_string(&attempts).unwrap();
            let accepted = attempts.lines().filter(|code| *code == "0").count();
            eprintln!("private-view race: 300 reads, {refused} refused, {accepted} successful writes, {executions} filter executions");
            assert_eq!(attempts.lines().count(), 300);
            assert_eq!(executions, 0, "a temp-writable agent rebound the private config");
            assert_eq!(accepted, 0, "the agent wrote the private view");
            assert_eq!(refused, 0);
        })).await;
        assert!(
            fs::read_dir(data).unwrap().next().is_none(),
            "read views leaked"
        );
    }

    /// Per-task executable injection avoids racing the process-wide PATH.
    /// Real Git handles every operation except the faked old version/flag.
    #[tokio::test]
    async fn old_git_reads_with_private_config_without_attribute_source() {
        use std::os::unix::fs::PermissionsExt;
        let dir = driver_repository();
        let repo = dir.path();
        let git = std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        let git = String::from_utf8(git.stdout).unwrap();
        let wrapper = repo.join("old-git");
        fs::write(&wrapper, format!(
            "#!/bin/sh\nfor arg do\n case \"$arg\" in\n --version) echo 'git version 2.34.1'; exit 0;;\n --attr-source*) echo 'unsupported flag' >&2; exit 129;;\n esac\ndone\nunset GIT_NO_LAZY_FETCH\nexec '{}' \"$@\"\n", git.trim()
        )).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let marker = repo.join("program-ran");
        run(
            repo,
            &[
                "config",
                "filter.agent.clean",
                &format!("touch '{}'; cat", marker.display()),
            ],
        );
        TEST_GIT
            .scope(wrapper, async {
                for location in [".gitattributes", ".git/info/attributes"] {
                    fs::write(repo.join(location), "* filter=agent\n").unwrap();
                    for args in [
                        &["diff", "--no-ext-diff"][..],
                        &["status", "--porcelain=v2"],
                    ] {
                        let result = read_only_git_bytes(repo.to_str().unwrap(), args).await;
                        assert!(!marker.exists());
                        let bytes = result.unwrap_or_else(|error| panic!("git {args:?}: {error}"));
                        assert!(String::from_utf8_lossy(&bytes).contains("nested/file"));
                    }
                    fs::remove_file(repo.join(location)).unwrap();
                }
                // Metadata/search remain usable on old Git.
                for args in [
                    &["rev-parse", "--show-toplevel"][..],
                    &["ls-files", "--cached"],
                ] {
                    assert!(read_only_git_bytes(repo.to_str().unwrap(), args)
                        .await
                        .is_ok());
                }
                // Older Git has no GIT_NO_LAZY_FETCH. A missing promised
                // object must not run a repository SSH command even when
                // its settings explicitly allow that transport.
                let commit = run(repo, &["rev-parse", "HEAD"]);
                fs::remove_file(
                    repo.join(".git/objects")
                        .join(&commit[..2])
                        .join(&commit[2..]),
                )
                .unwrap();
                let ssh = repo.join("ssh-fixture");
                fs::write(
                    &ssh,
                    format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
                )
                .unwrap();
                fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
                for (key, value) in [
                    ("remote.origin.url", "ssh://example.invalid/repo"),
                    ("remote.origin.promisor", "true"),
                    ("extensions.partialClone", "origin"),
                    ("protocol.ssh.allow", "always"),
                    ("core.sshCommand", ssh.to_str().unwrap()),
                ] {
                    run(repo, &["config", key, value]);
                }
                let read = read_only_git_bytes(
                    repo.to_str().unwrap(),
                    &["rev-parse", "--verify", "HEAD^{commit}"],
                )
                .await;
                assert!(read.is_err());
                assert!(
                    !marker.exists(),
                    "an old-Git metadata read ran the repository's SSH command"
                );
            })
            .await;
    }

    /// A thread's agent can write its repository's config and attributes, and
    /// Cupola reads that repository as the owner, outside the agent's sandbox,
    /// whenever the phone opens its changes. A read must never run a filter,
    /// textconv or other program those settings name.
    #[cfg(unix)]
    #[tokio::test]
    async fn reads_never_run_a_program_from_the_repositorys_settings() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let git = |args: &[&str]| {
            let output = crate::test_git::git()
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}");
        };
        std::fs::create_dir(&repo).unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.name", "Cupola Test"]);
        git(&["config", "user.email", "test@example.com"]);
        std::fs::write(repo.join("file"), "one\n").unwrap();
        git(&["add", "file"]);
        git(&["commit", "-q", "-m", "one"]);
        let path = repo.to_str().unwrap();
        assert!(read_only_git_bytes(path, &["diff", "--no-ext-diff"])
            .await
            .is_ok());

        // Husky v9 sets core.hooksPath in every repository it installs into,
        // and SSH signing sets gpg.format; the read flags neutralize both.
        git(&["config", "core.hooksPath", ".husky/_"]);
        git(&["config", "gpg.format", "ssh"]);
        std::fs::write(repo.join("file"), "husky\n").unwrap();
        for args in [
            &["diff", "--no-ext-diff"][..],
            &["status", "--porcelain=v2"],
        ] {
            let read = read_only_git_bytes(path, args).await;
            assert!(
                read.is_ok(),
                "git {args:?} refused a Husky/SSH-signing repo: {read:?}"
            );
        }

        let marker = dir.path().join("ran");
        git(&[
            "config",
            "filter.agent.clean",
            &format!("touch {}; cat", marker.display()),
        ]);
        std::fs::write(repo.join(".gitattributes"), "* filter=agent\n").unwrap();
        std::fs::write(repo.join("file"), "two\n").unwrap();
        for args in [
            &["diff", "--no-ext-diff"][..],
            &["status", "--porcelain=v2"],
            &["diff", "--no-ext-diff", "--numstat", "HEAD"],
        ] {
            let read = read_only_git_bytes(path, args).await;
            assert!(!marker.exists(), "git {args:?} ran the agent's filter");
            assert!(read.is_ok(), "git {args:?}: {read:?}");
        }
    }

    /// The parent config check cannot see a submodule's programs. Reproduce
    /// the review's inline diff bypass through the actual read boundary, and
    /// cover status too: both must report gitlink changes without running a
    /// submodule's filters to inspect its working files.
    #[cfg(unix)]
    #[tokio::test]
    async fn reads_never_run_a_program_from_a_submodules_settings() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let source = dir.path().join("source");
        for repo in [&parent, &source] {
            fs::create_dir(repo).unwrap();
            run(repo, &["init", "-q"]);
            fs::write(repo.join("file"), "first\n").unwrap();
            run(repo, &["add", "."]);
            run(repo, &["commit", "-qm", "initial"]);
        }
        run(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                source.to_str().unwrap(),
                "sub",
            ],
        );
        run(&parent, &["commit", "-qam", "submodule"]);
        run(&parent, &["config", "diff.submodule", "diff"]);
        run(&parent, &["config", "status.submoduleSummary", "true"]);
        // Even a per-submodule override must not make status enter it.
        run(&parent, &["config", "submodule.sub.ignore", "none"]);
        let sub = parent.join("sub");
        let marker = dir.path().join("filter-executed");
        run(
            &sub,
            &[
                "config",
                "filter.agent.clean",
                &format!("touch '{}'; cat", marker.display()),
            ],
        );
        fs::write(sub.join(".gitattributes"), "* filter=agent\n").unwrap();
        fs::write(sub.join("file"), "changed\n").unwrap();
        let path = parent.to_str().unwrap();
        assert!(git_actions::WriteView::prepare(&parent, true).await.is_ok());
        for args in [
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--find-renames",
                "--",
                ":(literal,top)sub",
            ][..],
            &["status", "--porcelain=v2"],
        ] {
            let read = read_only_git_bytes(path, args).await;
            assert!(!marker.exists(), "git {args:?} ran the submodule's filter");
            assert!(read.is_ok(), "{read:?}");
        }
        // Moving the gitlink remains observable without scanning nested files.
        run(&sub, &["config", "--remove-section", "filter.agent"]);
        fs::remove_file(sub.join(".gitattributes")).unwrap();
        run(&sub, &["add", "file"]);
        run(&sub, &["commit", "-qm", "new submodule commit"]);
        let diff = read_only_git_bytes(path, &["diff", "--no-ext-diff", "--", "sub"])
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&diff).contains("Subproject commit"));
        let status = read_only_git_bytes(path, &["status", "--porcelain=v2"])
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&status).contains("sub"));
    }

    #[test]
    fn shared_fixtures_match_the_schema() {
        for (definition, fixture) in [
            (
                "workspaceGitSummary",
                include_str!(
                    "../../../../packages/protocol/fixtures/workspace-git-summary-v1.json"
                ),
            ),
            (
                "workspaceGitChanges",
                include_str!(
                    "../../../../packages/protocol/fixtures/workspace-git-changes-v1.json"
                ),
            ),
            (
                "workspaceThreadChanges",
                include_str!(
                    "../../../../packages/protocol/fixtures/workspace-thread-changes-v1.json"
                ),
            ),
        ] {
            crate::tests::validate_http_contract(
                definition,
                &serde_json::from_str(fixture).unwrap(),
            );
        }
    }

    #[test]
    fn raw_numstat_pairs_status_counts_renames_and_binaries() {
        let bytes = b":100644 100644 aaa bbb M\0src/a.rs\0:000000 100644 000 ccc A\0new.txt\0:100644 100644 ddd eee R087\0old name.txt\0new name.txt\0:100644 000000 fff 000 D\0gone.txt\0:100644 100644 111 222 M\0logo.png\x003\t1\tsrc/a.rs\x002\t0\tnew.txt\x001\t1\t\0old name.txt\0new name.txt\x000\t4\tgone.txt\0-\t-\tlogo.png\0";
        let entries = parse_raw_numstat(bytes);
        assert_eq!(entries.len(), 5);
        assert_eq!(
            entries[0],
            DiffEntry {
                path: "src/a.rs".into(),
                original: None,
                status: b'M',
                binary: false,
                additions: Some(3),
                deletions: Some(1)
            }
        );
        assert_eq!(status_name(entries[1].status), "added");
        assert_eq!(entries[2].path, "new name.txt");
        assert_eq!(entries[2].original.as_deref(), Some("old name.txt"));
        assert_eq!(status_name(entries[2].status), "renamed");
        assert_eq!(
            (entries[2].additions, entries[2].deletions),
            (Some(1), Some(1))
        );
        assert_eq!(status_name(entries[3].status), "deleted");
        assert_eq!(entries[3].deletions, Some(4));
        assert!(entries[4].binary);
        assert_eq!(entries[4].additions, None);
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = crate::test_git::git()
            .args([
                "-c",
                "user.name=Cupola Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    async fn project_conversation(state: &crate::AppState, id: &str, folder: &Path) {
        project_conversation_in(state, id, folder, folder).await
    }

    /// A Project saving `folder` with a thread working in `cwd`.
    async fn project_conversation_in(state: &crate::AppState, id: &str, folder: &Path, cwd: &Path) {
        let path = folder.to_string_lossy().into_owned();
        let cwd = cwd.to_string_lossy().into_owned();
        let canonical = crate::canonical_path::canonicalize(folder)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let inputs = [wonder_store::ProjectRootInput {
            path: path.clone(),
            canonical_path: canonical,
        }];
        let project = match state
            .store
            .create_project(&format!("{id}-project"), id, id, id, &inputs, 0, "now")
            .await
            .unwrap()
        {
            wonder_store::ProjectCreate::Created(project) => project,
            _ => panic!("new project"),
        };
        state
            .store
            .create_project_conversation(wonder_store::ProjectConversationInsert {
                conversation_id: id,
                project_id: &project.id,
                family: wonder_store::AgentFamily::Codex,
                provider_store: "owner",
                native_session_id: Some(id),
                cwd: &cwd,
                roots_revision: project.roots_revision,
                title: id,
                model: None,
                effort: None,
                service_tier: None,
                access_mode: "workspace",
                claude_approval: "ask",
                plan_mode: false,
                creation_request_id: None,
                now: "now",
            })
            .await
            .unwrap();
    }

    async fn get(state: &crate::AppState, uri: &str) -> (StatusCode, serde_json::Value, String) {
        use axum::{body::to_bytes, body::Body, http::Request};
        use tower::ServiceExt;
        let response = crate::router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            text,
        )
    }

    fn file<'a>(changes: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
        changes["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == path)
            .unwrap_or_else(|| panic!("{path} missing from {changes}"))
    }

    #[tokio::test]
    async fn branch_summary_changes_and_diffs_for_a_worktree() {
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let scratch = tempfile::tempdir().unwrap();
        let main = scratch.path().join("main");
        fs::create_dir(&main).unwrap();
        run(&main, &["init", "-q"]);
        let lines = |count: usize, tag: &str| {
            (0..count)
                .map(|line| format!("{tag} line {line}\n"))
                .collect::<String>()
        };
        fs::write(main.join("a.txt"), lines(5, "a")).unwrap();
        fs::write(main.join("rename-me.txt"), lines(20, "r")).unwrap();
        fs::write(main.join("gone.txt"), "bye\n").unwrap();
        fs::write(main.join("big.txt"), lines(40_000, "big")).unwrap();
        fs::write(main.join("logo.bin"), b"\0\x01\x02old").unwrap();
        run(&main, &["add", "-A"]);
        run(&main, &["commit", "-qm", "base"]);
        let worktree = scratch.path().join("feature-tree");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        // One commit on the branch, one on main after it left.
        fs::write(
            worktree.join("a.txt"),
            lines(5, "a").replace("a line 2\n", "a line two\n"),
        )
        .unwrap();
        run(&worktree, &["mv", "rename-me.txt", "renamed.txt"]);
        run(&worktree, &["commit", "-qam", "feature work"]);
        fs::write(main.join("main-only.txt"), "main\n").unwrap();
        run(&main, &["add", "main-only.txt"]);
        run(&main, &["commit", "-qm", "main moves on"]);
        // Uncommitted: an edit, a deletion, an untracked file, a binary edit, a huge rewrite.
        fs::write(
            worktree.join("a.txt"),
            lines(5, "a").replace("a line 2\n", "a line two\n") + "added\n",
        )
        .unwrap();
        fs::remove_file(worktree.join("gone.txt")).unwrap();
        fs::write(worktree.join("new.txt"), "one\ntwo\nthree").unwrap();
        fs::write(worktree.join("logo.bin"), b"\0\x01\x02new").unwrap();
        fs::write(worktree.join("big.txt"), lines(40_000, "BIG")).unwrap();
        fs::create_dir(worktree.join("secret")).unwrap();
        fs::write(worktree.join("secret/key.txt"), "hidden\n").unwrap();
        project_conversation(&state, "git-branch", &worktree).await;
        let base = "/api/v1/conversations/git-branch/workspace/git";

        let (status, summary, text) = get(&state, &format!("{base}/summary?root=workspace")).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        crate::tests::validate_http_contract("workspaceGitSummary", &summary);
        assert_eq!(summary["repository"], true);
        assert_eq!(summary["branch"], "feature");
        assert_eq!(summary["detached"], false);
        assert_eq!(summary["worktree"], true);
        assert_eq!(summary["baseRef"], "main");
        assert_eq!(
            (summary["ahead"].as_u64(), summary["behind"].as_u64()),
            (Some(1), Some(1))
        );
        assert_eq!(summary["uncommitted"]["files"], 6, "{summary}");
        assert_eq!(summary["branchChanges"]["files"], 7, "{summary}");

        let (status, changes, text) = get(&state, &format!("{base}/changes?root=workspace")).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        crate::tests::validate_http_contract("workspaceGitChanges", &changes);
        assert_eq!(changes["scope"], "uncommitted");
        assert_eq!(changes["branch"], "feature");
        assert_eq!(file(&changes, "a.txt")["status"], "modified");
        assert_eq!(file(&changes, "a.txt")["additions"], 1);
        assert_eq!(file(&changes, "gone.txt")["status"], "deleted");
        assert_eq!(file(&changes, "new.txt")["status"], "untracked");
        assert_eq!(file(&changes, "new.txt")["additions"], 3);
        assert_eq!(file(&changes, "logo.bin")["binary"], true);
        assert_eq!(
            file(&changes, "logo.bin")["additions"],
            serde_json::Value::Null
        );

        let (status, branch, text) = get(
            &state,
            &format!("{base}/changes?root=workspace&scope=branch"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        crate::tests::validate_http_contract("workspaceGitChanges", &branch);
        assert_eq!(branch["baseRef"], "main");
        let renamed = file(&branch, "renamed.txt");
        assert_eq!(renamed["status"], "renamed");
        assert_eq!(renamed["originalPath"], "rename-me.txt");
        // The branch includes its commit and uncommitted work, not main's later commit.
        assert_eq!(file(&branch, "a.txt")["additions"], 2);
        assert!(!branch.to_string().contains("main-only.txt"));

        let (status, diff, text) = get(
            &state,
            &format!("{base}/diff?root=workspace&path=renamed.txt&originalPath=rename-me.txt&scope=branch"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        crate::tests::validate_http_contract("workspaceDiff", &diff);
        assert!(diff["diff"]
            .as_str()
            .unwrap()
            .contains("rename from rename-me.txt"));
        assert_eq!(diff["annotationVersion"], 1);
        let (status, unrelated, _) = get(
            &state,
            &format!(
                "{base}/diff?root=workspace&path=a.txt&originalPath=gone.txt&scope=uncommitted"
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(unrelated.get("annotationVersion").is_none());
        assert_eq!(
            verified_diff(
                &state,
                "git-branch",
                "workspace",
                "a.txt",
                "uncommitted",
                Some("gone.txt")
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        let (status, diff, _) = get(
            &state,
            &format!("{base}/diff?root=workspace&path=a.txt&scope=branch"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let text = diff["diff"].as_str().unwrap();
        assert!(
            text.contains("+a line two") && text.contains("+added"),
            "{text}"
        );
        let (status, diff, _) = get(
            &state,
            &format!("{base}/diff?root=workspace&path=a.txt&scope=uncommitted"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!diff["diff"].as_str().unwrap().contains("+a line two"));
        let (status, diff, _) = get(
            &state,
            &format!("{base}/diff?root=workspace&path=new.txt&scope=uncommitted"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(diff["diff"].as_str().unwrap().contains("+three"));
        let (status, diff, _) = get(
            &state,
            &format!("{base}/diff?root=workspace&path=gone.txt&scope=uncommitted"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(diff["diff"].as_str().unwrap().contains("-bye"));
        let (status, _, text) = get(
            &state,
            &format!("{base}/diff?root=workspace&path=big.txt&scope=uncommitted"),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{text}");
        assert!(text.contains("Open it on your computer"));
        // Escapes and protected paths are refused before Git runs.
        for (path, expected) in [
            ("../main/a.txt", StatusCode::BAD_REQUEST),
            ("%2Fetc%2Fpasswd", StatusCode::BAD_REQUEST),
        ] {
            let (status, _, text) = get(
                &state,
                &format!("{base}/diff?root=workspace&path={path}&scope=branch"),
            )
            .await;
            assert_eq!(status, expected, "{path}: {text}");
        }
        let (status, _, _) = get(
            &state,
            &format!("{base}/changes?root=workspace&scope=everything"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // A protected folder inside the workspace is never listed.
        let denied = vec![crate::canonical_path::canonicalize(worktree.join("secret")).unwrap()];
        let Resolved::Ready(mut context) = resolve(&state, "git-branch", Some("workspace"), None)
            .await
            .unwrap_or_else(|_| panic!("resolve"))
        else {
            panic!("repository")
        };
        assert_eq!(
            visible_path(&context, "secret/key.txt").as_deref(),
            Some("secret/key.txt")
        );
        context.denied = denied;
        assert_eq!(visible_path(&context, "secret/key.txt"), None);
        assert_eq!(visible_path(&context, "../main/a.txt"), None);
        assert_eq!(visible_path(&context, "a.txt").as_deref(), Some("a.txt"));
        // Read-only: the index and refs are as the test left them.
        assert_eq!(
            run(&worktree, &["status", "--porcelain"]).lines().count(),
            6
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn detached_head_unborn_branch_and_plain_folders() {
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let scratch = tempfile::tempdir().unwrap();
        let detached = scratch.path().join("detached");
        fs::create_dir(&detached).unwrap();
        run(&detached, &["init", "-q"]);
        fs::write(detached.join("a.txt"), "a\n").unwrap();
        run(&detached, &["add", "-A"]);
        run(&detached, &["commit", "-qm", "one"]);
        fs::write(detached.join("a.txt"), "a\nb\n").unwrap();
        run(&detached, &["commit", "-qam", "two"]);
        run(&detached, &["checkout", "-q", "--detach", "HEAD~1"]);
        project_conversation(&state, "git-detached", &detached).await;
        let (status, summary, text) = get(
            &state,
            "/api/v1/conversations/git-detached/workspace/git/summary?root=workspace",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        crate::tests::validate_http_contract("workspaceGitSummary", &summary);
        assert_eq!(summary["detached"], true);
        assert_eq!(summary["branch"], serde_json::Value::Null);
        assert_eq!(summary["worktree"], false);
        assert_eq!(summary["baseRef"], "main");
        assert_eq!(
            (summary["ahead"].as_u64(), summary["behind"].as_u64()),
            (Some(0), Some(1))
        );

        let unborn = scratch.path().join("unborn");
        fs::create_dir(&unborn).unwrap();
        run(&unborn, &["init", "-q"]);
        fs::write(unborn.join("first.txt"), "x\ny\n").unwrap();
        project_conversation(&state, "git-unborn", &unborn).await;
        let (status, summary, text) = get(
            &state,
            "/api/v1/conversations/git-unborn/workspace/git/summary?root=workspace",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        crate::tests::validate_http_contract("workspaceGitSummary", &summary);
        assert_eq!(summary["branch"], "main");
        assert_eq!(summary["head"], serde_json::Value::Null);
        assert_eq!(summary["uncommitted"]["additions"], 2);
        let (_, branch, _) = get(
            &state,
            "/api/v1/conversations/git-unborn/workspace/git/changes?root=workspace&scope=branch",
        )
        .await;
        assert_eq!(branch["available"], false);
        assert_eq!(branch["detail"], "This branch has no commits yet.");

        let plain = scratch.path().join("plain");
        fs::create_dir(&plain).unwrap();
        project_conversation(&state, "git-plain", &plain).await;
        let (status, summary, _) = get(
            &state,
            "/api/v1/conversations/git-plain/workspace/git/summary?root=workspace",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        crate::tests::validate_http_contract("workspaceGitSummary", &summary);
        assert_eq!(
            (summary["available"].clone(), summary["repository"].clone()),
            (true.into(), false.into())
        );
        let (_, changes, _) = get(
            &state,
            "/api/v1/conversations/git-plain/workspace/git/changes?root=workspace",
        )
        .await;
        crate::tests::validate_http_contract("workspaceGitChanges", &changes);
        assert_eq!(changes["available"], false);
        let (status, _, _) = get(
            &state,
            "/api/v1/conversations/git-plain/workspace/git/diff?root=workspace&path=x.txt&scope=uncommitted",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Regression: a thread working in a linked worktree of its Project's saved
    // checkout (the owner's branch threads) got 403 for its Git summary, which
    // the phone turned into a missing Git pill. It now reads the worktree's
    // branch; with worktree chats off the refusal says why in plain text, which
    // the phone shows as "No Git info".
    #[tokio::test]
    async fn a_worktree_threads_git_summary_names_its_branch_or_why_not() {
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let scratch = tempfile::tempdir().unwrap();
        let main = scratch.path().join("main");
        fs::create_dir(&main).unwrap();
        run(&main, &["init", "-q"]);
        fs::write(main.join("a.txt"), "a\n").unwrap();
        run(&main, &["add", "-A"]);
        run(&main, &["commit", "-qm", "base"]);
        let linked = scratch.path().join("worker");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-qb",
                "agent/pill",
                linked.to_str().unwrap(),
            ],
        );
        fs::write(linked.join("a.txt"), "a\nb\nc\n").unwrap();
        let linked = crate::canonical_path::canonicalize(&linked).unwrap();
        project_conversation_in(&state, "git-linked", &main, &linked).await;
        let uri = "/api/v1/conversations/git-linked/workspace/git/summary?root=workspace";

        let (status, summary, text) = get(&state, uri).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        crate::tests::validate_http_contract("workspaceGitSummary", &summary);
        assert_eq!(summary["branch"], "agent/pill", "{summary}");
        assert_eq!(summary["worktree"], true, "{summary}");
        assert_eq!(summary["uncommitted"]["additions"], 2, "{summary}");

        state.store.set_include_worktrees(false).await.unwrap();
        let (status, _, text) = get(&state, uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(text, crate::filesystem::WORKTREE_NOT_IN_PROJECT);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn change_lists_stop_at_the_cap_and_keep_full_totals() {
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let scratch = tempfile::tempdir().unwrap();
        let repo = scratch.path().join("many");
        fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "-q"]);
        fs::write(repo.join("README"), "x\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-qm", "one"]);
        for index in 0..=MAX_CHANGED_FILES {
            fs::write(repo.join(format!("f{index:05}.txt")), "line\n").unwrap();
        }
        project_conversation(&state, "git-many", &repo).await;
        let (status, changes, text) = get(
            &state,
            "/api/v1/conversations/git-many/workspace/git/changes?root=workspace",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        crate::tests::validate_http_contract("workspaceGitChanges", &changes);
        assert_eq!(changes["truncated"], true);
        assert_eq!(
            changes["files"].as_array().unwrap().len(),
            MAX_CHANGED_FILES
        );
        assert_eq!(changes["totals"]["files"], MAX_CHANGED_FILES + 1);
        assert_eq!(changes["totals"]["additions"], MAX_CHANGED_FILES + 1);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn diffs_name_one_file_and_never_reach_a_protected_descendant() {
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let scratch = tempfile::tempdir().unwrap();
        let repo = scratch.path().join("guarded");
        fs::create_dir_all(repo.join("config/private")).unwrap();
        run(&repo, &["init", "-q"]);
        fs::write(repo.join("config/app.txt"), "app\n").unwrap();
        fs::write(repo.join("config/private/key.txt"), "old secret\n").unwrap();
        fs::write(repo.join("config/gone.txt"), "bye\n").unwrap();
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-qm", "base"]);
        fs::write(repo.join("config/app.txt"), "app\nmore\n").unwrap();
        fs::write(repo.join("config/private/key.txt"), "NEW SECRET\n").unwrap();
        fs::remove_file(repo.join("config/gone.txt")).unwrap();
        project_conversation(&state, "git-guarded", &repo).await;
        let base =
            "/api/v1/conversations/git-guarded/workspace/git/diff?root=workspace&scope=uncommitted";
        // A folder, as path or as originalPath, would make Git diff every file
        // beneath it. It is refused before Git runs.
        for query in [
            "&path=config",
            "&path=config/",
            "&path=config/private",
            "&path=config/app.txt&originalPath=config",
        ] {
            let (status, _, text) = get(&state, &format!("{base}{query}")).await;
            assert!(!text.contains("SECRET"), "{query} returned: {text}");
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {text}");
        }
        // One file, including one deleted from the working tree, still works.
        let (status, diff, text) = get(&state, &format!("{base}&path=config/app.txt")).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(diff["diff"].as_str().unwrap().contains("+more"));
        let (status, diff, text) = get(&state, &format!("{base}&path=config/gone.txt")).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(diff["diff"].as_str().unwrap().contains("-bye"));
        // A name neither on disk nor in the compared tree is not a file.
        let (status, _, text) = get(&state, &format!("{base}&path=config/never.txt")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");

        // A protected descendant (a Project root may contain one) stays refused
        // as the file, as originalPath, and through its folder.
        let Resolved::Ready(mut context) = resolve(&state, "git-guarded", Some("workspace"), None)
            .await
            .unwrap_or_else(|_| panic!("resolve"))
        else {
            panic!("repository")
        };
        context.denied =
            vec![crate::canonical_path::canonicalize(repo.join("config/private")).unwrap()];
        for (path, original, expected) in [
            ("config/private/key.txt", None, StatusCode::FORBIDDEN),
            (
                "config/app.txt",
                Some("config/private/key.txt"),
                StatusCode::FORBIDDEN,
            ),
            ("config", None, StatusCode::BAD_REQUEST),
            ("config/app.txt", None, StatusCode::OK),
        ] {
            let query = WorkspaceDiffQuery {
                root: Some("workspace".into()),
                path: Some(path.into()),
                original_path: original.map(Into::into),
                scope: Some("uncommitted".into()),
                ..Default::default()
            };
            let response = context_diff(&context, ChangeScope::Uncommitted, query).await;
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("SECRET"), "{path}: {text}");
            assert_eq!(status, expected, "{path} {original:?}: {text}");
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }
}
