//! The relay route: one Cloudflare Tunnel per host, run by `cloudflared` and
//! provisioned by Wonder's Worker (`services/push`, `/v1/relay/*`).
//!
//! The host never signs in. A signed-in phone hands it a one-time grant; the
//! host redeems it with a host key signature and gets a tunnel token, which it
//! writes 0600 and passes to `cloudflared` by file, never on the command line.
//! Later connects need only the signature. When `cloudflared` exits (the tunnel
//! was reaped while idle, the token was rejected, the network dropped) the
//! supervisor connects again: the Worker recreates the tunnel at the same
//! address and returns a fresh token. `410 relay_gone` turns the relay off.
//!
//! State goes to `relay.json` in the data directory for `wonder relay status`
//! and the daemon; the token goes to `relay-token`. The tunnel ends at the
//! loopback listener. Cloudflare's edge ends TLS, so the relay address carries
//! only sealed RPC (`rpc::sealed`, end to end with the key the phone pinned at
//! pairing): [`refuses`] answers 404 to every other path there, and the
//! Worker's ingress rule forwards no other path (crates/wonderd/RELAY.md).

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;
use wonder_api::pairing_protocol::HostKey;

use crate::logging::JsonlLogger;

pub const STATUS_FILE: &str = "relay.json";
pub const TOKEN_FILE: &str = "relay-token";

/// After `410 relay_gone`: the account signed out, was deleted or disabled this host.
pub const OFF_MESSAGE: &str = "Relay is off for this computer. \
     Turn on Reach from anywhere on your iPhone to use it again.";
pub const FULL_MESSAGE: &str = "The relay is full right now. Cupola retries every 10 minutes. \
     Tailscale and the home network still work.";
pub const RETRYING_MESSAGE: &str =
    "Can't reach Cupola's relay service right now. Cupola keeps trying.";
pub const RECONNECTING_MESSAGE: &str = "Reconnecting to the relay.";

/// What `cloudflared` prints once the edge accepts a connection.
const REGISTERED_LINE: &str = "Registered tunnel connection";
const FULL_RETRY: Duration = Duration::from_secs(600);
const TEMPORARY_RETRY: Duration = Duration::from_secs(60);

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum RelayState {
    #[default]
    Off,
    Starting,
    Ready,
    /// On, but not reachable right now; `detail` says why.
    Unavailable,
    /// On, but this computer's tunnel is not running because Wonder stopped;
    /// it reconnects at the same address when Wonder starts.
    Stopped,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayStatus {
    pub enabled: bool,
    pub state: RelayState,
    /// The Worker's label for this host; with the host key it is all a reconnect needs.
    pub label: Option<String>,
    /// `https://r-<label>.<zone>`, unchanged across tunnel recreation.
    pub origin: Option<String>,
    pub detail: Option<String>,
}

pub fn read_status(data_dir: &Path) -> RelayStatus {
    std::fs::read(data_dir.join(STATUS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Replaces `name` in `data_dir` atomically with a 0600 file.
fn write_private(data_dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    // NamedTempFile is created 0600 on Unix; rename keeps the mode.
    let mut file = tempfile::NamedTempFile::new_in(data_dir)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    let path = data_dir.join(name);
    file.persist(&path).map_err(|error| error.error)?;
    Ok(path)
}

fn write_status(data_dir: &Path, status: &RelayStatus) -> io::Result<()> {
    let bytes = serde_json::to_vec(status).map_err(io::Error::other)?;
    write_private(data_dir, STATUS_FILE, &bytes).map(|_| ())
}

/// The relay origin, when the relay is on. Every request through the auth
/// middleware asks, so the parsed file is kept until it changes on disk.
pub fn relay_origin(data_dir: &Path) -> Option<String> {
    type Cached = (PathBuf, Option<FileStamp>, Option<String>);
    static CACHE: std::sync::Mutex<Vec<Cached>> = std::sync::Mutex::new(Vec::new());
    let path = data_dir.join(STATUS_FILE);
    let stamp = FileStamp::of(&path);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, _, origin)) = cache
        .iter()
        .find(|(cached, cached_stamp, _)| *cached == path && *cached_stamp == stamp)
    {
        return origin.clone();
    }
    let status = read_status(data_dir);
    let origin = status.origin.filter(|_| status.enabled);
    cache.retain(|(cached, _, _)| *cached != path);
    // Tests use many data directories; a daemon uses one.
    if cache.len() >= 64 {
        cache.remove(0);
    }
    cache.push((path, stamp, origin.clone()));
    origin
}

/// Identifies one version of a file: replaced atomically, it gets a new inode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
        })
    }
}

/// The relay address's host, when the relay is on.
pub fn relay_host(data_dir: &Path) -> Option<String> {
    relay_origin(data_dir)?
        .strip_prefix("https://")
        .map(authority_host)
}

/// The host part of an HTTP authority, as DNS compares it: lowercase, without
/// a port or a trailing dot. `R-X.getcupola.app.:443` is `r-x.getcupola.app`.
pub(crate) fn authority_host(authority: &str) -> String {
    let authority = authority.trim();
    let host = if authority.starts_with('[') {
        authority.split_inclusive(']').next().unwrap_or(authority)
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Whether a request that arrived for `host` (a `Host` header or URI
/// authority) must be refused because it came over the relay. The relay's
/// edge can read plaintext, so the relay address serves only sealed RPC
/// (`/api/v1/rpc/sealed`, see `rpc::sealed`): pairing, events, artifacts and
/// every other route stay on Tailscale, the home network and loopback, and
/// reach the phone over the relay only inside the sealed channel.
pub fn refuses(relay_host: Option<&str>, host: Option<&str>, path: &str) -> bool {
    let (Some(relay_host), Some(host)) = (relay_host, host) else {
        return false;
    };
    authority_host(host) == authority_host(relay_host) && relay_refused_path(path)
}

/// [`refuses`] for this host's relay address, checking every authority the
/// request names.
pub fn refuses_on_relay(data_dir: &Path, hosts: &[Option<&str>], path: &str) -> bool {
    if !relay_refused_path(path) || hosts.iter().all(Option::is_none) {
        return false;
    }
    let relay = relay_host(data_dir);
    hosts
        .iter()
        .any(|host| refuses(relay.as_deref(), *host, path))
}

/// The one path the relay address serves; the Worker's tunnel ingress names
/// the same path (`services/push/src/cloudflare.ts`).
pub const SEALED_PATH: &str = "/api/v1/rpc/sealed";

fn relay_refused_path(path: &str) -> bool {
    path != SEALED_PATH
}

/// The host key as the Worker expects it: the raw X9.63 point, base64url (87 characters).
pub fn worker_host_key(key: &HostKey) -> String {
    let spki = key.public_key_spki();
    URL_SAFE_NO_PAD.encode(&spki[spki.len() - 65..])
}

/// `{ts, nonce, sig}` over `<action>|<subject>|<ts>|<nonce>`, raw `r || s`.
fn host_signature(key: &HostKey, action: &str, subject: &str) -> io::Result<serde_json::Value> {
    let rng = SystemRandom::new();
    let spki = key.public_key_spki();
    let pair = EcdsaKeyPair::from_private_key_and_public_key(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        &key.to_secret_bytes(),
        &spki[spki.len() - 65..],
        &rng,
    )
    .map_err(|_| io::Error::other("host key cannot sign"))?;
    let mut nonce = [0_u8; 32];
    rng.fill(&mut nonce)
        .map_err(|_| io::Error::other("no randomness"))?;
    let nonce = URL_SAFE_NO_PAD.encode(nonce);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let signature = pair
        .sign(&rng, format!("{action}|{subject}|{ts}|{nonce}").as_bytes())
        .map_err(|_| io::Error::other("host key cannot sign"))?;
    Ok(serde_json::json!({
        "ts": ts,
        "nonce": nonce,
        "sig": URL_SAFE_NO_PAD.encode(signature.as_ref()),
    }))
}

#[derive(Debug, Eq, PartialEq)]
pub enum Connected {
    Token {
        token: String,
        origin: String,
    },
    /// `410 relay_gone`: off until a phone hands over a new grant.
    Gone,
    /// `503 relay_full`: existing tunnels keep working; creation waits.
    Full(Duration),
    /// The Worker refused the grant (expired, used, another key). A lost reply
    /// may have redeemed it, so the next try goes without it.
    GrantRefused,
    Retry(Duration),
}

/// The Worker's relay endpoints. The base URL is the push Worker's.
#[derive(Clone)]
pub struct Worker {
    base: String,
    client: reqwest::Client,
}

impl Worker {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("relay HTTP client configuration"),
        }
    }

    pub async fn connect(&self, key: &HostKey, label: &str, grant: Option<&str>) -> Connected {
        let Ok(mut body) = host_signature(key, "relay-connect", label) else {
            return Connected::Retry(TEMPORARY_RETRY);
        };
        if let Some(grant) = grant {
            body["grant"] = grant.into();
        }
        let url = format!("{}/v1/relay/hosts/{label}/connect", self.base);
        let response = match self.client.post(url).json(&body).send().await {
            Ok(response) => response,
            Err(_) => return Connected::Retry(TEMPORARY_RETRY),
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok())
            .map(Duration::from_secs);
        let reply: serde_json::Value = response.json().await.unwrap_or_default();
        let field = |name: &str| reply.get(name).and_then(serde_json::Value::as_str);
        match (status, field("error")) {
            (200, _) => match (field("token"), field("origin")) {
                // The address becomes the origin this daemon trusts for the
                // relay, so only this label's own address is taken from a reply.
                (Some(token), Some(origin)) if !token.is_empty() && own_address(label, origin) => {
                    Connected::Token {
                        token: token.to_owned(),
                        origin: origin.to_owned(),
                    }
                }
                _ => Connected::Retry(TEMPORARY_RETRY),
            },
            (410, _) => Connected::Gone,
            (503, Some("relay_full")) => Connected::Full(retry_after.unwrap_or(FULL_RETRY)),
            // A replayed or concurrent proof is retried with a fresh one.
            (403, Some("replayed_proof")) | (503, Some("relay_busy")) => {
                Connected::Retry(Duration::from_secs(1))
            }
            (400..=499, _) if grant.is_some() => Connected::GrantRefused,
            _ => Connected::Retry(retry_after.unwrap_or(TEMPORARY_RETRY)),
        }
    }

    /// Deletes this host's tunnel and address. Already gone counts as done.
    pub async fn disable(&self, key: &HostKey, label: &str) -> Result<(), String> {
        let body = host_signature(key, "relay-disable", label).map_err(|e| e.to_string())?;
        let url = format!("{}/v1/relay/hosts/{label}/disable", self.base);
        let response = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|_| RETRYING_MESSAGE.to_owned())?;
        match response.status().as_u16() {
            200 | 404 | 410 => Ok(()),
            _ => Err(RETRYING_MESSAGE.to_owned()),
        }
    }
}

/// Restart pacing after `cloudflared` exits, and a cap on the Worker's
/// `Retry-After` (tests shrink both).
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub restart_min: Duration,
    pub restart_max: Duration,
    /// A run at least this long resets the restart delay.
    pub stable_after: Duration,
    pub retry_cap: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            restart_min: Duration::from_secs(1),
            restart_max: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
            retry_cap: Duration::from_secs(3600),
        }
    }
}

pub struct Supervisor {
    pub data_dir: PathBuf,
    pub cloudflared: Cloudflared,
    pub worker: Worker,
    pub host_key: HostKey,
    pub label: String,
    pub logger: Option<Arc<JsonlLogger>>,
    pub timing: Timing,
}

/// `cloudflared` arguments. The token stays in the 0600 file, out of `ps`.
pub fn cloudflared_args(token_file: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = ["tunnel", "--no-autoupdate", "run", "--token-file"]
        .into_iter()
        .map(Into::into)
        .collect();
    args.push(token_file.as_os_str().to_owned());
    args
}

impl Supervisor {
    fn log(&self, severity: &str, event: &str, fields: serde_json::Value) {
        if let Some(logger) = &self.logger {
            let _ = logger.record(severity, event, fields);
        }
    }

    fn persist(
        &self,
        state: RelayState,
        origin: Option<&str>,
        detail: Option<&str>,
    ) -> io::Result<()> {
        let status = RelayStatus {
            enabled: state != RelayState::Off,
            state,
            label: (state != RelayState::Off).then(|| self.label.clone()),
            origin: origin.map(str::to_owned),
            detail: detail.map(str::to_owned),
        };
        write_status(&self.data_dir, &status)
    }

    fn publish(&self, state: RelayState, origin: Option<&str>, detail: Option<&str>) {
        if let Err(error) = self.persist(state, origin, detail) {
            self.log(
                "warn",
                "relay_status_write_failed",
                serde_json::json!({ "error": error.to_string() }),
            );
        }
    }

    /// Runs the relay until it is turned off (`410`) or `stop` fires. `grant` is
    /// the phone's one-time grant when turning the relay on; a restart passes `None`.
    pub async fn run(self, grant: Option<String>, mut stop: watch::Receiver<bool>) -> RelayStatus {
        let mut grant = grant;
        let mut origin = read_status(&self.data_dir)
            .origin
            .filter(|_| grant.is_none());
        let mut delay = self.timing.restart_min;
        self.publish(RelayState::Starting, origin.as_deref(), None);
        loop {
            if *stop.borrow() {
                break;
            }
            let outcome = tokio::select! {
                outcome = self.worker.connect(&self.host_key, &self.label, grant.as_deref()) => outcome,
                _ = stop.changed() => break,
            };
            let wait = match outcome {
                Connected::Token {
                    token,
                    origin: granted,
                } => {
                    grant = None;
                    origin = Some(granted);
                    // Looked up per start, so installing it and running
                    // `cupola relay on` again is enough. The address stays
                    // recorded, and a retry needs no new grant.
                    let Some(binary) = self.cloudflared.find() else {
                        self.publish(
                            RelayState::Unavailable,
                            origin.as_deref(),
                            Some(NO_CLOUDFLARED_MESSAGE),
                        );
                        self.log("warn", "relay_cloudflared_missing", serde_json::json!({}));
                        return read_status(&self.data_dir);
                    };
                    match self
                        .run_tunnel(&binary, &token, origin.as_deref(), &mut stop)
                        .await
                    {
                        Ok(Some(ran)) => {
                            delay = if ran >= self.timing.stable_after {
                                self.timing.restart_min
                            } else {
                                delay
                            };
                            let wait = delay;
                            delay = (delay * 2).min(self.timing.restart_max);
                            self.publish(
                                RelayState::Starting,
                                origin.as_deref(),
                                Some(RECONNECTING_MESSAGE),
                            );
                            wait
                        }
                        Ok(None) => break,
                        Err(error) => {
                            self.log(
                                "warn",
                                "relay_cloudflared_failed",
                                serde_json::json!({ "error": error.to_string() }),
                            );
                            self.publish(
                                RelayState::Unavailable,
                                origin.as_deref(),
                                Some(&format!("The relay could not start: {error}")),
                            );
                            self.timing.restart_max
                        }
                    }
                }
                Connected::Gone => {
                    let _ = std::fs::remove_file(self.data_dir.join(TOKEN_FILE));
                    self.publish(RelayState::Off, None, Some(OFF_MESSAGE));
                    self.log("info", "relay_gone", serde_json::json!({}));
                    return read_status(&self.data_dir);
                }
                Connected::Full(after) => {
                    self.publish(
                        RelayState::Unavailable,
                        origin.as_deref(),
                        Some(FULL_MESSAGE),
                    );
                    after
                }
                Connected::GrantRefused => {
                    grant = None;
                    Duration::ZERO
                }
                Connected::Retry(after) => {
                    self.publish(
                        RelayState::Unavailable,
                        origin.as_deref(),
                        Some(RETRYING_MESSAGE),
                    );
                    after
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(wait.min(self.timing.retry_cap)) => {}
                _ = stop.changed() => break,
            }
        }
        let _ = std::fs::remove_file(self.data_dir.join(TOKEN_FILE));
        self.publish(RelayState::Stopped, origin.as_deref(), None);
        read_status(&self.data_dir)
    }

    /// Records the address, writes the token and runs `cloudflared` until it
    /// exits (`Some(run time)`) or `stop` fires (`None`, after killing it).
    /// The address is on disk before the tunnel can carry a request, so the
    /// pairing-page gate knows it from the first one.
    async fn run_tunnel(
        &self,
        cloudflared: &Path,
        token: &str,
        origin: Option<&str>,
        stop: &mut watch::Receiver<bool>,
    ) -> io::Result<Option<Duration>> {
        self.persist(RelayState::Starting, origin, None)?;
        let token_file = write_private(&self.data_dir, TOKEN_FILE, token.as_bytes())?;
        let mut child = Command::new(cloudflared)
            .args(cloudflared_args(&token_file))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let started = Instant::now();
        self.log("info", "relay_cloudflared_started", serde_json::json!({}));
        let mut lines = child
            .stderr
            .take()
            .map(|stderr| BufReader::new(stderr).lines());
        let mut ready = false;
        let ran = loop {
            tokio::select! {
                line = async {
                    match lines.as_mut() {
                        Some(lines) => lines.next_line().await,
                        None => std::future::pending().await,
                    }
                } => match line {
                    Ok(Some(line)) => {
                        if !ready && line.contains(REGISTERED_LINE) {
                            ready = true;
                            self.publish(RelayState::Ready, origin, None);
                        }
                    }
                    _ => lines = None,
                },
                status = child.wait() => {
                    let status = status?;
                    self.log(
                        "warn",
                        "relay_cloudflared_exited",
                        serde_json::json!({ "code": status.code() }),
                    );
                    break Some(started.elapsed());
                }
                _ = stop.changed() => {
                    let _ = child.kill().await;
                    break None;
                }
            }
        };
        Ok(ran)
    }
}

/// The owner's choice, made on this computer (`cupola relay on|off`): while
/// it is off the daemon runs no tunnel and a phone cannot turn the relay on.
pub const SETTINGS_FILE: &str = "relay-settings.json";

/// Shown while the owner allowed the relay but no phone has handed over a grant.
pub const WAITING_MESSAGE: &str = "Turn on Reach from anywhere on your iPhone to finish.";
/// A phone tried to turn the relay on before the owner allowed it here.
pub const NOT_ALLOWED_MESSAGE: &str =
    "The relay is off on this computer. Run `cupola relay on` there first.";
pub const NO_CLOUDFLARED_MESSAGE: &str = "The relay needs Cloudflare's cloudflared on this \
     computer. Install it (for example `brew install cloudflared`), then run `cupola relay on`.";
/// The relay is off here, but the relay service did not confirm releasing the address.
pub const RELEASE_PENDING_MESSAGE: &str = "The relay is off on this computer. Cupola's relay \
     service could not be reached to release its address; it is removed after 10 minutes unused.";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelaySettings {
    pub allowed: bool,
}

pub fn read_settings(data_dir: &Path) -> RelaySettings {
    std::fs::read(data_dir.join(SETTINGS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn write_settings(data_dir: &Path, settings: RelaySettings) -> io::Result<()> {
    let bytes = serde_json::to_vec(&settings).map_err(io::Error::other)?;
    write_private(data_dir, SETTINGS_FILE, &bytes).map(|_| ())
}

/// What `relay.status` reports: the owner's choice and the tunnel's state.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RelayView {
    pub allowed: bool,
    pub enabled: bool,
    pub state: RelayState,
    pub origin: Option<String>,
    pub detail: Option<String>,
}

pub fn view(data_dir: &Path) -> RelayView {
    let allowed = read_settings(data_dir).allowed;
    let status = read_status(data_dir);
    let detail = match (&status.detail, allowed, status.enabled) {
        (Some(detail), _, _) => Some(detail.clone()),
        (None, true, false) => Some(WAITING_MESSAGE.to_owned()),
        (None, _, _) => None,
    };
    RelayView {
        allowed,
        enabled: status.enabled,
        state: status.state,
        origin: status.origin.filter(|_| status.enabled),
        detail,
    }
}

/// A Worker label: 26 base32 characters, as `services/push` issues them.
pub fn valid_label(label: &str) -> bool {
    label.len() == 26
        && label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
}

/// Whether `origin` is the relay address the Worker issues for `label`:
/// `https://r-<label>.<zone>` with a plain DNS zone, no port, path or userinfo.
pub fn own_address(label: &str, origin: &str) -> bool {
    let Some(host) = origin.strip_prefix("https://") else {
        return false;
    };
    let Some(zone) = host
        .strip_prefix("r-")
        .and_then(|rest| rest.strip_prefix(label))
        .and_then(|rest| rest.strip_prefix('.'))
    else {
        return false;
    };
    valid_label(label)
        && zone.contains('.')
        && zone.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

/// A one-time grant: 32 random bytes, base64url (the Worker's `secret`).
pub fn valid_grant(grant: &str) -> bool {
    grant.len() == 43
        && grant
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Where to look for `cloudflared`: one named path, or a list of directories.
#[derive(Clone, Debug, Default)]
pub struct Cloudflared {
    named: Option<PathBuf>,
    dirs: Vec<PathBuf>,
}

impl Cloudflared {
    /// Exactly this path (`WONDER_CLOUDFLARED`).
    pub fn at(path: PathBuf) -> Self {
        Self {
            named: Some(path),
            dirs: Vec::new(),
        }
    }

    /// The first `cloudflared` in these directories.
    pub fn search(dirs: Vec<PathBuf>) -> Self {
        Self { named: None, dirs }
    }

    /// The binary as it is now on disk.
    pub fn find(&self) -> Option<PathBuf> {
        match &self.named {
            Some(path) => Some(path.clone()).filter(|path| path.is_file()),
            None => self
                .dirs
                .iter()
                .map(|dir| dir.join("cloudflared"))
                .find(|candidate| candidate.is_file()),
        }
    }
}

/// Where the daemon finds `cloudflared` and the Worker.
#[derive(Clone, Debug)]
pub struct Environment {
    pub cloudflared: Cloudflared,
    pub worker: String,
    pub timing: Timing,
}

impl Environment {
    /// No `cloudflared` and no Worker: for hosts in tests.
    pub fn disabled() -> Self {
        Self {
            cloudflared: Cloudflared::default(),
            worker: String::new(),
            timing: Timing::default(),
        }
    }

    /// `WONDER_CLOUDFLARED`, else `cloudflared` on `PATH` or where Homebrew
    /// and Linux packages install it (a login service has a short `PATH`);
    /// the Worker is the push Worker (`WONDER_RELAY_ENDPOINT` overrides it).
    pub fn from_env() -> Self {
        let cloudflared = match std::env::var_os("WONDER_CLOUDFLARED") {
            Some(path) => Cloudflared::at(PathBuf::from(path)),
            None => {
                let path = std::env::var_os("PATH").unwrap_or_default();
                Cloudflared::search(
                    std::env::split_paths(&path)
                        .chain(
                            ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
                                .into_iter()
                                .map(PathBuf::from),
                        )
                        .collect(),
                )
            }
        };
        Self {
            cloudflared,
            worker: std::env::var("WONDER_RELAY_ENDPOINT")
                .ok()
                .or_else(crate::push::endpoint)
                .unwrap_or_default(),
            timing: Timing::default(),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum EnableError {
    /// The owner has not allowed the relay on this computer.
    NotAllowed,
    Invalid,
}

struct Running {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<RelayStatus>,
}

/// The daemon's relay: at most one [`Supervisor`], started and stopped here
/// only, in the order calls arrive.
#[derive(Clone)]
pub struct Manager {
    data_dir: PathBuf,
    environment: Arc<Environment>,
    logger: Option<Arc<JsonlLogger>>,
    running: Arc<tokio::sync::Mutex<Option<Running>>>,
}

impl Manager {
    pub fn new(
        data_dir: PathBuf,
        environment: Environment,
        logger: Option<Arc<JsonlLogger>>,
    ) -> Self {
        Self {
            data_dir,
            environment: Arc::new(environment),
            logger,
            running: Arc::default(),
        }
    }

    /// At daemon start: reconnect when the owner allows the relay and this
    /// computer already has an address (no grant needed).
    pub async fn resume(&self) {
        let status = read_status(&self.data_dir);
        if !read_settings(&self.data_dir).allowed || !status.enabled {
            return;
        }
        if let Some(label) = status.label.filter(|label| valid_label(label)) {
            let mut running = self.running.lock().await;
            self.start(&mut running, label, None).await;
        }
    }

    /// The owner's choice. Turning it off stops the tunnel and releases the
    /// address at the Worker; `Err` carries what the owner should know when
    /// the release could not be confirmed (the relay is off here either way).
    pub async fn set_allowed(&self, allowed: bool) -> Result<RelayView, String> {
        let mut running = self.running.lock().await;
        write_settings(&self.data_dir, RelaySettings { allowed }).map_err(|e| e.to_string())?;
        if allowed {
            // Also the retry after a stop the owner can fix, such as a
            // missing `cloudflared`.
            if running.as_ref().is_none_or(|run| run.task.is_finished()) {
                drop(running);
                self.resume().await;
            }
            return Ok(view(&self.data_dir));
        }
        stop(&mut running).await;
        let status = read_status(&self.data_dir);
        let _ = std::fs::remove_file(self.data_dir.join(TOKEN_FILE));
        let mut outcome = Ok(());
        if let Some(label) = status.label.as_deref().filter(|label| valid_label(label)) {
            let released = match crate::lan::host_key(&self.data_dir).await {
                Ok(key) => {
                    Worker::new(&self.environment.worker)
                        .disable(&key, label)
                        .await
                }
                Err(error) => Err(error.to_string()),
            };
            if released.is_err() {
                outcome = Err(RELEASE_PENDING_MESSAGE.to_owned());
            }
        }
        write_status(&self.data_dir, &RelayStatus::default()).map_err(|e| e.to_string())?;
        outcome.map(|()| view(&self.data_dir))
    }

    /// A phone's one-time grant for `label`: (re)starts the tunnel with it.
    pub async fn enable(&self, grant: &str, label: &str) -> Result<RelayView, EnableError> {
        if !valid_grant(grant) || !valid_label(label) {
            return Err(EnableError::Invalid);
        }
        let mut running = self.running.lock().await;
        if !read_settings(&self.data_dir).allowed {
            return Err(EnableError::NotAllowed);
        }
        stop(&mut running).await;
        self.start(&mut running, label.to_owned(), Some(grant.to_owned()))
            .await;
        Ok(view(&self.data_dir))
    }

    /// Stops the tunnel, keeping the relay on: it reconnects at the same
    /// address when the daemon starts again.
    pub async fn shutdown(&self) {
        stop(&mut *self.running.lock().await).await;
    }

    async fn start(&self, running: &mut Option<Running>, label: String, grant: Option<String>) {
        if running.as_ref().is_some_and(|run| !run.task.is_finished()) {
            return;
        }
        let host_key = match crate::lan::host_key(&self.data_dir).await {
            Ok(key) => key,
            Err(error) => {
                if let Some(logger) = &self.logger {
                    let _ = logger.record(
                        "warn",
                        "relay_host_key_unavailable",
                        serde_json::json!({ "error": error.to_string() }),
                    );
                }
                return;
            }
        };
        let supervisor = Supervisor {
            data_dir: self.data_dir.clone(),
            cloudflared: self.environment.cloudflared.clone(),
            worker: Worker::new(&self.environment.worker),
            host_key,
            label,
            logger: self.logger.clone(),
            timing: self.environment.timing,
        };
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(supervisor.run(grant, stopped));
        *running = Some(Running { stop, task });
    }
}

async fn stop(running: &mut Option<Running>) {
    if let Some(run) = running.take() {
        let _ = run.stop.send(true);
        let _ = run.task.await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::Json;
    use p256::ecdsa::signature::Verifier;
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    type Reply = (u16, Option<u64>, serde_json::Value);

    #[derive(Default)]
    struct Stub {
        replies: Mutex<VecDeque<Reply>>,
        requests: Mutex<Vec<(String, serde_json::Value)>>,
    }

    /// A stand-in for the Worker: answers scripted replies in order and records
    /// each request body.
    async fn stub_worker(replies: Vec<Reply>) -> (String, Arc<Stub>) {
        let stub = Arc::new(Stub {
            replies: Mutex::new(replies.into()),
            ..Default::default()
        });
        let app = axum::Router::new()
            .route(
                "/v1/relay/hosts/{label}/{action}",
                axum::routing::post(
                    |State(stub): State<Arc<Stub>>,
                     axum::extract::Path((label, action)): axum::extract::Path<(
                        String,
                        String,
                    )>,
                     Json(body): Json<serde_json::Value>| async move {
                        stub.requests
                            .lock()
                            .unwrap()
                            .push((format!("{action} {label}"), body));
                        let (status, retry_after, reply) = stub
                            .replies
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or((503, Some(0), serde_json::json!({"error":"exhausted"})));
                        let mut response =
                            (StatusCode::from_u16(status).unwrap(), Json(reply)).into_response();
                        if let Some(after) = retry_after {
                            response
                                .headers_mut()
                                .insert("retry-after", after.to_string().parse().unwrap());
                        }
                        response
                    },
                ),
            )
            .with_state(stub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), stub)
    }

    /// A fake `cloudflared`: records its argv, the token file's mode and
    /// contents, announces a registered connection, then exits 1 after `run_ms`
    /// or, with `None`, runs until killed. Portable to macOS and Linux shells.
    fn fake_cloudflared(dir: &Path, run_ms: Option<u32>) -> (PathBuf, PathBuf) {
        let record = dir.join("cloudflared-runs.log");
        let binary = dir.join("cloudflared");
        let script = format!(
            "#!/bin/sh\n\
             token_file=\"$5\"\n\
             if [ -n \"$(find \"$token_file\" -perm 600)\" ]; then mode=600; \
             else mode=$(ls -l \"$token_file\" | cut -c1-10); fi\n\
             printf 'argv=%s|%s|%s|%s|%s mode=%s token=%s\\n' \"$1\" \"$2\" \"$3\" \"$4\" \"$5\" \
             \"$mode\" \"$(cat \"$token_file\")\" >> '{}'\n\
             echo 'INF Registered tunnel connection connIndex=0 protocol=quic' >&2\n\
             {}\n",
            record.display(),
            match run_ms {
                Some(ms) => format!("sleep {}\nexit 1", f64::from(ms) / 1000.0),
                None => "exec sleep 600".into(),
            }
        );
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        (binary, record)
    }

    fn fast() -> Timing {
        Timing {
            restart_min: Duration::from_millis(20),
            restart_max: Duration::from_millis(80),
            stable_after: Duration::from_secs(60),
            retry_cap: Duration::from_millis(50),
        }
    }

    fn supervisor(dir: &Path, cloudflared: PathBuf, base: &str, key: HostKey) -> Supervisor {
        Supervisor {
            data_dir: dir.to_path_buf(),
            cloudflared: Cloudflared::at(cloudflared),
            worker: Worker::new(base),
            host_key: key,
            label: "k7xq2abcdefghijklmnopqrstu".into(),
            logger: None,
            timing: fast(),
        }
    }

    fn token(value: &str) -> Reply {
        (
            200,
            None,
            serde_json::json!({
                "token": value,
                "origin": "https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app",
                "label": "k7xq2abcdefghijklmnopqrstu",
            }),
        )
    }

    fn verify(key: &HostKey, action: &str, label: &str, body: &serde_json::Value) {
        let point = URL_SAFE_NO_PAD.decode(worker_host_key(key)).unwrap();
        let verifying = p256::ecdsa::VerifyingKey::from_sec1_bytes(&point).unwrap();
        let transcript = format!(
            "{action}|{label}|{}|{}",
            body["ts"].as_u64().unwrap(),
            body["nonce"].as_str().unwrap()
        );
        let sig = URL_SAFE_NO_PAD
            .decode(body["sig"].as_str().unwrap())
            .unwrap();
        let sig = p256::ecdsa::Signature::from_slice(&sig).unwrap();
        verifying.verify(transcript.as_bytes(), &sig).unwrap();
        assert_eq!(body["nonce"].as_str().unwrap().len(), 43);
    }

    /// The D4 check: the argv and token-file mode `cloudflared` sees, a restart
    /// with a fresh token after it exits, and a 410 turning the relay off with
    /// the stated message.
    #[tokio::test]
    async fn supervises_cloudflared_restarts_after_exit_and_turns_off_on_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (base, stub) = stub_worker(vec![
            token("token-one"),
            token("token-two"),
            (
                410,
                None,
                serde_json::json!({"error":"relay_gone","message":"Remote access is off."}),
            ),
        ])
        .await;
        let (cloudflared, record) = fake_cloudflared(dir.path(), Some(100));
        let key = HostKey::generate();
        let relay = supervisor(dir.path(), cloudflared, &base, key);
        let key = HostKey::from_secret_bytes(&relay.host_key.to_secret_bytes()).unwrap();
        let (_stop_tx, stop) = watch::channel(false);
        let finished = tokio::time::timeout(
            Duration::from_secs(10),
            relay.run(Some("grant-secret".into()), stop),
        )
        .await
        .expect("the supervisor stops on 410");

        let token_file = dir.path().join(TOKEN_FILE);
        let runs = std::fs::read_to_string(&record).unwrap();
        let expected_argv = format!(
            "tunnel|--no-autoupdate|run|--token-file|{}",
            token_file.display()
        );
        assert_eq!(
            runs.lines().collect::<Vec<_>>(),
            [
                format!("argv={expected_argv} mode=600 token=token-one"),
                format!("argv={expected_argv} mode=600 token=token-two"),
            ],
            "cloudflared runs once per token, restarted after exit"
        );

        let requests = stub.requests.lock().unwrap().clone();
        let label = "k7xq2abcdefghijklmnopqrstu";
        assert_eq!(requests.len(), 3);
        for (index, (path, body)) in requests.iter().enumerate() {
            assert_eq!(path, &format!("connect {label}"));
            verify(&key, "relay-connect", label, body);
            assert_eq!(
                body.get("grant").and_then(serde_json::Value::as_str),
                (index == 0).then_some("grant-secret"),
                "only the first connect redeems the grant"
            );
        }

        assert_eq!(
            finished,
            RelayStatus {
                enabled: false,
                state: RelayState::Off,
                label: None,
                origin: None,
                detail: Some(OFF_MESSAGE.into()),
            }
        );
        assert_eq!(read_status(dir.path()), finished);
        assert!(!token_file.exists(), "the token is deleted with the relay");
        assert_eq!(
            std::fs::metadata(dir.path().join(STATUS_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn waits_out_a_full_relay_and_drops_a_refused_grant() {
        let dir = tempfile::tempdir().unwrap();
        let (base, stub) = stub_worker(vec![
            (403, None, serde_json::json!({"error":"invalid_grant"})),
            (503, Some(600), serde_json::json!({"error":"relay_full"})),
            token("token-one"),
        ])
        .await;
        let (cloudflared, record) = fake_cloudflared(dir.path(), None);
        let relay = supervisor(dir.path(), cloudflared, &base, HostKey::generate());
        let (stop_tx, stop) = watch::channel(false);
        let run = tokio::spawn(relay.run(Some("grant-secret".into()), stop));

        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while read_status(dir.path()).state != RelayState::Ready {
            assert!(Instant::now() < deadline, "never ready: {seen:?}");
            let status = read_status(dir.path());
            if seen.last() != Some(&status.detail) {
                seen.push(status.detail);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(
            seen.contains(&Some(FULL_MESSAGE.into())),
            "the full relay is reported: {seen:?}"
        );
        let ready = read_status(dir.path());
        assert!(ready.enabled);
        assert_eq!(
            ready.origin.as_deref(),
            Some("https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app")
        );
        let grants: Vec<Option<String>> = stub
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(_, body)| body.get("grant").and_then(|g| g.as_str()).map(Into::into))
            .collect();
        assert_eq!(grants, [Some("grant-secret".into()), None, None]);

        stop_tx.send(true).unwrap();
        let stopped = tokio::time::timeout(Duration::from_secs(60), run)
            .await
            .expect("stop ends the supervisor")
            .unwrap();
        assert_ne!(stopped.state, RelayState::Ready, "the tunnel is gone");
        assert!(stopped.enabled, "stopping Cupola keeps the relay turned on");
        assert_eq!(stopped.origin, ready.origin, "and its address");
        assert_eq!(read_status(dir.path()), stopped);
        assert_eq!(stopped.state, RelayState::Stopped);
        assert_eq!(stopped.detail, None);
        assert!(!dir.path().join(TOKEN_FILE).exists());
        assert_eq!(std::fs::read_to_string(record).unwrap().lines().count(), 1);
    }

    /// HUNT-RELAY finding 2: the Worker's reply names the address the daemon
    /// then trusts as the relay origin (and, through `refuses`, the host whose
    /// every other path it answers 404 to). Only this label's own relay address
    /// is accepted: another host, a port, a path or plain HTTP is a bad reply,
    /// retried without starting `cloudflared`.
    #[tokio::test]
    async fn an_origin_other_than_this_labels_own_address_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let bad = [
            "https://evil.example",
            "https://tailnet-host.tail1234.ts.net",
            "https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app:8443",
            "https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app/api",
            "http://r-k7xq2abcdefghijklmnopqrstu.getcupola.app",
            "https://r-zzzzzzzzzzzzzzzzzzzzzzzzzz.getcupola.app",
            "https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app@evil.example",
            "https://r-k7xq2abcdefghijklmnopqrstu.",
            "https://localhost",
        ];
        let mut replies: Vec<Reply> = bad
            .iter()
            .map(|origin| {
                (
                    200,
                    None,
                    serde_json::json!({"token": "bad-token", "origin": origin}),
                )
            })
            .collect();
        replies.push(token("good-token"));
        let (base, _stub) = stub_worker(replies).await;
        let (cloudflared, record) = fake_cloudflared(dir.path(), None);
        let relay = supervisor(dir.path(), cloudflared, &base, HostKey::generate());
        let (stop_tx, stop) = watch::channel(false);
        let run = tokio::spawn(relay.run(Some("grant-secret".into()), stop));
        let deadline = Instant::now() + Duration::from_secs(10);
        while read_status(dir.path()).state != RelayState::Ready {
            assert!(Instant::now() < deadline, "never ready");
            let origin = read_status(dir.path()).origin;
            assert!(
                origin.is_none()
                    || origin.as_deref()
                        == Some("https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app"),
                "adopted {origin:?}"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        stop_tx.send(true).unwrap();
        run.await.unwrap();
        let runs = std::fs::read_to_string(record).unwrap();
        assert_eq!(runs.lines().count(), 1, "{runs}");
        assert!(runs.contains("token=good-token"), "{runs}");
    }

    const LABEL: &str = "k7xq2abcdefghijklmnopqrstu";
    const GRANT: &str = "grant-secret-grant-secret-grant-secret-0123";

    fn manager(dir: &Path, cloudflared: PathBuf, base: &str) -> Manager {
        Manager::new(
            dir.to_path_buf(),
            Environment {
                cloudflared: Cloudflared::at(cloudflared),
                worker: base.to_owned(),
                timing: fast(),
            },
            None,
        )
    }

    async fn until_ready(dir: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while read_status(dir).state != RelayState::Ready {
            assert!(
                Instant::now() < deadline,
                "never ready: {:?}",
                read_status(dir)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The owner's setting gates the tunnel: a grant is refused (and nothing
    /// reaches the Worker or starts `cloudflared`) until the owner allows the
    /// relay; then it starts, the daemon trusts the relay origin, and turning
    /// it off stops `cloudflared`, releases the address with a host-signed
    /// disable and forgets the token.
    #[tokio::test]
    async fn the_owners_setting_gates_the_tunnel_and_off_releases_the_address() {
        let dir = tempfile::tempdir().unwrap();
        let (base, stub) = stub_worker(vec![
            token("token-one"),
            (200, None, serde_json::json!({"disabled": true})),
        ])
        .await;
        let (cloudflared, record) = fake_cloudflared(dir.path(), None);
        let relay = manager(dir.path(), cloudflared, &base);

        assert_eq!(
            relay.enable(GRANT, LABEL).await,
            Err(EnableError::NotAllowed)
        );
        assert_eq!(
            relay.enable("short", LABEL).await,
            Err(EnableError::Invalid)
        );
        assert_eq!(
            relay.enable(GRANT, "UPPER").await,
            Err(EnableError::Invalid)
        );
        assert!(stub.requests.lock().unwrap().is_empty());
        assert!(!record.exists(), "cloudflared never started");
        assert_eq!(relay_origin(dir.path()), None);

        let waiting = relay.set_allowed(true).await.unwrap();
        assert!(waiting.allowed && !waiting.enabled);
        assert_eq!(waiting.detail.as_deref(), Some(WAITING_MESSAGE));
        assert!(stub.requests.lock().unwrap().is_empty(), "no grant yet");

        relay.enable(GRANT, LABEL).await.unwrap();
        until_ready(dir.path()).await;
        let origin = "https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app";
        assert_eq!(relay_origin(dir.path()).as_deref(), Some(origin));
        assert_eq!(view(dir.path()).origin.as_deref(), Some(origin));

        let off = relay.set_allowed(false).await.unwrap();
        assert_eq!(
            off,
            RelayView {
                allowed: false,
                enabled: false,
                state: RelayState::Off,
                origin: None,
                detail: None,
            }
        );
        assert_eq!(relay_origin(dir.path()), None, "no longer trusted");
        assert!(!dir.path().join(TOKEN_FILE).exists());
        let requests = stub.requests.lock().unwrap().clone();
        assert_eq!(
            requests
                .iter()
                .map(|(path, _)| path.as_str())
                .collect::<Vec<_>>(),
            [format!("connect {LABEL}"), format!("disable {LABEL}")]
        );
        assert_eq!(requests[0].1["grant"], GRANT);
        let key = crate::lan::load_or_create_host_key(dir.path()).unwrap();
        verify(&key, "relay-disable", LABEL, &requests[1].1);
        assert_eq!(std::fs::read_to_string(&record).unwrap().lines().count(), 1);
        assert_eq!(
            relay.enable(GRANT, LABEL).await,
            Err(EnableError::NotAllowed)
        );
    }

    /// A restarted daemon reconnects at the same address without a grant,
    /// only while the owner allows the relay.
    #[tokio::test]
    async fn a_restarted_daemon_reconnects_without_a_grant_only_when_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let (base, stub) = stub_worker(vec![token("token-two")]).await;
        let (cloudflared, _record) = fake_cloudflared(dir.path(), None);
        write_status(
            dir.path(),
            &RelayStatus {
                enabled: true,
                state: RelayState::Stopped,
                label: Some(LABEL.into()),
                origin: Some("https://r-k7xq2abcdefghijklmnopqrstu.getcupola.app".into()),
                detail: None,
            },
        )
        .unwrap();
        let relay = manager(dir.path(), cloudflared, &base);
        relay.resume().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(stub.requests.lock().unwrap().is_empty(), "not allowed");

        write_settings(dir.path(), RelaySettings { allowed: true }).unwrap();
        relay.resume().await;
        until_ready(dir.path()).await;
        let requests = stub.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, format!("connect {LABEL}"));
        assert!(requests[0].1.get("grant").is_none());
        relay.shutdown().await;
        let stopped = read_status(dir.path());
        assert!(stopped.enabled, "a daemon stop keeps the relay on");
        assert_eq!(stopped.state, RelayState::Stopped);
    }

    /// Without `cloudflared` the relay says what to install and keeps its
    /// address; once it is installed, `cupola relay on` (the owner's setting
    /// again) finds it and starts the tunnel without a new grant.
    #[tokio::test]
    async fn installing_cloudflared_then_relay_on_starts_the_tunnel() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let (base, stub) = stub_worker(vec![token("token-one"), token("token-two")]).await;
        let relay = Manager::new(
            dir.path().to_path_buf(),
            Environment {
                cloudflared: Cloudflared::search(vec![bin.clone()]),
                worker: base,
                timing: fast(),
            },
            None,
        );
        relay.set_allowed(true).await.unwrap();
        relay.enable(GRANT, LABEL).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while read_status(dir.path()).state != RelayState::Unavailable {
            assert!(Instant::now() < deadline, "{:?}", read_status(dir.path()));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let missing = read_status(dir.path());
        assert_eq!(missing.detail.as_deref(), Some(NO_CLOUDFLARED_MESSAGE));
        assert!(missing.enabled, "the address is kept for the retry");
        assert_eq!(missing.label.as_deref(), Some(LABEL));
        assert_eq!(
            view(dir.path()).detail.as_deref(),
            Some(NO_CLOUDFLARED_MESSAGE)
        );

        let (_, record) = fake_cloudflared(&bin, None);
        relay.set_allowed(true).await.unwrap();
        until_ready(dir.path()).await;
        assert_eq!(std::fs::read_to_string(&record).unwrap().lines().count(), 1);
        let requests = stub.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].1["grant"], GRANT);
        assert!(requests[1].1.get("grant").is_none(), "no new grant needed");
        relay.shutdown().await;
    }

    /// A paired phone hands over the Worker's grant with its device
    /// signature: unsigned, tampered or replayed requests start nothing; the
    /// signed one starts the tunnel with that grant.
    #[tokio::test]
    async fn a_paired_phones_signed_grant_starts_the_tunnel() {
        use crate::{ActionTranscript, AuthenticatedDevice, OwnerAuthority};
        use axum::{body::Body, http::Request, routing::post, Extension, Router};
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        use p256::ecdsa::{signature::Signer, Signature, SigningKey};
        use tower::ServiceExt;
        let (_dir, mut state) = crate::ingestion::tests::fixture().await;
        let bin = tempfile::tempdir().unwrap();
        let (base, stub) = stub_worker(vec![token("token-one")]).await;
        let (cloudflared, record) = fake_cloudflared(bin.path(), None);
        state.relay = manager(&state.data_dir, cloudflared, &base);
        state.relay.set_allowed(true).await.unwrap();
        let key = SigningKey::from_slice(&[17u8; 32]).unwrap();
        let point = key.verifying_key().to_sec1_point(false);
        state.pairing.lock().await.restore_device(
            "phone".into(),
            wonder_api::pairing_protocol::DevicePublicKeyJwk {
                kty: "EC".into(),
                crv: "P-256".into(),
                x: URL_SAFE_NO_PAD.encode(point.x().unwrap()),
                y: URL_SAFE_NO_PAD.encode(point.y().unwrap()),
            },
            false,
            None,
        );
        let path = "/api/v1/relay/enable";
        let app = Router::new()
            .route(path, post(crate::relay_enable))
            .layer(Extension(AuthenticatedDevice {
                device_id: "phone".into(),
                session_binding: "phone-session".into(),
            }))
            .layer(Extension(OwnerAuthority))
            .with_state(state.clone());
        let issued = crate::now_ms();
        let transcript = ActionTranscript {
            action: "relay.enable",
            target: path,
            body_sha256: &crate::action_body_sha256(
                &serde_json::json!({"grant": GRANT, "label": LABEL}),
            ),
            action_nonce: "relay-proof",
            session_binding: "phone-session",
            device_id: "phone",
            host_installation_id: &state.host_installation_id,
            issued_at_ms: issued,
            expected_state: "allowed",
        };
        let signature: Signature = key.sign(&transcript.to_bytes());
        let signed = serde_json::json!({"grant": GRANT, "label": LABEL,
            "actionNonce": "relay-proof", "issuedAtMs": issued,
            "signature": URL_SAFE_NO_PAD.encode(signature.to_bytes())});
        let mut tampered = signed.clone();
        tampered["label"] = "abcdefghijklmnopqrstuvwxyz".into();
        for (body, expected) in [
            (
                serde_json::json!({"grant": GRANT, "label": LABEL}),
                StatusCode::UNAUTHORIZED,
            ),
            (tampered, StatusCode::UNAUTHORIZED),
            (signed.clone(), StatusCode::OK),
            (signed, StatusCode::UNAUTHORIZED),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{body}");
        }
        until_ready(&state.data_dir).await;
        let requests = stub.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            1,
            "only the signed grant reached the Worker"
        );
        assert_eq!(requests[0].0, format!("connect {LABEL}"));
        assert_eq!(requests[0].1["grant"], GRANT);
        assert_eq!(std::fs::read_to_string(&record).unwrap().lines().count(), 1);
        state.relay.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn disable_treats_an_already_gone_host_as_done() {
        let (base, stub) = stub_worker(vec![
            (410, None, serde_json::json!({"error":"relay_gone"})),
            (
                503,
                Some(60),
                serde_json::json!({"error":"temporarily_unavailable"}),
            ),
        ])
        .await;
        let key = HostKey::generate();
        let worker = Worker::new(&base);
        assert_eq!(worker.disable(&key, "label").await, Ok(()));
        assert_eq!(
            worker.disable(&key, "label").await,
            Err(RETRYING_MESSAGE.to_owned())
        );
        let requests = stub.requests.lock().unwrap().clone();
        verify(&key, "relay-disable", "label", &requests[0].1);
    }

    /// Property: every spelling of the relay address a client can send (case,
    /// trailing dot, a port) is recognised, and no other host is.
    #[test]
    fn every_spelling_of_the_relay_address_is_refused_and_no_other() {
        let mut fuzz = crate::test_support::Fuzz::new(0x4E1A7);
        let relay = "r-k7xq2.getcupola.app";
        for _ in 0..5_000 {
            let mut host: String = relay
                .chars()
                .map(|c| {
                    if fuzz.below(2) == 0 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect();
            if fuzz.below(2) == 0 {
                host.push('.');
            }
            if fuzz.below(2) == 0 {
                host.push_str(&format!(":{}", fuzz.below(65_536)));
            }
            assert!(refuses(Some(relay), Some(&host), "/pair"), "{host}");
            assert!(!refuses(Some(relay), Some(&host), "/api/v1/rpc/sealed"));
            // Anything else: a different label, or noise around the name.
            let other = format!("{}{host}{}", fuzz.text(4), fuzz.text(4));
            if authority_host(&other) != relay {
                assert!(!refuses(Some(relay), Some(&other), "/pair"), "{other}");
            }
        }
    }

    /// The relay address serves sealed RPC and nothing else: every route a
    /// phone could use in plaintext (pairing, events, artifacts, plain RPC) is
    /// refused there, and served as before on every other address.
    #[test]
    fn relay_address_serves_only_sealed_rpc() {
        let relay = Some("r-k7xq2.getcupola.app");
        let paths = [
            "/pair",
            "/readyz",
            "/healthz",
            "/api/v1/mcp",
            "/api/v1/rpc",
            "/api/v1/rpc/sealed/",
            "/api/v1/rpc/sealedx",
            "/api/v1/pairing/code",
            "/api/v1/pairing/claim",
            "/api/v1/pairing/session",
            "/api/v1/host/proof",
            "/api/v1/host/status",
            "/api/v1/events",
            "/api/v1/artifacts/a1/download",
            "/",
        ];
        for host in [
            "r-k7xq2.getcupola.app",
            "R-K7XQ2.getcupola.app",
            "r-k7xq2.getcupola.app:443",
            "r-k7xq2.getcupola.app.",
            "R-K7XQ2.GETCUPOLA.APP.:443",
        ] {
            assert!(!refuses(relay, Some(host), SEALED_PATH), "{host}");
            for path in paths {
                assert!(refuses(relay, Some(host), path), "{host}{path}");
            }
        }
        for host in [
            "wonder.example.ts.net",
            "127.0.0.1:3777",
            "[::1]:3777",
            "r-k7xq2.getcupola.app.evil.example",
        ] {
            for path in paths.into_iter().chain([SEALED_PATH]) {
                assert!(!refuses(relay, Some(host), path), "{host}{path}");
            }
        }
        assert!(!refuses(None, Some("r-k7xq2.getcupola.app"), "/pair"));
        assert!(!refuses(relay, None, "/pair"));
    }

    const RELAY_HOST: &str = "r-k7xq2abcdefghijklmnopqrstu.getcupola.app";

    /// `/pair` through the daemon's router, as `cloudflared` would deliver it
    /// with `host` as its `Host` header (or as an absolute URI with `None`).
    async fn pair_status(state: &crate::AppState, host: Option<&str>) -> StatusCode {
        use tower::ServiceExt;
        let request = match host {
            Some(host) => axum::http::Request::builder()
                .uri("/pair")
                .header("host", host),
            None => axum::http::Request::builder().uri(format!("https://{RELAY_HOST}/pair")),
        };
        crate::router(state.clone())
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// The auth middleware compares the relay address as DNS does: a port,
    /// case or trailing dot in `Host`, or an absolute request URI, still names it.
    #[tokio::test]
    async fn router_refuses_the_pairing_page_for_every_spelling_of_the_relay_address() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let status = RelayStatus {
            enabled: true,
            state: RelayState::Ready,
            label: Some("k7xq2abcdefghijklmnopqrstu".into()),
            origin: Some(format!("https://{RELAY_HOST}")),
            detail: None,
        };
        write_status(&state.data_dir, &status).unwrap();
        for host in [
            Some(RELAY_HOST.to_owned()),
            Some(format!("{RELAY_HOST}:443")),
            Some(RELAY_HOST.to_ascii_uppercase()),
            Some(format!("{RELAY_HOST}.")),
            Some(format!("{}.:443", RELAY_HOST.to_ascii_uppercase())),
            None,
        ] {
            assert_eq!(
                pair_status(&state, host.as_deref()).await,
                StatusCode::NOT_FOUND,
                "{host:?}"
            );
        }
        for host in ["127.0.0.1:3777", "wonder.example.ts.net", "getcupola.app"] {
            assert_eq!(
                pair_status(&state, Some(host)).await,
                StatusCode::OK,
                "{host}"
            );
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// The first request a new tunnel can carry already meets the gate: the
    /// address is recorded before `cloudflared` starts, not when it registers.
    #[tokio::test]
    async fn router_refuses_the_pairing_page_before_the_tunnel_registers() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let relay_dir = tempfile::tempdir().unwrap();
        let (base, _stub) = stub_worker(vec![token("token-one")]).await;
        let started = relay_dir.path().join("started");
        let go = relay_dir.path().join("go");
        let binary = relay_dir.path().join("cloudflared");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\n\
                 echo 'INF Registered tunnel connection connIndex=0' >&2\nexec sleep 600\n",
                started.display(),
                go.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let relay = supervisor(&state.data_dir, binary, &base, HostKey::generate());
        let (stop_tx, stop) = watch::channel(false);
        let run = tokio::spawn(relay.run(Some("grant-secret".into()), stop));

        let deadline = Instant::now() + Duration::from_secs(10);
        while !started.exists() {
            assert!(Instant::now() < deadline, "cloudflared never started");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_ne!(read_status(&state.data_dir).state, RelayState::Ready);
        assert_eq!(
            pair_status(&state, Some(&format!("{RELAY_HOST}:443"))).await,
            StatusCode::NOT_FOUND,
            "refused before the first registration line"
        );

        std::fs::write(&go, b"").unwrap();
        while read_status(&state.data_dir).state != RelayState::Ready {
            assert!(Instant::now() < deadline, "never ready");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(60), run)
            .await
            .unwrap()
            .unwrap();
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[test]
    fn worker_host_key_is_the_raw_point() {
        let key = HostKey::generate();
        let encoded = worker_host_key(&key);
        assert_eq!(encoded.len(), 87);
        let point = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        assert_eq!(point[0], 0x04);
        assert!(key.public_key_spki().ends_with(&point));
    }
}
