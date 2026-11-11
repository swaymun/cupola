//! Diagnostics-only, no-model SSH transport experiment. Not a production route.
//!
//! Run the ignored test with an already verified SSH alias; OpenSSH owns key
//! handling. No provider credentials, existing threads or Cupola data are read.
//! Only initialize/initialized/thread-start are sent. No turn can be requested.
use serde::Serialize;
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::{timeout, Instant},
};

const MAX_FRAME: usize = 64 * 1024;
const MAX_FRAMES: usize = 64;
const REMOTE: &str = include_str!("ssh_spike/agent_probe.py");
use crate::thread_tools::stream::ssh::{self as stream_ssh, classify_stderr, SshClass};

/// Content-free evidence. Never expose remote response/error bodies or stderr.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub thread_started: bool,
    pub correlated_event: bool,
    pub frames_received: usize,
    pub first_thread_event_ms: u128,
    pub elapsed_ms: u128,
    pub remote_cleaned: bool,
}

/// Fixed error classes deliberately omit raw SSH/provider output and addresses.
#[derive(Debug, PartialEq, Eq)]
pub enum ProbeError {
    InvalidAlias,
    Transport,
    /// The alias's host key is not in the user's known_hosts: nothing was sent.
    HostKeyUnpinned,
    /// The host answered with another key than the pinned one: nothing was sent.
    HostKeyChanged,
    /// The host's key was accepted but it rejected ours.
    AuthRejected,
    /// ssh could not use the alias's key file (wrong permissions, a `.pub`, or unreadable).
    KeyFileUnusable,
    Deadline,
    FrameLimit,
    Protocol,
    RemoteRejected,
    CleanupUnconfirmed,
}

impl ProbeError {
    /// What to do about this failure, in words for the person running the probe. Fixed text: it never
    /// carries remote output, addresses or key material.
    pub fn fix(&self) -> &'static str {
        match self {
            Self::InvalidAlias => "Use an alias from ~/.ssh/config: letters, digits, '.', '_' and '-' only.",
            Self::HostKeyUnpinned => SshClass::HostKeyUnpinned.fix(),
            Self::HostKeyChanged => SshClass::HostKeyChanged.fix(),
            Self::AuthRejected => SshClass::AuthRejected.fix(),
            Self::KeyFileUnusable => SshClass::KeyFileUnusable.fix(),
            Self::Transport => "The connection failed. Run `ssh -o BatchMode=yes ALIAS true` to see why.",
            Self::Deadline => "The remote did not answer in time.",
            Self::FrameLimit | Self::Protocol => "The remote did not speak the expected protocol.",
            Self::RemoteRejected => "The remote agent refused the request.",
            Self::CleanupUnconfirmed => "The remote did not confirm removing its temporary files; check ~/.cache on the host.",
        }
    }
}

/// Classify OpenSSH's own stderr into a fixed class. The text is only matched, never kept or returned.
fn classify_ssh_stderr(stderr: &str) -> Option<ProbeError> {
    classify_stderr(stderr).map(|class| match class {
        SshClass::HostKeyChanged => ProbeError::HostKeyChanged,
        SshClass::KeyFileUnusable => ProbeError::KeyFileUnusable,
        SshClass::AuthRejected => ProbeError::AuthRejected,
        SshClass::HostKeyUnpinned => ProbeError::HostKeyUnpinned,
    })
}

fn shell_quote(value: &str) -> String {
    stream_ssh::shell_quote(value)
}

fn ssh_command(alias: &str) -> Result<Command, ProbeError> {
    // The shared builder owns the hardened arguments (strict host keys, read-only known_hosts, no forwarding).
    // This fixed, reviewed source is the only command sent to the remote shell. The local user's trusted SSH
    // config supplies the alias and pinned keys.
    stream_ssh::command(
        std::path::Path::new("/usr/bin/ssh"),
        None,
        alias,
        &format!("python3 -c {}", shell_quote(REMOTE)),
    )
    .ok_or(ProbeError::InvalidAlias)
}

async fn frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Value, ProbeError> {
    let mut bytes = Vec::new();
    let n = reader
        .take((MAX_FRAME + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|_| ProbeError::Transport)?;
    if n > MAX_FRAME {
        return Err(ProbeError::FrameLimit);
    }
    if n == 0 || bytes.last() != Some(&b'\n') {
        return Err(ProbeError::Transport);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| ProbeError::Protocol)?;
    if !value.is_object() {
        return Err(ProbeError::Protocol);
    }
    Ok(value)
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    value: Value,
) -> Result<(), ProbeError> {
    let mut bytes = serde_json::to_vec(&value).map_err(|_| ProbeError::Protocol)?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|_| ProbeError::Transport)?;
    writer.flush().await.map_err(|_| ProbeError::Transport)
}

async fn confirm_cleanup<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    remaining: usize,
) -> Result<(), ProbeError> {
    for _ in 0..remaining {
        let value = frame(reader).await?;
        if value["method"] == "cupolaSpike/cleaned" {
            return if value["params"]["removed"] == true {
                Ok(())
            } else {
                Err(ProbeError::CleanupUnconfirmed)
            };
        }
    }
    Err(ProbeError::FrameLimit)
}

/// Connect through an existing trusted OpenSSH alias and create one empty,
/// ephemeral Codex thread. No prompt, model turn or production API integration.
pub async fn probe(alias: &str) -> Result<ProbeReport, ProbeError> {
    probe_with(ssh_command(alias)?).await
}

async fn probe_with(mut command: Command) -> Result<ProbeReport, ProbeError> {
    let mut child = command.spawn().map_err(|_| ProbeError::Transport)?;
    let stderr = child.stderr.take();
    let mut input = child.stdin.take().ok_or(ProbeError::Transport)?;
    let mut output = BufReader::new(child.stdout.take().ok_or(ProbeError::Transport)?);
    let started = Instant::now();
    let result =
        timeout(Duration::from_secs(45), async {
            send(&mut input, json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"cupola_ssh_spike","title":"Cupola SSH spike","version":"0.1"}
        }})).await?;
            let mut initialized = false;
            let mut response_id = None;
            let mut event_id = None;
            let mut first_event = 0;
            for count in 1..=MAX_FRAMES {
                let value = frame(&mut output).await?;
                if value.get("error").is_some() {
                    return Err(ProbeError::RemoteRejected);
                }
                if value["id"] == 1 {
                    if initialized || !value["result"].is_object() {
                        return Err(ProbeError::Protocol);
                    }
                    initialized = true;
                    send(&mut input, json!({"method":"initialized"})).await?;
                    send(
                        &mut input,
                        json!({"id":2,"method":"thread/start","params":{
                            "ephemeral":true,"approvalPolicy":"never","sandbox":"read-only"
                        }}),
                    )
                    .await?;
                } else if value["id"] == 2 {
                    if !initialized || response_id.is_some() {
                        return Err(ProbeError::Protocol);
                    }
                    response_id = Some(thread_id(&value["result"]["thread"]["id"])?);
                } else if value["method"] == "thread/started" {
                    if !initialized || event_id.is_some() {
                        return Err(ProbeError::Protocol);
                    }
                    event_id = Some(thread_id(&value["params"]["thread"]["id"])?);
                    first_event = started.elapsed().as_millis();
                } else if value["method"] == "cupolaSpike/cleaned" {
                    return Err(ProbeError::CleanupUnconfirmed);
                }
                if let (Some(response), Some(event)) = (&response_id, &event_id) {
                    if response != event {
                        return Err(ProbeError::Protocol);
                    }
                    input.shutdown().await.map_err(|_| ProbeError::Transport)?;
                    drop(input);
                    // The remote wrapper acknowledges only after the CLI exited and
                    // its private temporary HOME was removed, not on stdin EOF alone.
                    confirm_cleanup(&mut output, MAX_FRAMES - count).await?;
                    if !child
                        .wait()
                        .await
                        .map_err(|_| ProbeError::Transport)?
                        .success()
                    {
                        return Err(ProbeError::Transport);
                    }
                    return Ok(ProbeReport {
                        thread_started: true,
                        correlated_event: true,
                        frames_received: count,
                        first_thread_event_ms: first_event,
                        elapsed_ms: started.elapsed().as_millis(),
                        remote_cleaned: true,
                    });
                }
            }
            Err(ProbeError::FrameLimit)
        })
        .await
        .unwrap_or(Err(ProbeError::Deadline));
    // Reap only our own SSH process on all failures. The remote watchdog ends
    // its own CLI process group after 25 s even if the connection has vanished.
    if result.is_err() {
        let _ = timeout(Duration::from_secs(3), child.kill()).await;
    }
    // ssh exiting before any protocol frame is the "failed silently" case: say whether it was the host
    // key or our key. The bytes are classified and dropped, so the report stays content-free.
    match result {
        Err(ProbeError::Transport) => {
            Err(classify_ssh_stderr(&stream_ssh::read_stderr(stderr).await)
                .unwrap_or(ProbeError::Transport))
        }
        other => other,
    }
}

fn thread_id(value: &Value) -> Result<String, ProbeError> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .map(str::to_owned)
        .ok_or(ProbeError::Protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ssh_spike_bounds_untrusted_frames_without_echoing_them() {
        timeout(Duration::from_secs(10), async {
            let cases = [
                (b"{\"method\":\"thread/started\"}\n".to_vec(), None),
                (b"banner or a secret\n".to_vec(), Some(ProbeError::Protocol)),
                (b"[]\n".to_vec(), Some(ProbeError::Protocol)),
                (b"{}".to_vec(), Some(ProbeError::Transport)),
                (vec![b'x'; MAX_FRAME + 1], Some(ProbeError::FrameLimit)),
            ];
            for (bytes, error) in cases {
                let result = frame(&mut &bytes[..]).await;
                assert_eq!(result.err(), error);
            }
            // The acknowledgement's method alone does not prove deletion.
            let cleanup_cases = [
                (json!({"removed":true}), Ok(())),
                (
                    json!({"removed":false}),
                    Err(ProbeError::CleanupUnconfirmed),
                ),
                (json!({}), Err(ProbeError::CleanupUnconfirmed)),
                (Value::Null, Err(ProbeError::CleanupUnconfirmed)),
                (json!({"removed":null}), Err(ProbeError::CleanupUnconfirmed)),
                (
                    json!({"removed":"true"}),
                    Err(ProbeError::CleanupUnconfirmed),
                ),
                (json!({"removed":1}), Err(ProbeError::CleanupUnconfirmed)),
            ];
            for (params, expected) in cleanup_cases {
                let mut bytes = serde_json::to_vec(&json!({
                    "method":"cupolaSpike/cleaned", "params":params
                }))
                .unwrap();
                bytes.push(b'\n');
                assert_eq!(confirm_cleanup(&mut &bytes[..], 1).await, expected);
            }
        })
        .await
        .expect("bounded frame decoder timed out");
    }

    #[tokio::test]
    async fn ssh_spike_refuses_option_and_shell_injection() {
        timeout(Duration::from_secs(10), async {
            for alias in [
                "",
                "-Fbad",
                "host;id",
                "user@host",
                "$(id)",
                "host\nother",
                "a b",
            ] {
                assert!(matches!(ssh_command(alias), Err(ProbeError::InvalidAlias)));
            }
            for alias in ["linux-build-host", "localhost", "192.0.2.1"] {
                assert!(ssh_command(alias).is_ok());
            }
            assert_eq!(shell_quote("a'b"), "'a'\\''b'");
            // Exercise the full command OpenSSH gives the remote login shell,
            // including the embedded payload's quotes, newlines and metacharacters.
            // Intercept python3 so this local security check cannot start a CLI.
            let command = ssh_command("linux-build-host").unwrap();
            let remote = command.as_std().get_args().last().unwrap().to_str().unwrap();
            let output = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "python3() {{ [ \"$#\" -eq 2 ] && [ \"$1\" = '-c' ] || return 2; printf '%s' \"$2\"; }}\n{remote}"
                ))
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            assert_eq!(output.stdout, REMOTE.as_bytes());
        })
        .await
        .expect("shell injection check timed out");
    }

    #[test]
    fn ssh_spike_never_loosens_host_key_checking_and_names_key_failures() {
        let command = ssh_command("linux-build-host").unwrap();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"-oStrictHostKeyChecking=yes".to_owned()));
        assert!(args.contains(&"-oUpdateHostKeys=no".to_owned()));
        assert!(!args.iter().any(|a| a.contains("StrictHostKeyChecking=no")
            || a.contains("accept-new")
            || a.contains("/dev/null")));

        // Real OpenSSH 10 messages, as captured by scripts/vms/test-sshhint.py.
        let cases = [
            ("No ED25519 host key is known for [127.0.0.1]:62004 and you have requested strict checking.\nHost key verification failed.\n", Some(ProbeError::HostKeyUnpinned)),
            ("@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\nOffending ED25519 key in /Users/x/.ssh/known_hosts:2\nHost key for [127.0.0.1]:62043 has changed and you have requested strict checking.\nHost key verification failed.\n", Some(ProbeError::HostKeyChanged)),
            ("x@127.0.0.1: Permission denied (publickey).\n", Some(ProbeError::AuthRejected)),
            ("Received disconnect from 1.2.3.4 port 22:2: Too many authentication failures\n", Some(ProbeError::AuthRejected)),
            ("Permissions 0644 for '/x/id' are too open.\nLoad key \"/x/id\": bad permissions\nx@h: Permission denied (publickey).\n", Some(ProbeError::KeyFileUnusable)),
            ("Load key \"/x/id.pub\": invalid format\n", Some(ProbeError::KeyFileUnusable)),
            ("ssh: connect to host h port 22: Connection timed out\n", None),
            ("", None),
        ];
        for (stderr, expected) in cases {
            assert_eq!(classify_ssh_stderr(stderr), expected, "{stderr}");
        }
        // Every class has a fix, and none tells anyone to turn checking off or carries remote output.
        for error in [
            ProbeError::InvalidAlias,
            ProbeError::Transport,
            ProbeError::HostKeyUnpinned,
            ProbeError::HostKeyChanged,
            ProbeError::AuthRejected,
            ProbeError::KeyFileUnusable,
            ProbeError::Deadline,
            ProbeError::FrameLimit,
            ProbeError::Protocol,
            ProbeError::RemoteRejected,
            ProbeError::CleanupUnconfirmed,
        ] {
            let fix = error.fix();
            assert!(
                !fix.is_empty()
                    && !fix.contains("StrictHostKeyChecking no")
                    && !fix.contains("=no"),
                "{error:?}"
            );
        }
    }

    #[tokio::test]
    async fn ssh_spike_names_the_key_failure_when_ssh_exits_before_any_frame() {
        // A stand-in for ssh that fails the way OpenSSH does; the probe must report the class, not a bare transport error.
        for (stderr, expected) in [
            ("Host key verification failed.", ProbeError::HostKeyUnpinned),
            (
                "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\nHost key verification failed.",
                ProbeError::HostKeyChanged,
            ),
            (
                "x@h: Permission denied (publickey).",
                ProbeError::AuthRejected,
            ),
            (
                "ssh: connect to host h port 22: Connection timed out",
                ProbeError::Transport,
            ),
        ] {
            let mut command = Command::new("/bin/sh");
            command
                .arg("-c")
                .arg(format!("printf '%s\\n' '{stderr}' >&2; exit 255"))
                .kill_on_drop(true)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let result = timeout(Duration::from_secs(10), probe_with(command))
                .await
                .expect("probe hung");
            assert_eq!(result.err(), Some(expected), "{stderr}");
        }
    }

    #[tokio::test]
    #[ignore = "requires WONDER_SSH_SPIKE_HOST with an existing pinned SSH host key"]
    async fn ssh_spike_real_remote_cli_streams_one_empty_thread() {
        let alias = std::env::var("WONDER_SSH_SPIKE_HOST").expect("set WONDER_SSH_SPIKE_HOST");
        let report = timeout(Duration::from_secs(55), probe(&alias))
            .await
            .expect("SSH spike exceeded its outer deadline")
            .expect("SSH spike failed; fixed error class above contains no remote output");
        assert!(report.thread_started && report.correlated_event && report.remote_cleaned);
        println!("{}", serde_json::to_string(&report).unwrap());
    }
}
