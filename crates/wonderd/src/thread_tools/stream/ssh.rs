//! The one place that builds Cupola's `ssh` command lines and classifies what OpenSSH says when it refuses
//! (docs/ssh-keys.md): the remote-screen tool and `ssh_spike` both use it, so the host-key rules have one owner.
//!
//! OpenSSH owns keys. The user's `known_hosts` is read, never written; a host key that is not pinned, or has
//! changed, stops the connection before a byte is sent. Nothing here creates, reads or passes a key.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::{Child, ChildStderr, Command},
    time::timeout,
};

/// Why OpenSSH refused, as a fixed class. The matched text is never kept or returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshClass {
    HostKeyUnpinned,
    HostKeyChanged,
    AuthRejected,
    KeyFileUnusable,
}

impl SshClass {
    /// What to do about it, in words for the owner. Fixed text: no remote output, addresses or key material, and
    /// never a suggestion to turn host-key checking off.
    pub(crate) fn fix(self) -> &'static str {
        match self {
            Self::HostKeyUnpinned => "The alias's host key is not pinned. Run `ssh ALIAS` once in a terminal, compare the fingerprint with the one on the machine (ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub), and answer yes. Cupola never accepts a key on its own.",
            Self::HostKeyChanged => "The host answered with a different key than the one pinned, so nothing was sent. If you rebuilt or re-addressed it yourself, confirm the new fingerprint on the machine, run `ssh-keygen -R ALIAS`, then pin it once. If you did not, do not accept it: another machine may be answering.",
            Self::AuthRejected => "The host's key was accepted but it rejected ours. Check `ssh -v ALIAS` for the offered keys; IdentityFile must be the private key (not the .pub) and its .pub must be in the account's authorized_keys. Set IdentitiesOnly yes in the alias if many keys are loaded.",
            Self::KeyFileUnusable => "ssh could not use the alias's key file. Make it the private key (not the .pub), chmod 600 it, and check it with `ssh-keygen -y -f FILE`.",
        }
    }
}

/// Classify OpenSSH's own stderr. The text is only matched, never kept or returned.
pub(crate) fn classify_stderr(stderr: &str) -> Option<SshClass> {
    let has = |needle: &str| stderr.contains(needle);
    if has("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || has("Offending ")
        || has("has changed and you have requested strict checking")
    {
        Some(SshClass::HostKeyChanged)
    } else if has("UNPROTECTED PRIVATE KEY")
        || has("bad permissions")
        || has("invalid format")
        || has("Load key")
    {
        Some(SshClass::KeyFileUnusable)
    } else if has("Permission denied (") || has("Too many authentication failures") {
        Some(SshClass::AuthRejected)
    } else if has("Host key verification failed")
        || has("No host key is known")
        || has("host key is known for")
    {
        Some(SshClass::HostKeyUnpinned)
    } else {
        None
    }
}

/// At most 8 KiB of ssh's stderr, waiting at most a second, so a stuck child cannot hold the caller.
pub(crate) async fn read_stderr(stderr: Option<ChildStderr>) -> String {
    let Some(stderr) = stderr else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = timeout(
        Duration::from_secs(1),
        stderr.take(8 * 1024).read_to_end(&mut bytes),
    )
    .await;
    String::from_utf8_lossy(&bytes).into_owned()
}

/// An alias as `~/.ssh/config` names it: letters, digits, `.`, `_`, `-`, starting alphanumeric. No `user@`, address
/// brackets, `:port`, whitespace, option-like text or shell metacharacters.
pub(crate) fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 253
        && alias.as_bytes()[0].is_ascii_alphanumeric()
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The OpenSSH client, if this computer has one.
pub(crate) fn default_program() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(windows) {
        &["C:\\Windows\\System32\\OpenSSH\\ssh.exe"]
    } else {
        &["/usr/bin/ssh"]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Arguments every Cupola ssh connection starts with: batch mode, strict host keys read from the user's
/// `known_hosts` and never written, publickey only, and no agent, X11 or port forwarding. `config` is a file for
/// tests; production passes `None` so OpenSSH reads the user's own config.
fn hardened_args(config: Option<&Path>, alias: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(config) = config {
        args.push("-F".into());
        args.push(config.to_string_lossy().into_owned());
    }
    for option in [
        "-T",
        "-oBatchMode=yes",
        "-oStrictHostKeyChecking=yes",
        // The user's known_hosts is read, never written: no silent key updates or DNS-supplied keys.
        "-oUpdateHostKeys=no",
        "-oVerifyHostKeyDNS=no",
        "-oPreferredAuthentications=publickey",
        "-oPasswordAuthentication=no",
        "-oKbdInteractiveAuthentication=no",
        "-oForwardAgent=no",
        "-oForwardX11=no",
        "-oClearAllForwardings=yes",
        "-oPermitLocalCommand=no",
        "-oControlMaster=no",
        "-oControlPath=none",
        "-oConnectTimeout=10",
        "-oServerAliveInterval=5",
        "-oServerAliveCountMax=2",
        "-oLogLevel=ERROR",
        "--",
    ] {
        args.push(option.into());
    }
    args.push(alias.to_owned());
    args
}

/// An `ssh` child for `alias` whose remote command is `remote` (already shell-quoted by the caller), with piped
/// stdio and `kill_on_drop`. `None` when the alias is not a plain alias.
pub(crate) fn command(
    program: &Path,
    config: Option<&Path>,
    alias: &str,
    remote: &str,
) -> Option<Command> {
    if !valid_alias(alias) {
        return None;
    }
    let mut command = Command::new(program);
    command
        .args(hardened_args(config, alias))
        .arg(remote)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Read only after ssh exits, bounded and classified; never returned.
        .stderr(Stdio::piped());
    Some(command)
}

/// A spawned ssh child with its pipes.
pub(crate) struct Spawned {
    pub(crate) child: Child,
    pub(crate) stdin: tokio::process::ChildStdin,
    pub(crate) stdout: BufReader<tokio::process::ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
}

pub(crate) fn spawn(mut command: Command) -> std::io::Result<Spawned> {
    let mut child = command.spawn()?;
    let missing = || std::io::Error::other("ssh pipe missing");
    let stdin = child.stdin.take().ok_or_else(missing)?;
    let stdout = BufReader::new(child.stdout.take().ok_or_else(missing)?);
    let stderr = child.stderr.take();
    Ok(Spawned {
        child,
        stdin,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_are_plain_names_only() {
        for bad in [
            "",
            "-Fbad",
            "host;id",
            "user@host",
            "$(id)",
            "host\nother",
            "a b",
            "a*",
            "a?",
            "!a",
            "[::1]",
            "host:22",
            "../x",
            &"a".repeat(254),
        ] {
            assert!(!valid_alias(bad), "{bad:?}");
        }
        for good in ["lab", "boat-1", "a.b_c", "192.0.2.1", &"a".repeat(253)] {
            assert!(valid_alias(good), "{good:?}");
        }
    }

    #[test]
    fn the_command_never_loosens_host_key_checking_or_forwards_anything() {
        let command = command(Path::new("/usr/bin/ssh"), None, "lab", "true").unwrap();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for required in [
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oUpdateHostKeys=no",
            "-oVerifyHostKeyDNS=no",
            "-oForwardAgent=no",
            "-oForwardX11=no",
            "-oClearAllForwardings=yes",
            "-oPasswordAuthentication=no",
            "-oControlMaster=no",
        ] {
            assert!(args.contains(&required.to_owned()), "{required}");
        }
        assert!(!args.iter().any(|a| a.contains("StrictHostKeyChecking=no")
            || a.contains("accept-new")
            || a.contains("/dev/null")
            || a == "-i"
            || a == "-F"));
        // `--` ends options before the alias, so an alias can never be read as one.
        let dashes = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dashes + 1], "lab");
        assert_eq!(args.last().unwrap(), "true");
        assert!(
            super::command(Path::new("/usr/bin/ssh"), None, "-oProxyCommand=x", "true").is_none()
        );
    }

    #[test]
    fn real_openssh_messages_are_classified() {
        let cases = [
            ("No ED25519 host key is known for [127.0.0.1]:62004 and you have requested strict checking.\nHost key verification failed.\n", Some(SshClass::HostKeyUnpinned)),
            ("@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\nOffending ED25519 key in /Users/x/.ssh/known_hosts:2\nHost key for [127.0.0.1]:62043 has changed and you have requested strict checking.\nHost key verification failed.\n", Some(SshClass::HostKeyChanged)),
            ("x@127.0.0.1: Permission denied (publickey).\n", Some(SshClass::AuthRejected)),
            ("Received disconnect from 1.2.3.4 port 22:2: Too many authentication failures\n", Some(SshClass::AuthRejected)),
            ("Permissions 0644 for '/x/id' are too open.\nLoad key \"/x/id\": bad permissions\nx@h: Permission denied (publickey).\n", Some(SshClass::KeyFileUnusable)),
            ("Load key \"/x/id.pub\": invalid format\n", Some(SshClass::KeyFileUnusable)),
            ("ssh: connect to host h port 22: Connection timed out\n", None),
            ("", None),
        ];
        for (stderr, expected) in cases {
            assert_eq!(classify_stderr(stderr), expected, "{stderr}");
        }
        for class in [
            SshClass::HostKeyUnpinned,
            SshClass::HostKeyChanged,
            SshClass::AuthRejected,
            SshClass::KeyFileUnusable,
        ] {
            let fix = class.fix();
            assert!(!fix.contains("StrictHostKeyChecking no") && !fix.contains("=no"));
        }
    }
}
