//! The supervisor's recovery endpoint and its wedged-host watchdog.
//!
//! The supervisor already outlives the host daemon, so it also answers a
//! paired phone when the daemon cannot: [`wonderd::rescue`] decides who may
//! ask and what (status, restart, redacted diagnostics); this module is the
//! supervisor's half. [`Shared`] is what the endpoint reads and the
//! supervisor loop writes, [`Endpoint`] serves it, and [`Watchdog`] notices a
//! daemon that is alive but no longer answering.
//!
//! The endpoint listens where the daemon's home network route does, on the
//! port after it (3779 by default), pinned to the same host key, and not at
//! all while home network connections are off. It is not part of the
//! Tailscale route: that forwards to the daemon's loopback address through
//! Tailscale Serve, which the supervisor does not extend. A phone away from
//! home cannot rescue the host until a second Serve port is designed.
//!
//! macOS and Linux supervisors run it. The Windows service has no home
//! network listener (`home_port`), so nothing listens there, and its
//! supervisor neither runs the watchdog nor stays up after giving up.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::Value;
use wonder_api::rescue::{RescueState, RescueStatus};
use wonderd::rescue::{Rescue, RescueHost};

use super::{home_port, listen_addr, Paths, Result};

/// What the supervisor knows about the daemon, shared with the endpoint.
pub(super) struct Shared {
    inner: Mutex<Inner>,
}

struct Inner {
    state: RescueState,
    last_exit: Option<String>,
    crash_count: u32,
    restart_requested: bool,
}

impl Shared {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                state: RescueState::Starting,
                last_exit: None,
                crash_count: 0,
                restart_requested: false,
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn set_state(&self, state: RescueState) {
        let mut inner = self.lock();
        // A host that is stopping stays stopped.
        if inner.state != RescueState::Stopped {
            inner.state = state;
        }
    }

    /// The daemon went away: why, and how many crashes the supervisor is
    /// still counting against its limit.
    pub(super) fn record_exit(&self, reason: String, crash_count: usize) {
        let mut inner = self.lock();
        inner.last_exit = Some(reason);
        inner.crash_count = crash_count.try_into().unwrap_or(u32::MAX);
    }

    /// True once per request, for the supervisor loop to act on.
    pub(super) fn take_restart_request(&self) -> bool {
        std::mem::take(&mut self.lock().restart_requested)
    }
}

impl RescueHost for Shared {
    fn status(&self) -> RescueStatus {
        let inner = self.lock();
        RescueStatus {
            state: inner.state,
            last_exit: inner.last_exit.clone(),
            crash_count: inner.crash_count,
            version: wonderd::RELEASE_VERSION.to_owned(),
        }
    }

    fn request_restart(&self) -> RescueState {
        let mut inner = self.lock();
        if inner.state == RescueState::Stopped {
            return RescueState::Stopped;
        }
        inner.restart_requested = true;
        inner.state = RescueState::Starting;
        inner.state
    }
}

/// How the endpoint is reached.
pub(super) enum Bind {
    /// On this computer's home network addresses, TLS pinned to the host key.
    Home(u16),
    /// Plain HTTP on a listener the caller made (tests, on loopback).
    #[cfg_attr(not(test), allow(dead_code))]
    Plain(TcpListener),
}

/// A running endpoint. Dropping it stops the listeners.
pub(super) struct Endpoint {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Endpoint {
    pub(super) fn start(paths: &Paths, bind: Bind, shared: Arc<Shared>) -> Result<Self> {
        let rescue = Rescue::new(&paths.data, &paths.logs(), shared);
        let router = rescue.router();
        let data = paths.data.clone();
        let logger = Arc::new(wonderd::logging::JsonlLogger::new(
            paths.logs(),
            "rescue.jsonl",
        )?);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (ready, started) = mpsc::channel::<std::result::Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("rescue".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => return drop(ready.send(Err(error.to_string()))),
                };
                runtime.block_on(async move {
                    match bind {
                        Bind::Home(port) => {
                            match wonderd::lan::spawn(
                                router,
                                &data,
                                port,
                                logger,
                                wonderd::lan::HomeRoutes::default(),
                            ) {
                                Ok(listeners) => {
                                    let _ = ready.send(Ok(()));
                                    let _ = stopped.await;
                                    listeners.abort();
                                }
                                Err(error) => {
                                    let _ = ready.send(Err(error.to_string()));
                                }
                            }
                        }
                        Bind::Plain(listener) => {
                            let listener = listener
                                .set_nonblocking(true)
                                .and_then(|()| tokio::net::TcpListener::from_std(listener));
                            match listener {
                                Ok(listener) => {
                                    let _ = ready.send(Ok(()));
                                    let _ = axum::serve(listener, router)
                                        .with_graceful_shutdown(async {
                                            let _ = stopped.await;
                                        })
                                        .await;
                                }
                                Err(error) => {
                                    let _ = ready.send(Err(error.to_string()));
                                }
                            }
                        }
                    }
                });
            })?;
        match started.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self {
                stop: Some(stop),
                thread: Some(thread),
            }),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err("the recovery endpoint did not start".into()),
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The rescue port: the one after the home network port.
pub(super) fn port(config: &Value) -> Option<u16> {
    home_port(config)?.checked_add(1)
}

/// What the supervisor does about the endpoint and the watchdog.
pub(super) struct Options {
    pub(super) bind: Option<Bind>,
    pub(super) watchdog: WatchdogPolicy,
}

impl Options {
    pub(super) const OFF: Self = Self {
        bind: None,
        watchdog: WatchdogPolicy::OFF,
    };

    /// The host's own setup: serve on the home network when it is on, and
    /// watch the daemon where a supervisor can act on what it sees.
    pub(super) fn host(config: &Value) -> Self {
        if !cfg!(unix) {
            return Self::OFF;
        }
        Self {
            bind: port(config).map(Bind::Home),
            watchdog: WatchdogPolicy::HOST,
        }
    }
}

/// What the supervisor loop is given to work with the endpoint.
pub(super) struct Supervision {
    pub(super) shared: Arc<Shared>,
    pub(super) watchdog: WatchdogPolicy,
    /// Whether a phone can reach the endpoint. Only then does a host that
    /// gave up restarting stay up for it.
    pub(super) served: bool,
}

/// How a daemon that stops answering is recognized.
#[derive(Clone, Copy, Debug)]
pub(super) struct WatchdogPolicy {
    /// Time between the starts of two probes; zero turns the watchdog off.
    pub(super) interval: Duration,
    /// How long one probe may take.
    pub(super) timeout: Duration,
    /// Consecutive failed probes that make a wedged daemon.
    pub(super) failures: u32,
    /// Failures in this long after a start, before the daemon first
    /// answered, are not counted: it is loading.
    pub(super) startup_grace: Duration,
}

impl WatchdogPolicy {
    pub(super) const OFF: Self = Self {
        interval: Duration::ZERO,
        timeout: Duration::ZERO,
        failures: 0,
        startup_grace: Duration::ZERO,
    };
    /// Three failed probes ten seconds apart, two minutes of startup.
    pub(super) const HOST: Self = Self {
        interval: Duration::from_secs(10),
        timeout: Duration::from_secs(5),
        failures: 3,
        startup_grace: Duration::from_secs(120),
    };
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WatchdogEvent {
    /// The daemon answered for the first time since it started.
    Answering,
    /// It failed `failures` probes in a row.
    Wedged,
}

/// Probes the daemon's liveness route (`/healthz`, which answers while the
/// daemon runs even when its readiness route says 503 during recovery) from
/// a short-lived thread at a time, so a daemon that never answers cannot
/// hold the supervisor loop.
pub(super) struct Watchdog {
    policy: WatchdogPolicy,
    address: Option<SocketAddr>,
    started: Instant,
    answered: bool,
    failed: u32,
    next_probe: Instant,
    probe: Option<mpsc::Receiver<bool>>,
}

impl Watchdog {
    pub(super) fn new(policy: WatchdogPolicy, config: &Value) -> Self {
        let now = Instant::now();
        Self {
            policy,
            address: listen_addr(config)
                .to_socket_addrs()
                .ok()
                .and_then(|mut addresses| addresses.next()),
            started: now,
            answered: false,
            failed: 0,
            next_probe: now,
            probe: None,
        }
    }

    fn enabled(&self) -> bool {
        !self.policy.interval.is_zero() && self.address.is_some()
    }

    /// A new daemon was started at `now`.
    pub(super) fn restarted(&mut self, now: Instant) {
        self.started = now;
        self.answered = false;
        self.failed = 0;
        self.probe = None;
        self.next_probe = now + self.policy.interval.min(Duration::from_secs(1));
    }

    /// Whether the daemon missed a probe since it last answered.
    pub(super) fn failing(&self) -> bool {
        self.failed > 0
    }

    pub(super) fn poll(&mut self, now: Instant) -> Option<WatchdogEvent> {
        if !self.enabled() {
            return None;
        }
        if let Some(probe) = &self.probe {
            let healthy = match probe.try_recv() {
                Ok(healthy) => healthy,
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(mpsc::TryRecvError::Disconnected) => false,
            };
            self.probe = None;
            if healthy {
                self.failed = 0;
                if !std::mem::replace(&mut self.answered, true) {
                    return Some(WatchdogEvent::Answering);
                }
            } else if self.answered || now.duration_since(self.started) >= self.policy.startup_grace
            {
                self.failed += 1;
                if self.failed >= self.policy.failures {
                    return Some(WatchdogEvent::Wedged);
                }
            }
            return None;
        }
        if now >= self.next_probe {
            self.next_probe = now + self.policy.interval;
            let (tx, rx) = mpsc::sync_channel(1);
            self.probe = Some(rx);
            let (address, timeout) = (self.address.unwrap(), self.policy.timeout);
            std::thread::spawn(move || {
                let _ = tx.send(probe(address, timeout));
            });
        }
        None
    }
}

/// True when `address` answers `GET /healthz` with a 2xx status in `timeout`.
fn probe(address: SocketAddr, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let Ok(mut stream) = TcpStream::connect_timeout(&address, timeout) else {
        return false;
    };
    let remaining = |deadline: Instant| deadline.saturating_duration_since(Instant::now());
    let _ = stream.set_write_timeout(Some(remaining(deadline).max(Duration::from_millis(1))));
    if stream
        .write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut head = [0u8; 12];
    let mut read = 0;
    while read < head.len() {
        let left = remaining(deadline);
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        match stream.read(&mut head[read..]) {
            Ok(0) | Err(_) => return false,
            Ok(count) => read += count,
        }
    }
    // "HTTP/1.1 200"
    head.starts_with(b"HTTP/1.") && head[9] == b'2'
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::{
        load_config, process::OwnedChild, supervise_with_rescue, write_private, CrashRestarts,
        DAEMON_STOP_GRACE,
    };
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use p256::ecdsa::{signature::Signer, Signature, SigningKey};
    use serde_json::json;
    use std::{
        os::unix::{fs::PermissionsExt, net::UnixStream},
        path::Path,
        process::Command,
    };
    use wonder_api::rescue::RescueCommand;

    const HOST_ID: &str = "0b9a3a30-4e0c-4c6a-9d19-1f6f3f5c2a11";

    /// The stub daemon: records each start, then serves `/healthz`. With a
    /// `wedge` file it accepts connections and never answers; with a `crash`
    /// file it exits at once.
    const DAEMON_PY: &str = r#"
import os, socket, sys, threading
port = int(sys.argv[1])
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port))
s.listen(16)
wedge = os.path.exists("wedge")
open("up-%d" % os.getpid(), "w").close()
while True:
    c, _ = s.accept()
    if wedge:
        continue
    c.recv(1024)
    c.sendall(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
    c.close()
"#;

    /// Runs the real supervisor in a separate process, so signal handlers
    /// and `STOPPING` stay out of other tests. Its settings are files in the
    /// data directory: `policy` = "limit first-delay-ms interval-ms
    /// timeout-ms failures grace-ms".
    #[test]
    fn rescue_supervisor_fixture() {
        if std::env::var_os("WONDER_RESCUE_FIXTURE").is_none() {
            return;
        }
        let paths = Paths::resolve().unwrap();
        let policy: Vec<u64> = std::fs::read_to_string(paths.data.join("policy"))
            .unwrap()
            .split_whitespace()
            .map(|part| part.parse().unwrap())
            .collect();
        let restarts = if policy[0] == 0 {
            CrashRestarts::NEVER
        } else {
            CrashRestarts {
                limit: policy[0] as usize,
                window: Duration::from_secs(600),
                first_delay: Duration::from_millis(policy[1]),
                max_delay: Duration::from_millis(policy[1] * 2),
            }
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        std::fs::write(
            paths.data.join("rescue-port"),
            listener.local_addr().unwrap().port().to_string(),
        )
        .unwrap();
        let options = Options {
            bind: Some(Bind::Plain(listener)),
            watchdog: WatchdogPolicy {
                interval: Duration::from_millis(policy[2]),
                timeout: Duration::from_millis(policy[3]),
                failures: policy[4] as u32,
                startup_grace: Duration::from_millis(policy[5]),
            },
        };
        supervise_with_rescue(
            &paths,
            &load_config(&paths).unwrap(),
            restarts,
            DAEMON_STOP_GRACE,
            options,
        )
        .unwrap();
    }

    struct Host {
        directory: tempfile::TempDir,
        child: OwnedChild,
        key: SigningKey,
        client: reqwest::Client,
        nonce: u64,
    }

    impl Drop for Host {
        fn drop(&mut self) {
            if let Ok(mut stream) = UnixStream::connect(self.root().join("headless/control.sock")) {
                let _ = stream.write_all(b"stop\n");
                let deadline = Instant::now() + Duration::from_secs(40);
                while !self.child.exited().unwrap_or(true) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn jwk(key: &SigningKey) -> String {
        let point = key.verifying_key().to_sec1_point(false);
        json!({"kty": "EC", "crv": "P-256",
            "x": URL_SAFE_NO_PAD.encode(point.x().unwrap()),
            "y": URL_SAFE_NO_PAD.encode(point.y().unwrap())})
        .to_string()
    }

    impl Host {
        /// A paired phone, a stub daemon, and a supervisor with `policy`
        /// (see `rescue_supervisor_fixture`), plus `files` in place first.
        async fn start(policy: &str, files: &[&str]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let paths = Paths {
                data: root.into(),
                state: root.join("headless"),
            };
            std::fs::create_dir_all(&paths.state).unwrap();
            write_private(
                &paths.config(),
                json!({"tunnel": "none", "origin": "https://host.example.test",
                    "listenAddr": format!("127.0.0.1:{}", free_port()),
                    "homeNetwork": {"enabled": false, "port": 3778}})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
            write_private(&paths.capability(), b"fixture-only").unwrap();
            std::fs::write(root.join("policy"), policy).unwrap();
            std::fs::write(root.join("daemon.py"), DAEMON_PY).unwrap();
            std::fs::write(root.join("host-installation-id"), format!("{HOST_ID}\n")).unwrap();
            let daemon = root.join("daemon");
            std::fs::write(
                &daemon,
                "#!/bin/sh\ncd \"$WONDER_DATA_DIR\" || exit 1\necho $$ > daemon.pid\necho start >> starts\n\
                 [ -e crash ] && exit 1\nexec python3 daemon.py \"${WONDER_LISTEN_ADDR##*:}\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&daemon, std::fs::Permissions::from_mode(0o700)).unwrap();
            for file in files {
                std::fs::write(root.join(file), "").unwrap();
            }
            // The paired phone, in the store the daemon would have written.
            let key = SigningKey::from_slice(&[7_u8; 32]).unwrap();
            let url = format!(
                "sqlite://{}?mode=rwc",
                root.join("wonder.sqlite3").display()
            );
            let store = wonder_store::Store::connect(&url).await.unwrap();
            store
                .upsert_owner_device("phone", "Phone", &jwk(&key), "now")
                .await
                .unwrap();
            let log = std::fs::File::create(root.join("host.log")).unwrap();
            let child = OwnedChild::spawn(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "rescue::tests::rescue_supervisor_fixture",
                        "--nocapture",
                    ])
                    .env("WONDER_RESCUE_FIXTURE", "1")
                    .env("WONDER_DATA_DIR", root)
                    .env("WONDER_DAEMON_BIN", &daemon)
                    .stdout(log.try_clone().unwrap())
                    .stderr(log),
            )
            .unwrap();
            drop(store);
            let host = Self {
                directory,
                child,
                key,
                client: reqwest::Client::new(),
                nonce: 0,
            };
            host.wait("endpoint up", || host.read("rescue-port").len() > 1)
                .await;
            host
        }

        fn root(&self) -> &Path {
            self.directory.path()
        }

        fn read(&self, file: &str) -> String {
            std::fs::read_to_string(self.root().join(file)).unwrap_or_default()
        }

        fn starts(&self) -> usize {
            self.read("starts").lines().count()
        }

        async fn wait(&self, label: &str, predicate: impl Fn() -> bool) {
            let start = Instant::now();
            while !predicate() {
                assert!(
                    start.elapsed() < Duration::from_secs(60),
                    "{label} timed out; log={}",
                    self.read("host.log")
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        fn sign(&mut self, command: RescueCommand, device: &str, key: &SigningKey) -> Value {
            self.nonce += 1;
            let nonce = format!("nonce-{:016}", self.nonce);
            let issued = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let signature: Signature =
                key.sign(&command.transcript_bytes(device, HOST_ID, &nonce, issued));
            json!({"deviceId": device, "actionNonce": nonce, "issuedAtMs": issued,
                "signature": URL_SAFE_NO_PAD.encode(signature.to_bytes())})
        }

        async fn send(&self, command: RescueCommand, body: &Value) -> (u16, Value) {
            let url = format!(
                "http://127.0.0.1:{}{}",
                self.read("rescue-port").trim(),
                command.path()
            );
            let response = self.client.post(url).json(body).send().await.unwrap();
            let status = response.status().as_u16();
            (status, response.json().await.unwrap_or(Value::Null))
        }

        /// A request signed by the paired phone.
        async fn call(&mut self, command: RescueCommand) -> (u16, Value) {
            let key = self.key.clone();
            let body = self.sign(command, "phone", &key);
            self.send(command, &body).await
        }

        async fn state(&mut self) -> String {
            self.call(RescueCommand::Status).await.1["state"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        }

        async fn wait_state(&mut self, wanted: &str) {
            let start = Instant::now();
            while self.state().await != wanted {
                assert!(
                    start.elapsed() < Duration::from_secs(60),
                    "never {wanted}; log={}",
                    self.read("host.log")
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        fn daemon_pid(&self) -> i32 {
            self.read("daemon.pid").trim().parse().unwrap()
        }
    }

    // limit 1: the first crash is replaced after 100 ms, the second gives up.
    const GIVE_UP_FAST: &str = "1 100 200 500 3 5000";
    // Watchdog off (interval 0 is "off"), restarts as in GIVE_UP_FAST.
    const NO_WATCHDOG: &str = "1 100 0 0 0 0";
    // Replaces a crashed daemon; probes every 100 ms, wedged after 3 misses.
    const WATCHED: &str = "5 100 100 300 3 1500";

    #[tokio::test]
    async fn rescue_serves_status_while_the_daemon_is_down_and_restarts_it() {
        let mut host = Host::start(GIVE_UP_FAST, &["crash"]).await;
        // Two crashes: one replaced, one past the limit. The supervisor
        // stays up and answers a paired phone.
        host.wait_state("gaveUp").await;
        let (status, body) = host.call(RescueCommand::Status).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["crashCount"], 2);
        assert_eq!(body["version"], wonderd::RELEASE_VERSION);
        assert!(
            body["lastExit"]
                .as_str()
                .unwrap()
                .contains("exit status: 1"),
            "{body}"
        );
        let starts = host.starts();
        assert_eq!(starts, 2);
        // Published for `cupola status` on the loop's next turn.
        host.wait("gave-up published", || {
            host.read("headless/origin-status.jsonl")
                .lines()
                .last()
                .is_some_and(|line| line.contains("\"gave-up\""))
        })
        .await;

        // Anyone else is refused, and nothing starts.
        let stranger = SigningKey::from_slice(&[9_u8; 32]).unwrap();
        let unpaired = host.sign(RescueCommand::Restart, "stranger", &stranger);
        assert_eq!(host.send(RescueCommand::Restart, &unpaired).await.0, 401);
        let forged = host.sign(RescueCommand::Restart, "phone", &stranger);
        assert_eq!(host.send(RescueCommand::Restart, &forged).await.0, 401);
        let key = host.key.clone();
        let once = host.sign(RescueCommand::Restart, "phone", &key);
        std::fs::remove_file(host.root().join("crash")).unwrap();
        let (status, body) = host.send(RescueCommand::Restart, &once).await;
        assert_eq!((status, &body["accepted"]), (200, &json!(true)), "{body}");
        // The same request again is a replay.
        assert_eq!(host.send(RescueCommand::Restart, &once).await.0, 401);

        // The daemon is back, answering, and the give-up is forgotten.
        host.wait_state("running").await;
        assert_eq!(host.starts(), starts + 1);
        let (_, body) = host.call(RescueCommand::Status).await;
        assert_eq!(body["crashCount"], 0);
        let (status, body) = host.call(RescueCommand::Diagnostics).await;
        assert_eq!(
            (status, &body["status"]["state"]),
            (200, &json!("running")),
            "{body}"
        );
        assert!(body["logLines"].is_array());
    }

    #[tokio::test]
    async fn rescue_restart_replaces_a_running_daemon_and_is_rate_limited() {
        let mut host = Host::start(NO_WATCHDOG, &[]).await;
        host.wait("daemon started", || host.starts() == 1).await;
        let first_pid = host.daemon_pid();
        for round in 2..=4 {
            let (status, body) = host.call(RescueCommand::Restart).await;
            assert_eq!((status, &body["accepted"]), (200, &json!(true)), "{body}");
            host.wait("restarted", || host.starts() == round).await;
        }
        assert_ne!(host.daemon_pid(), first_pid);
        // The old daemon is gone, not left running beside the new one.
        host.wait(
            "old daemon gone",
            || unsafe { libc::kill(first_pid, 0) } != 0,
        )
        .await;
        // The fourth request in ten minutes is refused and starts nothing.
        let (status, body) = host.call(RescueCommand::Restart).await;
        assert_eq!(
            (status, &body["error"]),
            (429, &json!("rate_limited")),
            "{body}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(host.starts(), 4);
        // A requested restart is not a crash.
        assert_eq!(host.call(RescueCommand::Status).await.1["crashCount"], 0);
    }

    #[tokio::test]
    async fn rescue_watchdog_replaces_a_stopped_daemon() {
        let mut host = Host::start(WATCHED, &[]).await;
        host.wait_state("running").await;
        let stopped = host.daemon_pid();
        // SIGSTOP: alive, holding its port, answering nothing.
        unsafe { libc::kill(stopped, libc::SIGSTOP) };
        host.wait("replaced", || host.starts() == 2).await;
        host.wait_state("running").await;
        assert_ne!(host.daemon_pid(), stopped);
        host.wait(
            "stopped daemon killed",
            || unsafe { libc::kill(stopped, 0) } != 0,
        )
        .await;
        let (_, body) = host.call(RescueCommand::Status).await;
        assert_eq!(body["crashCount"], 1);
        assert_eq!(body["lastExit"], "it stopped answering");
    }

    #[tokio::test]
    async fn rescue_watchdog_replaces_a_daemon_that_never_answers() {
        // A daemon that accepts and says nothing from the start is wedged
        // once its startup grace is up; its replacement answers.
        let mut host = Host::start(WATCHED, &["wedge"]).await;
        host.wait("first start", || host.starts() == 1).await;
        let wedged = host.daemon_pid();
        // Its replacement must not wedge too, once it is listening.
        host.wait("first daemon listening", || {
            host.root().join(format!("up-{wedged}")).exists()
        })
        .await;
        std::fs::remove_file(host.root().join("wedge")).unwrap();
        host.wait("replaced", || host.starts() == 2).await;
        host.wait_state("running").await;
        assert_ne!(host.daemon_pid(), wedged);
    }

    #[tokio::test]
    async fn rescue_wedged_daemon_counts_toward_giving_up() {
        // Every daemon wedges, so the watchdog's kills use up the restart
        // limit like crashes do; the host then waits for a phone.
        let mut host = Host::start("1 100 100 300 2 300", &["wedge"]).await;
        host.wait_state("gaveUp").await;
        assert_eq!(host.starts(), 2);
        let (_, body) = host.call(RescueCommand::Status).await;
        assert_eq!(
            (&body["crashCount"], &body["lastExit"]),
            (&json!(2), &json!("it stopped answering"))
        );
    }

    #[tokio::test]
    async fn rescue_without_a_restart_policy_a_wedged_daemon_ends_the_host() {
        // Linux under systemd: no replacements, so a wedge stops the
        // supervisor and the service manager restarts the host.
        let host = Host::start("0 0 100 300 2 300", &["wedge"]).await;
        host.wait("supervisor exited", || host.child.exited().unwrap_or(false))
            .await;
        assert!(host.read("host.log").contains("a host process exited"));
    }

    /// One poll at the fixture's clock `at`, then waits for the probe it
    /// started (a closed port refuses at once) and returns what that said.
    fn round(watchdog: &mut Watchdog, at: Instant) -> Option<WatchdogEvent> {
        loop {
            if let Some(event) = watchdog.poll(at) {
                return Some(event);
            }
            watchdog.probe.as_ref()?;
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn rescue_watchdog_waits_out_startup_before_counting_failures() {
        let closed = free_port();
        let config = json!({"listenAddr": format!("127.0.0.1:{closed}")});
        let policy = WatchdogPolicy {
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(10),
            failures: 2,
            startup_grace: Duration::from_secs(40),
        };
        let mut watchdog = Watchdog::new(policy, &config);
        // The fixture's clock, not the wall clock, decides what has elapsed.
        let start = Instant::now();
        watchdog.restarted(start);
        let at = |seconds| start + Duration::from_secs(seconds);
        for seconds in [1, 11, 21, 31] {
            assert_eq!(round(&mut watchdog, at(seconds)), None);
        }
        assert!(
            !watchdog.failing(),
            "counted a failure during startup grace"
        );
        assert_eq!(round(&mut watchdog, at(41)), None);
        assert!(watchdog.failing());
        assert_eq!(round(&mut watchdog, at(51)), Some(WatchdogEvent::Wedged));
    }

    #[test]
    fn rescue_watchdog_ignores_a_ready_route_that_says_503() {
        // Only an answer counts; the daemon's recovery 503s are not a wedge,
        // and neither is its readiness: the probe asks for liveness.
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for status in ["200 OK", "204 No Content", "503 Service Unavailable"] {
                let (mut stream, _) = server.accept().unwrap();
                let mut request = [0u8; 256];
                let read = stream.read(&mut request).unwrap();
                assert!(request[..read].starts_with(b"GET /healthz "));
                write!(stream, "HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n").unwrap();
            }
        });
        let timeout = Duration::from_secs(10);
        assert!(probe(address, timeout));
        assert!(probe(address, timeout));
        assert!(!probe(address, timeout));
        handle.join().unwrap();
    }
}
