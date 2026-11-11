//! Which repository a Project's folder belongs to, so the same checkout on
//! two computers can be shown together even when the Projects are named
//! differently.
//!
//! The identity is grouping data only. It is derived from a remote URL the
//! folder's owner (or anyone who can write its Git config) chose, so it never
//! authorizes anything: no access, delegation or trust decision may rest on
//! two keys being equal.
//!
//! Version 1 rules (`v1:` keys):
//! - Hosts are compared case-insensitively; paths keep their case, ports,
//!   scheme and any `.git` suffix (a server can hold `repo` and `repo.git`
//!   as different repositories), except on known hosting services whose
//!   behavior is documented (GitHub, GitLab, Bitbucket), where every
//!   supported form of the same repository reduces to one key.
//! - Credentials are never kept. Queries, fragments, `.`/`..` segments,
//!   percent escapes, local paths (POSIX, Windows drive, UNC, `file:`),
//!   remote helpers, loopback hosts and unqualified host names (usually SSH
//!   aliases, which differ per computer) give no identity.
//! - SSH paths relative to the login account keep the account name, since
//!   another account's `team/repo` is a different directory.
//! - The subpath is the folder's place inside its repository as Git spells
//!   it on the computer that holds it, `/`-separated, empty at the top.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use wonder_store::StoredProject;

/// The identity rules a key was made with. A key from different rules never
/// equals a key from these.
const VERSION: &str = "v1";
/// How long a computed identity is reused while none of the Git files it
/// was read from changed. Includes and other config Cupola does not stamp
/// are picked up within this time.
const TTL: Duration = Duration::from_secs(120);
/// Folders remembered at once; the cache is cleared past this.
const MAX_ENTRIES: usize = 512;
/// Identities read from Git at the same time when a list is built.
pub(crate) const CONCURRENCY: usize = 8;

/// The repository a Project's primary folder belongs to.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Repository {
    /// Versioned, credential-free key for grouping the same repository
    /// across computers. Equal keys never authorize anything.
    pub(crate) key: String,
    /// The folder's place inside the repository, `/`-separated; empty at
    /// the repository's top.
    pub(crate) subpath: String,
    /// The repository's own name, as its remote spells it.
    pub(crate) name: String,
}

/// A remote URL reduced to its repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Remote {
    pub(crate) key: String,
    /// Lowercase host after any known-service alias, without port.
    pub(crate) host: String,
    /// Path segments as the remote spells them, `.git` removed from the last.
    pub(crate) segments: Vec<String>,
}

impl Remote {
    pub(crate) fn name(&self) -> &str {
        self.segments.last().map(String::as_str).unwrap_or("")
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Scheme {
    Https,
    Http,
    Ssh,
    Git,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Scheme::Https => "https",
            Scheme::Http => "http",
            Scheme::Ssh => "ssh",
            Scheme::Git => "git",
        }
    }
    fn default_port(self) -> u16 {
        match self {
            Scheme::Https => 443,
            Scheme::Http => 80,
            Scheme::Ssh => 22,
            Scheme::Git => 9418,
        }
    }
}

/// Hosting services whose repository addressing is documented: every
/// protocol reaches the same `owner/name` and paths are case-insensitive.
struct Service {
    host: &'static str,
    /// The service's SSH-over-HTTPS-port host.
    alt_ssh: &'static str,
    /// Exact segment count, or `None` for nested groups (two or more).
    segments: Option<usize>,
}

const SERVICES: &[Service] = &[
    Service {
        host: "github.com",
        alt_ssh: "ssh.github.com",
        segments: Some(2),
    },
    Service {
        host: "gitlab.com",
        alt_ssh: "altssh.gitlab.com",
        segments: None,
    },
    Service {
        host: "bitbucket.org",
        alt_ssh: "altssh.bitbucket.org",
        segments: Some(2),
    },
];

/// The repository a remote URL names, or `None` when it cannot be told
/// apart safely from other repositories (see the module rules).
pub(crate) fn parse_remote(url: &str) -> Option<Remote> {
    let url = url.trim();
    if url.is_empty()
        || url.len() > 2048
        || !url.is_ascii()
        || url.bytes().any(|b| b.is_ascii_control() || b == b' ')
        || url.contains(['?', '#', '%', '\\'])
    {
        return None;
    }
    let (scheme, user, host, port, path, home_relative) = match url.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = match scheme.to_ascii_lowercase().as_str() {
                "https" => Scheme::Https,
                "http" => Scheme::Http,
                "ssh" | "git+ssh" | "ssh+git" => Scheme::Ssh,
                "git" => Scheme::Git,
                // file:, remote helpers and anything else.
                _ => return None,
            };
            let (authority, path) = rest.split_once('/')?;
            let (user, host_port) = match authority.rsplit_once('@') {
                Some((user, host_port)) => (Some(user), host_port),
                None => (None, authority),
            };
            let (host, port) = split_port(host_port)?;
            let port = port.unwrap_or(scheme.default_port());
            let home_relative = scheme == Scheme::Ssh && path.split('/').next() == Some("~");
            let path = if home_relative { &path[1..] } else { path };
            (scheme, user, host, port, path, home_relative)
        }
        None => {
            // `[user@]host:path`, Git's SCP-like form: a colon before any
            // slash. Anything else is a local path.
            if url.contains("::") || url.starts_with(['/', '.', '~']) {
                return None;
            }
            let (user, rest) = match url.split_once('@') {
                Some((user, rest)) if !user.contains([':', '/']) => (Some(user), rest),
                _ => (None, url),
            };
            let (host, path) = if let Some(bracketed) = rest.strip_prefix('[') {
                let (inner, after) = bracketed.split_once(']')?;
                (&rest[..inner.len() + 2], after.strip_prefix(':')?)
            } else {
                let colon = rest.find(':')?;
                if rest[..colon].contains('/') {
                    return None;
                }
                (&rest[..colon], &rest[colon + 1..])
            };
            let (path, home_relative) = match path.strip_prefix('/') {
                Some(absolute) => (absolute, false),
                None => (path.strip_prefix("~/").unwrap_or(path), true),
            };
            (Scheme::Ssh, user, host, 22, path, home_relative)
        }
    };
    let host = normalize_host(host)?;
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" => continue,
            "." | ".." => return None,
            segment => segments.push(segment.to_owned()),
        }
    }
    let spelled = segments.pop()?;
    let last = spelled.strip_suffix(".git").unwrap_or(&spelled).to_owned();
    if last.is_empty() || last.starts_with('~') && segments.is_empty() && !home_relative {
        return None;
    }
    segments.push(last);

    // A known service, reached on its usual port or its SSH-over-443 host.
    let service = SERVICES.iter().find(|service| {
        (host == service.host && port == scheme.default_port())
            || (host == service.alt_ssh && scheme == Scheme::Ssh && port == 443)
    });
    if let Some(service) = service {
        let count_ok = match service.segments {
            Some(count) => segments.len() == count,
            None => segments.len() >= 2,
        };
        if !count_ok || segments.iter().any(|s| s.starts_with('~')) {
            return None;
        }
        let path = segments.join("/").to_ascii_lowercase();
        return Some(Remote {
            key: format!("{VERSION}:{}/{path}", service.host),
            host: service.host.to_owned(),
            segments,
        });
    }

    let account = if home_relative {
        // Relative to an account chosen by this computer's SSH settings
        // when none is named: not comparable.
        let user = user.filter(|user| valid_user(user))?;
        format!("{user}@")
    } else {
        String::new()
    };
    let port = if port == scheme.default_port() {
        String::new()
    } else {
        format!(":{port}")
    };
    let home = if home_relative { "~/" } else { "" };
    let parents: String = segments[..segments.len() - 1]
        .iter()
        .map(|segment| format!("{segment}/"))
        .collect();
    Some(Remote {
        key: format!(
            "{VERSION}:{}://{account}{host}{port}/{home}{parents}{spelled}",
            scheme.as_str(),
        ),
        host,
        segments,
    })
}

fn valid_user(user: &str) -> bool {
    // The account name only; `user:password` never forms part of a key.
    (1..=64).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn split_port(host_port: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if host_port.starts_with('[') {
        let end = host_port.find(']')?;
        let (host, rest) = host_port.split_at(end + 1);
        match rest {
            "" => (host, None),
            rest => (host, Some(rest.strip_prefix(':')?)),
        }
    } else {
        match host_port.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        }
    };
    let port = match port {
        // `host:` with no number is Git's default port.
        Some("") | None => None,
        Some(port) if port.bytes().all(|b| b.is_ascii_digit()) => {
            Some(port.parse::<u16>().ok().filter(|p| *p != 0)?)
        }
        Some(_) => return None,
    };
    Some((host, port))
}

/// Lowercase DNS name or bracketed IPv6 literal. Unqualified names (SSH
/// aliases, single-letter Windows drives) and loopback hosts are refused:
/// they name a different machine on each computer.
fn normalize_host(host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    if let Some(literal) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        let address: std::net::Ipv6Addr = literal.parse().ok()?;
        if address.is_loopback() || address.is_unspecified() {
            return None;
        }
        return Some(format!("[{address}]"));
    }
    let host = host.strip_suffix('.').unwrap_or(&host);
    if host.is_empty()
        || host.len() > 253
        || !host.contains('.')
        || !host
            .split('.')
            .all(|label| !label.is_empty() && label.len() <= 63)
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        || host == "localhost"
        || host.ends_with(".localhost")
    {
        return None;
    }
    if let Ok(address) = host.parse::<std::net::Ipv4Addr>() {
        if address.is_loopback() || address.is_unspecified() {
            return None;
        }
    }
    Some(host.to_owned())
}

// ---------------------------------------------------------------------------
// Reading a folder's identity on this computer
// ---------------------------------------------------------------------------

/// The Git files an identity was read from, by modification time and size.
type Stamp = Vec<Option<(SystemTime, u64)>>;

#[derive(Clone)]
struct Entry {
    repository: Option<Repository>,
    files: Vec<PathBuf>,
    stamp: Stamp,
    read_at: Instant,
}

type Slot = Arc<tokio::sync::Mutex<Option<Entry>>>;

fn cache() -> &'static Mutex<HashMap<PathBuf, Slot>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Slot>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

async fn stamp(files: &[PathBuf]) -> Stamp {
    let mut stamp = Vec::with_capacity(files.len());
    for file in files {
        stamp.push(
            tokio::fs::metadata(file)
                .await
                .ok()
                .and_then(|m| Some((m.modified().ok()?, m.len()))),
        );
    }
    stamp
}

/// The repository holding `project`'s primary folder, when it has one with
/// a comparable origin. Reused until the folder's Git config, `.git` entry
/// or the TTL changes; concurrent readers of one folder share one read.
pub(crate) async fn repository(project: &StoredProject) -> Option<Repository> {
    let root = project
        .roots
        .iter()
        .find(|root| root.id == project.primary_root_id)
        .or_else(|| project.roots.first())?;
    repository_at(Path::new(&root.canonical_path)).await
}

pub(crate) async fn repository_at(folder: &Path) -> Option<Repository> {
    let slot = {
        let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= MAX_ENTRIES && !map.contains_key(folder) {
            map.clear();
        }
        map.entry(folder.to_path_buf()).or_default().clone()
    };
    let mut entry = slot.lock().await;
    if let Some(cached) = entry.as_ref() {
        if cached.read_at.elapsed() < TTL && stamp(&cached.files).await == cached.stamp {
            return cached.repository.clone();
        }
    }
    let (repository, files) = read(folder).await;
    let stamp = stamp(&files).await;
    *entry = Some(Entry {
        repository: repository.clone(),
        files,
        stamp,
        read_at: Instant::now(),
    });
    repository
}

/// Reads Git on this computer through the hardened read-only runner (no
/// user or system config, hooks or helpers), so `insteadOf` rewrites and
/// SSH aliases from the owner's personal settings stay unresolved.
async fn read(folder: &Path) -> (Option<Repository>, Vec<PathBuf>) {
    // Without a repository, `git init` here (or in a parent) shows up as a
    // new `.git` entry in this folder, or within the TTL.
    let mut files = vec![folder.join(".git")];
    let Some(path) = folder.to_str() else {
        return (None, files);
    };
    let Ok(located) = crate::filesystem::read_only_git_bytes(
        path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--is-inside-work-tree",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
            "--show-prefix",
        ],
    )
    .await
    else {
        return (None, files);
    };
    let Ok(located) = String::from_utf8(located) else {
        return (None, files);
    };
    let lines: Vec<&str> = located.lines().collect();
    let (top, git_dir, common, prefix) = match lines.as_slice() {
        ["true", top, git_dir, common] => (*top, *git_dir, *common, ""),
        ["true", top, git_dir, common, prefix] => (*top, *git_dir, *common, *prefix),
        _ => return (None, files),
    };
    files = vec![
        Path::new(top).join(".git"),
        Path::new(common).join("config"),
        Path::new(git_dir).join("config.worktree"),
    ];
    let Ok(url) =
        crate::filesystem::read_only_git_bytes(path, &["remote", "get-url", "origin"]).await
    else {
        return (None, files);
    };
    let Some(remote) = std::str::from_utf8(&url).ok().and_then(parse_remote) else {
        return (None, files);
    };
    let repository = Repository {
        key: remote.key.clone(),
        subpath: prefix.trim_end_matches('/').to_owned(),
        name: remote.name().to_owned(),
    };
    (Some(repository), files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls_reduce_to_collision_safe_keys() {
        let gh = Some("v1:github.com/swaymun/wonder");
        for (url, key) in [
            // Every GitHub form of one repository is one key.
            ("https://github.com/swaymun/wonder.git", gh),
            ("https://github.com/Swaymun/Wonder", gh),
            ("https://user:secret@github.com/swaymun/wonder/", gh),
            ("http://github.com/swaymun/wonder", gh),
            ("git@github.com:swaymun/wonder.git", gh),
            ("ssh://git@github.com/swaymun/wonder", gh),
            ("ssh://git@github.com:22/swaymun/wonder", gh),
            ("git+ssh://git@GitHub.COM/swaymun/wonder", gh),
            ("ssh://git@ssh.github.com:443/swaymun/wonder.git", gh),
            ("git://github.com/swaymun/wonder.git", gh),
            ("  https://github.com/swaymun/wonder\n", gh),
            ("https://github.com/swaymun/wonder/tree", None),
            ("https://github.com/swaymun", None),
            // A non-standard port on a known host is not that service.
            (
                "https://github.com:8443/swaymun/wonder",
                Some("v1:https://github.com:8443/swaymun/wonder"),
            ),
            // GitLab nests groups.
            (
                "git@gitlab.com:Group/Sub/Repo.git",
                Some("v1:gitlab.com/group/sub/repo"),
            ),
            (
                "https://gitlab.com/group/sub/repo",
                Some("v1:gitlab.com/group/sub/repo"),
            ),
            (
                "ssh://git@altssh.bitbucket.org:443/team/repo.git",
                Some("v1:bitbucket.org/team/repo"),
            ),
            // Generic servers keep path case, port and scheme.
            (
                "ssh://git@git.example:2222/Team/Repo",
                Some("v1:ssh://git.example:2222/Team/Repo"),
            ),
            (
                "ssh://git@git.example:2223/Team/Repo",
                Some("v1:ssh://git.example:2223/Team/Repo"),
            ),
            (
                "ssh://git@git.example/team/repo",
                Some("v1:ssh://git.example/team/repo"),
            ),
            (
                "ssh://git@GIT.Example.:22/Team/Repo.git",
                Some("v1:ssh://git.example/Team/Repo.git"),
            ),
            // A generic server may hold `Repo` and `Repo.git` as two
            // repositories, so the suffix stays in the key.
            (
                "ssh://git@git.example/srv/Repo",
                Some("v1:ssh://git.example/srv/Repo"),
            ),
            (
                "ssh://git@git.example/srv/Repo.git",
                Some("v1:ssh://git.example/srv/Repo.git"),
            ),
            (
                "https://git.example/Team/Repo.git",
                Some("v1:https://git.example/Team/Repo.git"),
            ),
            (
                "https://git.example/Team/Repo",
                Some("v1:https://git.example/Team/Repo"),
            ),
            (
                "https://git.example:443/Team/Repo",
                Some("v1:https://git.example/Team/Repo"),
            ),
            (
                "http://git.example/Team/Repo",
                Some("v1:http://git.example/Team/Repo"),
            ),
            (
                "https://ci:token@git.example/Team/Repo",
                Some("v1:https://git.example/Team/Repo"),
            ),
            (
                "ssh://git@[2001:db8::1]:2222/srv/repo",
                Some("v1:ssh://[2001:db8::1]:2222/srv/repo"),
            ),
            // SCP-like paths are relative to the login account.
            (
                "git@git.example:team/repo.git",
                Some("v1:ssh://git@git.example/~/team/repo.git"),
            ),
            (
                "deploy@git.example:team/repo.git",
                Some("v1:ssh://deploy@git.example/~/team/repo.git"),
            ),
            (
                "ssh://alice@git.example/~/repo",
                Some("v1:ssh://alice@git.example/~/repo"),
            ),
            (
                "git@git.example:/srv/git/repo.git",
                Some("v1:ssh://git.example/srv/git/repo.git"),
            ),
            ("git.example:team/repo", None),
            ("ssh://git.example/~/repo", None),
            // SSH aliases and other unqualified names differ per computer.
            ("github-work:swaymun/wonder.git", None),
            ("git@gh:swaymun/wonder", None),
            ("ssh://myserver/srv/repo", None),
            // Local remotes.
            ("/Users/me/repo.git", None),
            ("./repo", None),
            ("../repo", None),
            ("~/repo", None),
            ("repo", None),
            ("file:///srv/repo.git", None),
            ("FILE:///srv/repo.git", None),
            ("C:\\Users\\me\\repo", None),
            ("C:/Users/me/repo", None),
            ("C:repo", None),
            ("\\\\server\\share\\repo", None),
            ("//server/share/repo", None),
            ("ssh://localhost/srv/repo", None),
            ("ssh://git@127.0.0.1/srv/repo", None),
            ("https://[::1]/repo", None),
            ("git@sub.localhost:repo", None),
            // Malformed or ambiguous.
            ("https://github.com/o/r?ref=x", None),
            ("https://github.com/o/r#readme", None),
            ("https://git.example/a/../r", None),
            ("https://git.example/Team%2FRepo", None),
            ("https://git.example:99999/r", None),
            ("https://git.example:x/r", None),
            ("https://git.example", None),
            ("https://git.example/", None),
            ("https://git.example/.git", None),
            ("https://gît.example/r", None),
            ("https://git example/r", None),
            ("ftp://git.example/r", None),
            ("persistent-https://git.example/r", None),
            ("ext::ssh git.example repo", None),
            ("rad::z6Mk/repo", None),
            ("", None),
        ] {
            assert_eq!(parse_remote(url).map(|r| r.key).as_deref(), key, "{url:?}");
        }
    }

    #[test]
    fn remote_names_keep_their_spelling() {
        let remote = parse_remote("git@github.com:Swaymun/Wonder.git").unwrap();
        assert_eq!(remote.name(), "Wonder");
        assert_eq!(remote.host, "github.com");
        assert_eq!(remote.segments, ["Swaymun", "Wonder"]);
        let remote = parse_remote("ssh://git@ssh.github.com:443/o/r").unwrap();
        assert_eq!(remote.host, "github.com");
    }

    fn git(dir: &Path, args: &[&str]) {
        use std::io::{Read, Seek};
        // Output goes to a file, not a pipe, so a Git that never exits can be
        // killed and named instead of holding the whole run.
        let mut log = tempfile::tempfile().unwrap();
        let mut child = crate::test_git::git()
            .arg("-C")
            .arg(dir)
            .args(["-c", "protocol.file.allow=always"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log.try_clone().unwrap())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if started.elapsed() > std::time::Duration::from_secs(60) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("git {args:?} did not finish in 60 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let mut text = String::new();
        log.rewind().unwrap();
        log.read_to_string(&mut text).unwrap();
        assert!(status.success(), "git {args:?}: {text}");
    }

    fn repo(dir: &Path, origin: Option<&str>) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        if let Some(origin) = origin {
            git(dir, &["remote", "add", "origin", origin]);
        }
        std::fs::write(dir.join("README"), "x").unwrap();
        git(dir, &["add", "README"]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    #[cfg(not(windows))]
    fn add_submodule(main: &Path, source: &Path, path: &str) {
        git(
            main,
            &["submodule", "add", "-q", source.to_str().unwrap(), path],
        );
    }

    /// `git submodule add` is a shell script, and the shell MinGit ships spins
    /// forever under Wine (Cygwin cannot find its FAST_CWD pointer; measured
    /// with `git submodule status` too), which held every Windows land for its
    /// whole time limit. A clone whose Git directory lives inside the main
    /// repository is the same layout: a `.git` file in the folder pointing at
    /// `.git/modules/<path>`.
    #[cfg(windows)]
    fn add_submodule(main: &Path, source: &Path, path: &str) {
        let module = main.join(".git").join("modules").join(path);
        std::fs::create_dir_all(module.parent().unwrap()).unwrap();
        git(
            main,
            &[
                "clone",
                "-q",
                "--separate-git-dir",
                module.to_str().unwrap(),
                source.to_str().unwrap(),
                path,
            ],
        );
    }

    #[tokio::test]
    async fn folders_read_their_repository_on_this_computer() {
        let temp = tempfile::tempdir().unwrap();
        let base = crate::canonical_path::canonicalize(temp.path()).unwrap();
        let main = base.join("Cupola");
        repo(&main, Some("git@github.com:Swaymun/Wonder.git"));
        let wonder = Repository {
            key: "v1:github.com/swaymun/wonder".into(),
            subpath: String::new(),
            name: "Wonder".into(),
        };
        assert_eq!(repository_at(&main).await, Some(wonder.clone()));

        // A subfolder keeps Git's spelling of its place in the repository.
        let nested = main.join("Apps").join("iOS app");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            repository_at(&nested).await,
            Some(Repository {
                subpath: "Apps/iOS app".into(),
                ..wonder.clone()
            })
        );

        // A linked worktree is the same repository.
        let linked = base.join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        );
        assert_eq!(repository_at(&linked).await, Some(wonder.clone()));

        // A submodule is its own repository, read from its own origin.
        let library = base.join("library");
        repo(&library, Some("https://git.example/Team/Library.git"));
        add_submodule(&main, &library, "vendor/library");
        // The submodule's origin is the local clone source: no identity.
        assert_eq!(repository_at(&main.join("vendor/library")).await, None);
        git(
            &main.join("vendor/library"),
            &[
                "remote",
                "set-url",
                "origin",
                "https://git.example/Team/Library.git",
            ],
        );
        assert_eq!(
            repository_at(&main.join("vendor/library")).await,
            Some(Repository {
                key: "v1:https://git.example/Team/Library.git".into(),
                subpath: String::new(),
                name: "Library".into(),
            })
        );

        // A local or missing origin gives no identity, and a plain folder
        // none at all.
        let local = base.join("local");
        repo(&local, Some(main.to_str().unwrap()));
        assert_eq!(repository_at(&local).await, None);
        let bare = base.join("no-origin");
        repo(&bare, None);
        assert_eq!(repository_at(&bare).await, None);
        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(repository_at(&plain).await, None);
    }

    #[tokio::test]
    async fn a_changed_origin_is_read_again() {
        let temp = tempfile::tempdir().unwrap();
        let dir = crate::canonical_path::canonicalize(temp.path())
            .unwrap()
            .join("r");
        repo(&dir, Some("https://git.example/Team/One.git"));
        assert_eq!(repository_at(&dir).await.unwrap().name, "One");
        // A different length always changes the stamp, whatever the
        // filesystem's timestamp resolution.
        git(
            &dir,
            &[
                "remote",
                "set-url",
                "origin",
                "https://git.example/Team/Second.git",
            ],
        );
        assert_eq!(repository_at(&dir).await.unwrap().name, "Second");
        // A folder that becomes a repository is noticed through its `.git`.
        let later = crate::canonical_path::canonicalize(temp.path())
            .unwrap()
            .join("later");
        std::fs::create_dir_all(&later).unwrap();
        assert_eq!(repository_at(&later).await, None);
        repo(&later, Some("https://git.example/Team/Later.git"));
        assert_eq!(repository_at(&later).await.unwrap().name, "Later");
    }
}
