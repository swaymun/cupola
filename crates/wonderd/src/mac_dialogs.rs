//! macOS dialogs that wait for the person at the Mac (a privacy permission
//! prompt, a keychain or administrator password, a download confirmation)
//! while a thread's turn runs. An agent's `osascript` call, for example, can
//! hang on an Automation prompt with nothing in the conversation saying why.
//!
//! While a turn runs or an observed dialog still waits, the computer-use helper
//! reports the dialogs on screen. One host-level snapshot powers a named,
//! counted computer notice. Window ownership does not identify the conversation
//! that raised a dialog, so observation never marks a chat as blocked.
//! Wonder never clicks these dialogs.
use super::*;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::io::BufReader;

const POLL: Duration = Duration::from_secs(2);

static DIALOG_WINDOWS: LazyLock<Mutex<Vec<DialogWindow>>> = LazyLock::new(Default::default);
static SNAPSHOT_KNOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct DialogWindow {
    #[serde(rename = "windowId")]
    pub window_id: u32,
    pub owner: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

/// Every tap resolves a currently observed window, including after another
/// dialog was answered. A stale notification never resurrects a closed target.
pub(crate) fn dialog_window() -> Option<DialogWindow> {
    DIALOG_WINDOWS.lock().ok()?.first().cloned()
}

/// Group descriptions without losing the individual windows used for capture.
/// Phones name the reported title or dialog kind, never the system process owner.
fn snapshot_for(windows: &[DialogWindow]) -> serde_json::Value {
    let mut groups = BTreeMap::new();
    for window in windows {
        *groups
            .entry((&window.owner, &window.title, &window.kind))
            .or_insert(0usize) += 1;
    }
    let dialogs: Vec<_> = groups.into_iter().map(|((owner, title, kind), count)| {
        serde_json::json!({"owner": owner, "title": title, "kind": kind, "count": count})
    }).collect();
    serde_json::json!({"count": windows.len(), "dialogs": dialogs})
}

pub(crate) fn snapshot() -> serde_json::Value {
    DIALOG_WINDOWS
        .lock()
        .map(|windows| snapshot_for(&windows))
        .unwrap_or_else(|_| snapshot_for(&[]))
}

/// Reading the last snapshot does not start native observation. Running work
/// and already-pending dialogs own the watcher, independently of phone presence.
pub(crate) fn observe() -> Option<serde_json::Value> {
    SNAPSHOT_KNOWN.load(Ordering::SeqCst).then(snapshot)
}

pub(crate) async fn forget_window(state: &AppState, window: &DialogWindow) {
    if let Ok(mut windows) = DIALOG_WINDOWS.lock() {
        windows.retain(|current| current != window);
    }
    sync_computer_push(state, &running(state).await).await;
}

pub(crate) async fn clear_gone(state: &AppState) {
    sync_computer_push(state, &running(state).await).await;
}
/// Followed threads whose turn runs in another app on the Mac (external_push.rs).
static EXTERNAL_RUNNING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// The followed threads external_push saw working in their desktop app.
pub(crate) fn set_external_running(running: HashSet<String>) {
    if let Ok(mut external) = EXTERNAL_RUNNING.lock() {
        *external = running;
    }
}

/// One `--watch-dialogs` line: the dialogs now on screen.
fn dialog_snapshot(line: &[u8]) -> Option<DialogSnapshot> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let dialogs = value.get("dialogs")?.as_array()?;
    if dialogs.len() > 256 {
        return None;
    }
    let mut seen = HashSet::new();
    let mut windows = Vec::new();
    for value in dialogs {
        let mut window = serde_json::from_value::<DialogWindow>(value.clone()).ok()?;
        window.owner = window.owner.trim().to_owned();
        window.title = window.title.and_then(|title| {
            let title = title.trim();
            let mut end = title.len().min(240);
            while !title.is_char_boundary(end) {
                end -= 1;
            }
            (!title.is_empty()).then(|| title[..end].to_owned())
        });
        window.kind = window.kind.and_then(|kind| {
            let kind = kind.trim();
            (!kind.is_empty()).then(|| kind.to_owned())
        });
        if window.kind.as_ref().is_some_and(|kind| kind.len() > 80) {
            return None;
        }
        if window.window_id == 0 || window.owner.is_empty() || window.owner.len() > 150 {
            return None;
        }
        if seen.insert(window.window_id) {
            windows.push(window);
        }
    }
    Some(DialogSnapshot {
        open: !windows.is_empty(),
        windows,
    })
}

#[derive(Debug)]
struct DialogSnapshot {
    open: bool,
    windows: Vec<DialogWindow>,
}

#[derive(Default)]
struct ComputerPush {
    host: String,
    key: Option<String>,
    conversation: Option<String>,
    event_snapshot: Option<serde_json::Value>,
    notice: Option<serde_json::Value>,
}
static COMPUTER_PUSH: LazyLock<tokio::sync::Mutex<ComputerPush>> = LazyLock::new(Default::default);

/// One host alert per description/count change, independent of running chats.
/// New counts supersede unsent alerts; the service extension replaces delivered
/// alerts for this host. A zero withdraws outstanding delivery.
async fn sync_computer_push(state: &AppState, running: &HashSet<String>) {
    // The watcher and a tap that proves a window gone share one write boundary.
    // Hold it through storage so an older insertion cannot follow withdrawal.
    let mut last = COMPUTER_PUSH.lock().await;
    let notice = snapshot();
    let count = notice["count"].as_u64().unwrap_or(0) as usize;
    let same_host = last.host == state.host_installation_id;
    if !same_host {
        last.event_snapshot = None;
    }
    // Journal count changes for replay/reconnect; phones never poll this watcher.
    // A failed journal write is retried at the same serialized boundary.
    if SNAPSHOT_KNOWN.load(Ordering::SeqCst)
        && last.event_snapshot.as_ref() != Some(&notice)
        && crate::publish_event_with_context(
            state,
            wonder_api::WonderEvent::Activity {
                category: "mac_dialogs".into(),
                state: "changed".into(),
                detail: Some(notice.to_string()),
            },
            crate::EventContext::default(),
        )
        .await
        .is_ok()
    {
        last.event_snapshot = Some(notice.clone());
    }
    let (previous, key, conversation) = {
        if same_host
            && last.notice.as_ref() == Some(&notice)
            && (count == 0 || last.key.is_some() || running.is_empty())
        {
            return;
        }
        let previous = same_host.then(|| last.key.take()).flatten();
        // Existing push authorization stays bound to a real Project thread.
        // Its encrypted content routes the phone to the computer, not that chat.
        let conversation = running
            .iter()
            .min()
            .cloned()
            .or_else(|| same_host.then(|| last.conversation.clone()).flatten());
        let key = (count > 0 && conversation.is_some())
            .then(|| format!("mac-permission:computer:{}", uuid::Uuid::new_v4()));
        *last = ComputerPush {
            host: state.host_installation_id.clone(),
            key: key.clone(),
            conversation: conversation.clone(),
            event_snapshot: last.event_snapshot.clone(),
            notice: Some(notice),
        };
        (previous, key, conversation)
    };
    if let Some(previous) = previous {
        let _ = state.store.withdraw_external_push(&previous).await;
    }
    if let (Some(key), Some(conversation)) = (key, conversation) {
        if !matches!(
            state
                .store
                .enqueue_external_push(&key, &conversation, "attention", None)
                .await,
            Ok(true)
        ) {
            // A registration may appear while a dialog still waits. Retry the
            // current count rather than treating a failed insert as delivered.
            last.key = None;
        }
    }
}

#[derive(Clone, Copy)]
struct Timing {
    poll: Duration,
    retry_initial: Duration,
    retry_max: Duration,
    healthy_after: Duration,
    first_snapshot: Duration,
    unverified_after: Duration,
}

const TIMING: Timing = Timing {
    poll: POLL,
    retry_initial: POLL,
    retry_max: Duration::from_secs(60),
    healthy_after: Duration::from_secs(30),
    first_snapshot: Duration::from_secs(10),
    unverified_after: Duration::from_secs(30),
};

#[derive(Debug)]
enum Failure {
    Launch(std::io::ErrorKind),
    Read(std::io::ErrorKind),
    Eof,
    InvalidOutput,
    FirstSnapshotTimeout,
}

/// How long a watcher gets to exit after its standard input closes.
const STOP_GRACE: Duration = Duration::from_secs(1);

struct Watch {
    /// Taken only by `Drop`, which stops the watcher.
    child: Option<tokio::process::Child>,
    snapshots: tokio::sync::mpsc::Receiver<Result<DialogSnapshot, Failure>>,
    reader: tokio::task::JoinHandle<()>,
    started: tokio::time::Instant,
    valid_since: Option<tokio::time::Instant>,
}

impl Watch {
    fn try_exit(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut()?.try_wait().ok().flatten()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.reader.abort();
        let Some(mut child) = self.child.take() else {
            return;
        };
        // Closing standard input is the watcher's stop signal; the macOS
        // launcher then exits and cleans up after itself. A watcher that
        // ignores it is killed after the grace, or at once (kill_on_drop)
        // when no runtime is left to wait.
        drop(child.stdin.take());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if tokio::time::timeout(STOP_GRACE, child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                }
            });
        }
    }
}

fn launch(bin: &std::path::Path) -> Result<Watch, Failure> {
    let mut child = crate::computer_sessions::helper_process(bin)
        .arg("--watch-dialogs")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // Never retain or log arbitrary helper stderr (which can contain window data).
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| Failure::Launch(error.kind()))?;
    #[cfg(all(test, target_os = "macos"))]
    tests::observe_child(bin, child.id().expect("spawned child PID"));
    let stdout = child
        .stdout
        .take()
        .ok_or(Failure::Read(std::io::ErrorKind::BrokenPipe))?;
    let (send, snapshots) = tokio::sync::mpsc::channel(1);
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout);
        let mut buffer = Vec::new();
        loop {
            // Keep partial lines in this owned task: a polling tick must not cancel
            // read_bounded_line and discard the beginning of a snapshot.
            let snapshot =
                match crate::computer_sessions::read_bounded_line(&mut lines, &mut buffer).await {
                    Ok(Some(line)) => dialog_snapshot(&line).ok_or(Failure::InvalidOutput),
                    Ok(None) => Err(Failure::Eof),
                    Err(error) => Err(Failure::Read(error.kind())),
                };
            let failed = snapshot.is_err();
            if send.send(snapshot).await.is_err() || failed {
                break;
            }
        }
    });
    Ok(Watch {
        child: Some(child),
        snapshots,
        reader,
        started: tokio::time::Instant::now(),
        valid_since: None,
    })
}

async fn next_snapshot(watch: &mut Option<Watch>) -> Result<DialogSnapshot, Failure> {
    match watch {
        Some(watch) => watch.snapshots.recv().await.unwrap_or(Err(Failure::Eof)),
        None => std::future::pending().await,
    }
}

struct Retry {
    delay: Duration,
    at: tokio::time::Instant,
}

impl Retry {
    fn failed(
        &mut self,
        state: &AppState,
        failure: Failure,
        watch: &mut Option<Watch>,
        timing: Timing,
    ) {
        self.delay = if self.delay.is_zero() {
            timing.retry_initial
        } else {
            self.delay.saturating_mul(2).min(timing.retry_max)
        };
        self.at = tokio::time::Instant::now() + self.delay;
        let exit = watch.as_mut().and_then(Watch::try_exit);
        let (reason, error_kind) = match failure {
            Failure::Launch(kind) => ("launch", Some(format!("{kind:?}"))),
            Failure::Read(kind) => ("read", Some(format!("{kind:?}"))),
            Failure::Eof => ("eof", None),
            Failure::InvalidOutput => ("invalid_output", None),
            Failure::FirstSnapshotTimeout => ("first_snapshot_timeout", None),
        };
        // One fixed-size operational record per delayed retry, without paths,
        // window metadata, raw output, or stderr buffers.
        let _ = state.logger.record(
            "warn",
            "mac_dialog_watcher_failed",
            serde_json::json!({
                "reason": reason,
                "errorKind": error_kind,
                "exitCode": exit.and_then(|status| status.code()),
                "retryMs": self.delay.as_millis() as u64,
            }),
        );
        *watch = None;
    }
}

async fn running(state: &AppState) -> HashSet<String> {
    let mut running: HashSet<String> = state
        .store
        .running_conversations()
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    if let Ok(external) = EXTERNAL_RUNNING.lock() {
        running.extend(external.iter().cloned());
    }
    running
}

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    spawn_with_timing(state, TIMING)
}

fn spawn_with_timing(state: AppState, timing: Timing) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg!(target_os = "macos") || state.computer_use_bin.is_none() {
            return;
        }
        let mut watch: Option<Watch> = None;
        let mut dialog_open = false;
        let mut active = false;
        let mut unverified_since: Option<tokio::time::Instant> = None;
        let mut retry = Retry {
            delay: Duration::ZERO,
            at: tokio::time::Instant::now(),
        };
        let mut interval = tokio::time::interval(timing.poll);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let failure = tokio::select! {
                _ = interval.tick() => watch.as_ref().and_then(|watch| {
                    (watch.valid_since.is_none() && watch.started.elapsed() >= timing.first_snapshot)
                        .then_some(Failure::FirstSnapshotTimeout)
                }),
                snapshot = next_snapshot(&mut watch) => match snapshot {
                    Ok(snapshot) => {
                        unverified_since = None;
                        dialog_open = snapshot.open;
                        if let Ok(mut windows) = DIALOG_WINDOWS.lock() {
                            *windows = snapshot.windows;
                            SNAPSHOT_KNOWN.store(true, Ordering::SeqCst);
                        }
                        if let Some(watch) = &mut watch {
                            watch.valid_since.get_or_insert_with(tokio::time::Instant::now);
                        }
                        None
                    }
                    Err(failure) => Some(failure),
                },
                _ = tokio::time::sleep_until(retry.at), if active && watch.is_none() => None,
            };
            // A spawn or a single valid line followed by an immediate exit is
            // not recovery. Quiet after a valid snapshot is normal: the helper
            // only emits changes, so require sustained valid service instead.
            if watch.as_ref().is_some_and(|watch| {
                watch
                    .valid_since
                    .is_some_and(|since| since.elapsed() >= timing.healthy_after)
            }) {
                retry.delay = Duration::ZERO;
            }
            if let Some(failure) = failure {
                retry.failed(&state, failure, &mut watch, timing);
                // Brief failures retain attention, but an unverifiable snapshot
                // must not leave a permanent count or a dead window target.
                unverified_since.get_or_insert_with(tokio::time::Instant::now);
            }
            if unverified_since.is_some_and(|since| since.elapsed() >= timing.unverified_after) {
                dialog_open = false;
                SNAPSHOT_KNOWN.store(true, Ordering::SeqCst);
            }
            let running = running(&state).await;
            active = !running.is_empty() || dialog_open;
            if !active {
                watch = None;
                dialog_open = false;
            } else if watch.is_none() && tokio::time::Instant::now() >= retry.at {
                match launch(state.computer_use_bin.as_ref().expect("configured helper")) {
                    Ok(started) => watch = Some(started),
                    Err(failure) => retry.failed(&state, failure, &mut watch, timing),
                }
            }
            if !dialog_open {
                if let Ok(mut windows) = DIALOG_WINDOWS.lock() {
                    windows.clear();
                }
            }
            sync_computer_push(&state, &running).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The daemon has one watcher; tests serialize their host snapshot and push state.
    #[cfg(unix)]
    static WATCH_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[cfg(target_os = "macos")]
    const FAST: Timing = Timing {
        poll: Duration::from_millis(10),
        retry_initial: Duration::from_millis(50),
        retry_max: Duration::from_millis(200),
        healthy_after: Duration::from_millis(150),
        first_snapshot: Duration::from_secs(2),
        unverified_after: Duration::from_secs(30),
    };

    #[cfg(target_os = "macos")]
    async fn helper_fixture(body: &str) -> (tempfile::TempDir, AppState) {
        use std::os::unix::fs::PermissionsExt;
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        DIALOG_WINDOWS.lock().unwrap().clear();
        SNAPSHOT_KNOWN.store(false, Ordering::SeqCst);
        *COMPUTER_PUSH.lock().await = ComputerPush::default();
        let helper = dir.path().join("watch-dialogs");
        std::fs::write(dir.path().join("observe-children"), "").unwrap();
        std::fs::write(&helper, format!(
            "#!/bin/sh\n[ \"$1\" = --watch-dialogs ] || exit 64\ncd \"$(dirname \"$0\")\" || exit 70\n[ ! -p startup-gate ] || {{ : > startup-entered; read -r ready < startup-gate; }}\necho $$ >> launches\n{body}\n"
        )).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        state.computer_use_bin = Some(helper);
        (dir, state)
    }

    #[cfg(target_os = "macos")]
    async fn start_turn(state: &AppState) -> String {
        let wonder_store::MessageInsert::Inserted(message) = state
            .store
            .insert_message(
                "owner",
                "retry",
                "work",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "0",
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        state
            .store
            .update_message_delivery(&message.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        message.id
    }

    // Test-only ownership observation at the actual spawn boundary, independent
    // of shell startup and its readiness marker. Never used by Release.
    #[cfg(target_os = "macos")]
    pub(super) fn observe_child(bin: &std::path::Path, pid: u32) {
        use std::io::Write;
        let dir = bin.parent().unwrap();
        if dir.join("observe-children").exists() {
            writeln!(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("owned-children"))
                    .unwrap(),
                "{pid}"
            )
            .unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    fn owned_pids(dir: &std::path::Path) -> Vec<u32> {
        std::fs::read_to_string(dir.join("owned-children"))
            .unwrap_or_default()
            .lines()
            .map(|pid| pid.parse().unwrap())
            .collect()
    }

    // Real time bounds fixture readiness and cleanup even when watcher time is
    // paused. Yielding keeps Tokio from auto-advancing past OS child startup.
    #[cfg(target_os = "macos")]
    async fn observed(mut ready: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !ready() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::task::yield_now().await;
        }
        true
    }

    /// Keep paused watcher time still while real fixture/store I/O completes,
    /// using the same real-time readiness bound as `observed`.
    #[cfg(target_os = "macos")]
    async fn observed_io<F: std::future::Future>(future: F) -> F::Output {
        tokio::select! {
            result = future => result,
            _ = observed(|| false) => panic!("fixture I/O did not reach its observed state"),
        }
    }

    /// Let a stopped watcher's paused stop grace run out, so a helper that
    /// ignores its closed standard input is killed. Yield first: the stop task
    /// starts its grace only when first polled.
    #[cfg(target_os = "macos")]
    async fn stop_grace_elapses() {
        tokio::task::yield_now().await;
        tokio::time::advance(STOP_GRACE + Duration::from_millis(1)).await;
    }

    #[cfg(target_os = "macos")]
    async fn children_reaped(dir: &std::path::Path) -> Vec<u32> {
        let children = owned_pids(dir);
        assert!(
            observed(|| children.iter().all(|pid| !alive(*pid))).await,
            "owned children still present: {children:?}"
        );
        for pid in &children {
            let mut status = 0;
            // ESRCH alone does not prove our child was waited on. ECHILD after
            // disappearance proves Tokio reaped it, rather than this assertion.
            assert_eq!(
                unsafe { libc::waitpid(*pid as libc::pid_t, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
        eprintln!(
            "OWNERSHIP child_pids={children:?} reaped={} remaining=0",
            children.len()
        );
        children
    }

    /// The OS child exists but cannot reach the old in-script PID marker.
    /// This must still count as an owned launch and be reaped after timeout.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn startup_timeout_owns_and_reaps_a_child_before_its_script_marker() {
        use std::os::unix::ffi::OsStrExt;
        let _guard = WATCH_TEST.lock().await;
        let (dir, state) = helper_fixture("exit 42").await;
        let gate =
            std::ffi::CString::new(dir.path().join("startup-gate").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(gate.as_ptr(), 0o600) }, 0);
        start_turn(&state).await;
        tokio::time::pause();
        let task = spawn_with_timing(state, FAST);
        tokio::task::yield_now().await;
        tokio::time::advance(FAST.poll).await;
        let ready = observed(|| {
            owned_pids(dir.path()).len() == 1 && dir.path().join("startup-entered").exists()
        })
        .await;
        if ready {
            tokio::time::advance(FAST.first_snapshot + FAST.poll).await;
        }
        let mut reached = ready && observed(|| !failures(dir.path()).is_empty()).await;
        // Only the first child is startup-delayed. Later attempts reach the
        // normal exit fixture, reproducing the original six-failures/five-markers
        // evidence without depending on scheduler load.
        std::fs::remove_file(dir.path().join("startup-gate")).unwrap();
        for attempt in 1..6 {
            if !reached {
                break;
            }
            let delay = failures(dir.path())[attempt - 1]["retryMs"]
                .as_u64()
                .unwrap();
            tokio::time::advance(Duration::from_millis(delay)).await;
            reached = observed(|| {
                owned_pids(dir.path()).len() == attempt + 1
                    && std::fs::read_to_string(dir.path().join("launches"))
                        .unwrap_or_default()
                        .lines()
                        .count()
                        == attempt
            })
            .await
                && observed(|| failures(dir.path()).len() == attempt + 1).await;
        }
        task.abort();
        let _ = task.await;
        // The startup-delayed child is blocked before it can see stdin close.
        stop_grace_elapses().await;
        let owned = children_reaped(dir.path()).await;
        let children = pids(dir.path());
        let records = failures(dir.path());
        let markers = std::fs::read_to_string(dir.path().join("launches"))
            .unwrap_or_default()
            .lines()
            .count();
        eprintln!("STARTUP delayed_before_marker={ready} sequence_observed={reached} actual_children={} counted_launches={} script_markers={markers} failures={records:?}", owned.len(), children.len());
        assert!(ready && reached, "startup-timeout sequence did not run");
        assert_eq!(owned.len(), 6);
        assert_eq!(
            children.len(),
            6,
            "a pre-marker timeout still owns a real child"
        );
        assert_eq!(markers, 5);
        assert_eq!(records.len(), 6);
        let reasons: Vec<_> = records
            .iter()
            .map(|record| record["reason"].as_str().unwrap())
            .collect();
        assert_eq!(
            reasons,
            ["first_snapshot_timeout", "eof", "eof", "eof", "eof", "eof"]
        );
        let delays: Vec<_> = records
            .iter()
            .map(|record| record["retryMs"].as_u64().unwrap())
            .collect();
        assert_eq!(delays, [50, 100, 200, 200, 200, 200]);
    }

    #[cfg(target_os = "macos")]
    fn pids(dir: &std::path::Path) -> Vec<u32> {
        owned_pids(dir)
    }

    #[cfg(target_os = "macos")]
    fn ready_pids(dir: &std::path::Path) -> Vec<u32> {
        std::fs::read_to_string(dir.join("launches"))
            .unwrap_or_default()
            .lines()
            .map(|pid| pid.parse().unwrap())
            .collect()
    }

    #[cfg(target_os = "macos")]
    fn failures(dir: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join("logs/test.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|record| record["event"] == "mac_dialog_watcher_failed")
            .collect()
    }

    #[cfg(target_os = "macos")]
    fn alive(pid: u32) -> bool {
        // Only PIDs recorded by this test's own child fixture.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(target_os = "macos")]
    struct FixtureTask(Option<tokio::task::JoinHandle<()>>);

    #[cfg(target_os = "macos")]
    impl FixtureTask {
        async fn start(state: AppState, timing: Timing) -> Self {
            let task = Self(Some(spawn_with_timing(state, timing)));
            tokio::task::yield_now().await;
            // Drive the initial interval tick, then hold watcher time while the
            // OS child starts. This does not change either first-snapshot limit.
            tokio::time::advance(timing.poll).await;
            task
        }

        async fn stop(mut self, dir: &std::path::Path) -> Vec<u32> {
            let task = self.0.take().unwrap();
            task.abort();
            let _ = task.await;
            stop_grace_elapses().await;
            children_reaped(dir).await
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for FixtureTask {
        fn drop(&mut self) {
            if let Some(task) = &self.0 {
                task.abort();
            }
        }
    }

    #[cfg(target_os = "macos")]
    async fn hold_clock(duration: Duration) {
        let until = std::time::Instant::now() + duration;
        while std::time::Instant::now() < until {
            tokio::task::yield_now().await;
        }
    }

    /// Observe a helper the watcher stops on its own (after its store reads).
    /// Drive the paused stop grace while retaining the real-time hang guard.
    #[cfg(target_os = "macos")]
    async fn stopped_after_grace(pid: u32) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while alive(pid) {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            stop_grace_elapses().await;
        }
        true
    }

    /// Observe the production retry floor after a real child has failed.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn failed_helpers_obey_the_production_retry_floor() {
        let _guard = WATCH_TEST.lock().await;
        for (mode, body, reason) in [
            ("exit", "exit 42", "eof"),
            (
                "invalid",
                "echo 'not-json-private-payload'; exit 42",
                "invalid_output",
            ),
        ] {
            let (dir, state) = helper_fixture(body).await;
            start_turn(&state).await;
            let startup = std::time::Instant::now();
            let task = spawn(state);
            // Store/process scheduling is not part of the retry observation.
            // Require both an actual child launch and its handled failure; a
            // watcher that never launches or reports failure must fail readiness.
            let ready = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    if !ready_pids(dir.path()).is_empty() && !failures(dir.path()).is_empty() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            let startup_elapsed = startup.elapsed();
            let start = std::time::Instant::now();
            if ready.is_ok() {
                tokio::time::sleep(Duration::from_millis(750)).await;
            }
            // Join before assertions (including readiness failure) and retain
            // WATCH_TEST until every recorded child has exited.
            task.abort();
            let _ = task.await;
            let elapsed = start.elapsed();
            let children = pids(dir.path());
            let records = failures(dir.path());
            children_reaped(dir.path()).await;
            eprintln!("AFTER production mode={mode} ready={} startup={startup_elapsed:?} launches={} elapsed={elapsed:?} child_pids={children:?} children_stopped=true failures={records:?}", ready.is_ok(), children.len());
            assert!(ready.is_ok(), "helper launch and failure were not observed");
            assert_eq!(children.len(), 1);
            assert_eq!(records.len(), 1);
            assert_eq!(records[0]["retryMs"], 2000);
            assert_eq!(records[0]["reason"], reason);
            assert_eq!(ready_pids(dir.path()), children);
        }
    }

    /// Real spawn/read/exit failures own the retry contract; timing is scaled so
    /// repeated capped retries can be checked without a multi-minute test.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn failing_children_escalate_and_cap_retries_without_resetting_on_spawn() {
        let _guard = WATCH_TEST.lock().await;
        for (mode, body, reason) in [
            ("exit", "exit 42", "eof"),
            (
                "invalid",
                "while :; do echo 'not-json-private-payload'; done",
                "invalid_output",
            ),
            ("valid_exit", "echo '{\"dialogs\":[]}'; exit 42", "eof"),
            ("unterminated", "printf '{\"dialogs\":[]}'", "read"),
            (
                "oversized",
                "exec /usr/bin/awk 'BEGIN { for (i=0; i<140000; i++) printf \"x\"; print \"\" }'",
                "read",
            ),
            ("silent", "exec /bin/cat", "first_snapshot_timeout"),
            ("missing", "exit 42", "launch"),
        ] {
            let (dir, state) = helper_fixture(body).await;
            if mode == "missing" {
                std::fs::remove_file(state.computer_use_bin.as_ref().unwrap()).unwrap();
            }
            start_turn(&state).await;
            tokio::time::pause();
            let task = FixtureTask::start(state, FAST).await;
            let mut reached = true;
            let mut early_launch = false;
            for attempt in 1..=6 {
                if mode != "missing" {
                    // No watcher time passes during shell startup. The scenario
                    // must reach its real marker before its intended failure is
                    // observed; a missing marker fails instead of being skipped.
                    reached = observed(|| {
                        pids(dir.path()).len() == attempt && ready_pids(dir.path()).len() == attempt
                    })
                    .await;
                    if reached && mode == "silent" {
                        tokio::time::advance(FAST.first_snapshot + FAST.poll).await;
                    }
                }
                reached = reached && observed(|| failures(dir.path()).len() == attempt).await;
                if !reached || attempt == 6 {
                    break;
                }
                let delay = failures(dir.path())[attempt - 1]["retryMs"]
                    .as_u64()
                    .unwrap();
                tokio::time::advance(Duration::from_millis(delay - 1)).await;
                hold_clock(Duration::from_millis(10)).await;
                early_launch |= pids(dir.path()).len()
                    != if mode == "missing" { 0 } else { attempt }
                    || failures(dir.path()).len() != attempt;
                tokio::time::advance(FAST.poll + Duration::from_millis(1)).await;
            }
            let children = task.stop(dir.path()).await;
            tokio::time::resume();
            let records = failures(dir.path());
            let markers = ready_pids(dir.path());
            eprintln!("AFTER scaled mode={mode} reached={reached} launches={} ready_markers={} child_pids={children:?} reaped={} remaining=0 early_launch={early_launch} failures={records:?}", children.len(), markers.len(), children.len());
            assert!(
                reached,
                "{mode} did not reach its intended post-readiness failure"
            );
            assert!(!early_launch, "a pending retry launched before its delay");
            assert_eq!(records.len(), 6);
            assert_eq!(children.len(), if mode == "missing" { 0 } else { 6 });
            assert_eq!(
                markers, children,
                "every ordinary child must reach readiness"
            );
            let delays: Vec<_> = records
                .iter()
                .map(|record| record["retryMs"].as_u64().unwrap())
                .collect();
            assert_eq!(delays, [50, 100, 200, 200, 200, 200]);
            for record in records {
                assert_eq!(record["reason"], reason);
                assert!(!record.to_string().contains("private-payload"));
                assert!(record.to_string().len() < 320);
                match mode {
                    "missing" => assert_eq!(record["errorKind"], "NotFound"),
                    "unterminated" => assert_eq!(record["errorKind"], "UnexpectedEof"),
                    "oversized" => assert_eq!(record["errorKind"], "InvalidData"),
                    _ => {}
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_sustained_valid_snapshot_recovers_and_resets_failure_history() {
        let _guard = WATCH_TEST.lock().await;
        let (dir, state) = helper_fixture(
            r#"
count=$(wc -l < launches | tr -d ' ')
[ "$count" -lt 3 ] && exit 42
echo '{"dialogs":[{"windowId":31507,"owner":"UserNotificationCenter"}]}'
[ "$count" -eq 3 ] && sleep 0.25
exit 42
"#,
        )
        .await;
        start_turn(&state).await;
        tokio::time::pause();
        let task = FixtureTask::start(state, FAST).await;
        let mut reached = true;
        for attempt in 1..=5 {
            reached = observed(|| {
                pids(dir.path()).len() == attempt && ready_pids(dir.path()).len() == attempt
            })
            .await;
            if reached && attempt == 3 {
                reached = observed(|| snapshot()["count"] == 1).await;
                if reached {
                    tokio::time::advance(FAST.healthy_after + FAST.poll).await;
                }
            }
            reached = reached
                && observed(|| {
                    failures(dir.path()).len() == attempt
                        && (snapshot()["count"] == 1) == (attempt >= 3)
                })
                .await;
            if !reached || attempt == 5 {
                break;
            }
            let delay = failures(dir.path())[attempt - 1]["retryMs"]
                .as_u64()
                .unwrap();
            tokio::time::advance(Duration::from_millis(delay) + FAST.poll).await;
        }
        let children = task.stop(dir.path()).await;
        tokio::time::resume();
        let records = failures(dir.path());
        let delays: Vec<_> = records
            .iter()
            .map(|record| record["retryMs"].as_u64().unwrap())
            .collect();
        eprintln!("RECOVERY reached={reached} launches={} retry_ms={delays:?} reaped={} remaining=0 failures={records:?}", children.len(), children.len());
        assert!(
            reached,
            "recovery did not reach its observed snapshot and failures"
        );
        assert_eq!(children.len(), 5);
        assert_eq!(ready_pids(dir.path()), children);
        assert_eq!(delays, [50, 100, 50, 100, 200]);
        assert!(records.iter().all(|record| record["reason"] == "eof"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_quiet_stream_survives_split_lines_and_large_stderr() {
        let _guard = WATCH_TEST.lock().await;
        let (dir, state) = helper_fixture(
            r#"
/usr/bin/awk 'BEGIN { for (i=0; i<20000; i++) print "private-stderr-payload" }' >&2
printf '{"dialogs":'
: > partial-snapshot
while [ ! -e complete-snapshot ]; do sleep 0.01; done
printf '[{"windowId":31507,"owner":"UserNotificationCenter"}]}\n'
exec /bin/cat
"#,
        )
        .await;
        let message = start_turn(&state).await;
        // The native process runs on OS time. Hold watcher time until its
        // partial line exists, then force polling across that incomplete line.
        // Startup load must not turn this healthy-stream fixture into a timeout.
        tokio::time::pause();
        let task = FixtureTask::start(state.clone(), FAST).await;
        let partial = observed(|| dir.path().join("partial-snapshot").exists()).await;
        tokio::time::advance(FAST.poll * 3).await;
        hold_clock(Duration::from_millis(10)).await;
        assert!(snapshot()["count"] == 0);
        std::fs::write(dir.path().join("complete-snapshot"), "").unwrap();
        let opened =
            observed(|| ready_pids(dir.path()).len() == 1 && (snapshot()["count"] == 1)).await;
        if opened {
            tokio::time::advance(FAST.first_snapshot + FAST.poll).await;
            hold_clock(Duration::from_millis(10)).await;
        }
        let quiet_launches = pids(dir.path()).len();
        let records = failures(dir.path());
        // The timed quiet-stream check is complete. Restore real time before
        // SQLite I/O so auto-advance cannot expire its pool deadlines while the
        // OS write is pending and make the watcher briefly infer an idle turn.
        tokio::time::resume();
        state
            .store
            .update_message_delivery(&message, "delivered", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let retained = snapshot()["count"] == 1;
        // Stopping advances the paused stop grace.
        tokio::time::pause();
        let children = task.stop(dir.path()).await;
        assert!(
            partial && opened && retained,
            "quiet stream did not retain its observed dialog after the turn ended"
        );
        assert_eq!(quiet_launches, 1);
        assert_eq!(children.len(), 1);
        assert!(records.is_empty());
        assert!(failures(dir.path()).is_empty());
        eprintln!("QUIET launches=1 split_line_across_polls=true stderr_bytes=460000 failures=0 reaped=1 remaining=0");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn idle_turns_and_abort_release_the_child_and_its_reader() {
        let _guard = WATCH_TEST.lock().await;
        let (dir, state) = helper_fixture("echo '{\"dialogs\":[]}'; exec /bin/sleep 60").await;
        tokio::time::pause();
        let task = FixtureTask::start(state.clone(), FAST).await;
        tokio::time::advance(Duration::from_millis(100)).await;
        hold_clock(Duration::from_millis(10)).await;
        let idle_launches = pids(dir.path()).len();
        let message = observed_io(start_turn(&state)).await;
        // Cross the timer wheel's millisecond rounding boundary as well.
        tokio::time::advance(FAST.poll + Duration::from_millis(1)).await;
        let first_ready =
            observed(|| pids(dir.path()).len() == 1 && ready_pids(dir.path()).len() == 1).await;
        let first = pids(dir.path()).first().copied();
        observed_io(state.store.update_message_delivery(
            &message,
            "delivered",
            Some("thread"),
            Some("turn"),
        ))
        .await
        .unwrap();
        tokio::time::advance(FAST.poll + Duration::from_millis(1)).await;
        let first_stopped = match first {
            Some(pid) => stopped_after_grace(pid).await,
            None => false,
        };
        tokio::time::advance(Duration::from_millis(100)).await;
        hold_clock(Duration::from_millis(10)).await;
        let stopped_launches = pids(dir.path()).len();
        observed_io(state.store.update_message_delivery(
            &message,
            "streaming",
            Some("thread"),
            Some("turn"),
        ))
        .await
        .unwrap();
        tokio::time::advance(FAST.poll + Duration::from_millis(1)).await;
        let second_ready =
            observed(|| pids(dir.path()).len() == 2 && ready_pids(dir.path()).len() == 2).await;
        let second = pids(dir.path()).get(1).copied();
        let children = task.stop(dir.path()).await;
        tokio::time::advance(FAST.retry_max + FAST.poll).await;
        hold_clock(Duration::from_millis(10)).await;
        let after_abort = pids(dir.path()).len();
        // Inspect the owned reader while its stdout read is pending, rather
        // than relying on a helper that cooperates with closed stdin.
        let mut watch = launch(state.computer_use_bin.as_ref().unwrap()).unwrap();
        let snapshot = observed_io(watch.snapshots.recv()).await;
        let reader = watch.reader.abort_handle();
        let third = watch.child.as_ref().unwrap().id().unwrap();
        drop(watch);
        stop_grace_elapses().await;
        let reader_finished = observed(|| reader.is_finished()).await;
        let all_children = children_reaped(dir.path()).await;
        eprintln!("LIFECYCLE ignores_stdin_eof=true idle_launches={idle_launches} stopped_pid={first:?} aborted_pid={second:?} dropped_pid={third} reader_finished={reader_finished} reaped={} remaining=0", all_children.len());
        assert_eq!(idle_launches, 0, "idle must not launch a helper");
        assert!(
            first_ready && first_stopped && second_ready,
            "lifecycle observations did not complete"
        );
        assert_eq!(stopped_launches, 1, "stopped turns must stay idle");
        assert_eq!(children.len(), 2);
        assert_eq!(after_abort, 2, "abort must not relaunch");
        assert!(matches!(
            snapshot,
            Some(Ok(DialogSnapshot { open: false, .. }))
        ));
        assert!(reader_finished);
        assert_eq!(all_children.len(), 3);
    }

    /// Reading computer attention must not activate native observation on an idle
    /// host. The watcher belongs to running work and already-pending dialogs.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn reading_the_dialog_count_does_not_start_an_idle_helper() {
        let _guard = WATCH_TEST.lock().await;
        let (dir, state) = helper_fixture("echo '{\"dialogs\":[]}'; exec /bin/sleep 60").await;
        tokio::time::pause();
        let task = FixtureTask::start(state, FAST).await;
        for _ in 0..3 {
            let _ = observe();
            tokio::time::advance(FAST.poll + Duration::from_millis(1)).await;
            hold_clock(Duration::from_millis(20)).await;
        }
        let launches = owned_pids(dir.path()).len();
        task.stop(dir.path()).await;
        assert_eq!(
            launches, 0,
            "A foreground phone reading the count must not start the native watcher"
        );
    }

    /// A confirmed dialog must expire if the native watcher can no longer
    /// verify it, while retries continue for the still-running turn.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_failed_helper_expires_its_last_dialog_instead_of_waiting_forever() {
        let _guard = WATCH_TEST.lock().await;
        DIALOG_WINDOWS.lock().unwrap().clear();
        let (dir, state) = helper_fixture(
            r#"
[ -e fail ] && exit 42
echo '{"dialogs":[{"windowId":31507,"owner":"UserNotificationCenter"}]}'
while [ ! -e fail ]; do sleep 0.01; done
exit 42
"#,
        )
        .await;
        start_turn(&state).await;
        tokio::time::pause();
        let task = FixtureTask::start(state, FAST).await;
        let opened = observed(|| snapshot()["count"] == 1).await;
        std::fs::write(dir.path().join("fail"), "").unwrap();
        let failed = observed(|| !failures(dir.path()).is_empty()).await;
        tokio::time::advance(Duration::from_secs(60)).await;
        let cleared = observed(|| snapshot()["count"] == 0).await;
        task.stop(dir.path()).await;
        tokio::time::resume();
        assert!(
            opened && failed && cleared,
            "opened={opened} failed={failed} expired={cleared}"
        );
    }

    #[cfg(target_os = "macos")]
    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    /// Real thread-list and push boundaries own the header rule: host attention
    /// reaches the phone, while the running chat never acquires a permission wait.
    // The watcher only runs on macOS (`spawn_with_timing` returns elsewhere).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn an_open_dialog_notifies_the_host_without_claiming_the_running_chat_is_blocked() {
        let _guard = WATCH_TEST.lock().await;
        DIALOG_WINDOWS.lock().unwrap().clear();
        use std::os::unix::fs::PermissionsExt;
        const CONVERSATION: &str = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let close = dir.path().join("close-dialog");
        let helper = dir.path().join("watch-dialogs");
        std::fs::write(dir.path().join("observe-children"), "").unwrap();
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\n[ \"$1\" = --watch-dialogs ] || exit 64\necho '{{\"dialogs\":[]}}'\necho '{}'\nwhile [ ! -e '{}' ]; do sleep 0.1; done\necho '{{\"dialogs\":[]}}'\ncat >/dev/null\n",
                r#"{"dialogs":[{"x":626,"y":189,"windowId":31507,"height":202,"width":260,"owner":"UserNotificationCenter","kind":"permission"}]}"#,
                close.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        state.computer_use_bin = Some(helper);
        state
            .store
            .register_push("owner", "dialogs", "https://push.test", "secret")
            .await
            .unwrap();
        let wonder_store::MessageInsert::Inserted(message) = state
            .store
            .insert_message("owner", "dialog", "work", "hash", CONVERSATION, "0")
            .await
            .unwrap()
        else {
            panic!()
        };
        state
            .store
            .update_message_delivery(&message.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let project = state
            .store
            .project_conversation(CONVERSATION)
            .await
            .unwrap()
            .unwrap()
            .project_id;
        async fn thread(state: &AppState, project: &str) -> serde_json::Value {
            let (_, page) = crate::test_support::call(
                state,
                "GET",
                &format!("/api/v1/projects/{project}/threads"),
                serde_json::Value::Null,
            )
            .await;
            page["threads"]
                .as_array()
                .and_then(|threads| {
                    threads
                        .iter()
                        .find(|thread| thread["conversationId"] == CONVERSATION)
                        .cloned()
                })
                .expect("the running thread is listed")
        }
        let started = std::time::Instant::now();
        let task = spawn(state.clone());
        while snapshot()["count"] != 1 {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "dialog not seen"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let shown = started.elapsed();
        let listed = thread(&state, &project).await;
        assert!(listed.get("waitingOnMacPermission").is_none());
        crate::test_support::assert_http_contract("projectThreadSummary", &listed);
        assert_eq!(snapshot()["count"], 1);
        state.store.distribute_push().await.unwrap();
        let alert = state
            .store
            .next_push()
            .await
            .unwrap()
            .expect("A background phone must still receive a Mac dialog alert");
        assert_eq!(alert.kind, "attention");
        assert_eq!(alert.conversation_id, CONVERSATION);
        assert!(alert
            .source_key
            .starts_with("external:mac-permission:computer:"));
        let preview = crate::push::delivery_preview(&state, &alert)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            preview.conversation_id,
            wonder_store::HOST_VIEW_CONVERSATION_ID
        );
        assert_eq!(preview.body, "1 dialog waiting");
        assert!(preview.approval.is_none() && preview.question.is_none());
        sync_computer_push(&state, &set(&[CONVERSATION, "second-running-thread"])).await;
        state.store.distribute_push().await.unwrap();
        assert!(
            state.store.next_push().await.unwrap().is_none(),
            "A second running thread cannot duplicate the computer alert"
        );
        state
            .store
            .finish_push_claim(&alert, "pending")
            .await
            .unwrap();
        // Ending the model turn does not answer a computer dialog.
        state
            .store
            .update_message_delivery(&message.id, "delivered", Some("thread"), Some("turn"))
            .await
            .unwrap();
        tokio::time::sleep(POLL + Duration::from_millis(50)).await;
        assert_eq!(snapshot()["count"], 1);

        let closed = std::time::Instant::now();
        std::fs::write(&close, "").unwrap();
        while snapshot()["count"] != 0 {
            assert!(
                closed.elapsed() < Duration::from_secs(10),
                "dialog close not seen"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let cleared = closed.elapsed();
        assert!(thread(&state, &project)
            .await
            .get("waitingOnMacPermission")
            .is_none());
        assert_eq!(snapshot()["count"], 0);
        assert!(
            crate::push::delivery_preview(&state, &alert)
                .await
                .unwrap()
                .is_none(),
            "A stale in-flight alert cannot revive a vanished dialog"
        );
        state.store.distribute_push().await.unwrap();
        assert!(
            state.store.next_push().await.unwrap().is_none(),
            "Closing the last dialog withdraws its unsent alert"
        );
        task.abort();
        let _ = task.await;
        #[cfg(target_os = "macos")]
        assert_eq!(children_reaped(dir.path()).await.len(), 1);

        eprintln!("dialog shown after {shown:?}; cleared {cleared:?} after it closed");
    }

    /// Lines as `WonderComputerUse --watch-dialogs` printed them on macOS
    /// 26.6.2 for a `UserNotificationCenter` alert that closed on its own.
    #[test]
    fn reads_the_recorded_helper_lines() {
        let recorded = [
            r#"{"dialogs":[]}"#,
            r#"{"dialogs":[{"x":626,"y":189,"windowId":31507,"height":202,"width":260,"owner":"UserNotificationCenter","kind":"permission"}]}"#,
            r#"{"dialogs":[]}"#,
        ];
        let open: Vec<_> = recorded
            .iter()
            .map(|line| dialog_snapshot(line.as_bytes()).map(|snapshot| snapshot.open))
            .collect();
        assert_eq!(open, vec![Some(false), Some(true), Some(false)]);
        assert!(dialog_snapshot(b"{}").is_none());
        assert!(dialog_snapshot(b"not json").is_none());
        assert!(dialog_snapshot(br#"{"dialogs":[{}]}"#).is_none());
        assert!(dialog_snapshot(
            br#"{"dialogs":[{"windowId":0,"owner":"UserNotificationCenter"}]}"#
        )
        .is_none());
        let snapshot = dialog_snapshot(recorded[1].as_bytes()).unwrap();
        assert_eq!(
            snapshot.windows,
            vec![DialogWindow {
                window_id: 31507,
                owner: "UserNotificationCenter".into(),
                title: None,
                kind: Some("permission".into()),
            }]
        );
        let prefix = "a".repeat(239);
        let long_title = serde_json::json!({"dialogs": [
            {"windowId": 31507, "owner": "UserNotificationCenter", "kind": "permission", "title": format!(" {prefix}é trailing title ")},
            {"windowId": 31508, "owner": "SecurityAgent", "kind": "password"}
        ]});
        let snapshot = dialog_snapshot(&serde_json::to_vec(&long_title).unwrap())
            .expect("A long title must not discard the host's dialog snapshot");
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(
            snapshot.windows[0].title.as_deref(),
            Some(prefix.as_str()),
            "Truncate at a UTF-8 boundary within the phone's 240-byte bound"
        );
    }

    #[test]
    fn duplicate_descriptions_merge_without_losing_exact_window_targets() {
        let parsed = dialog_snapshot(
            br#"{"dialogs":[
            {"windowId":1,"owner":"CuaDriver","kind":"permission"},
            {"windowId":2,"owner":"CuaDriver","kind":"permission"},
            {"windowId":2,"owner":"CuaDriver","kind":"permission"},
            {"windowId":3,"owner":"CuaDriver","kind":"permission"},
            {"windowId":4,"owner":"CuaDriver","title":"Screen recording","kind":"permission"},
            {"windowId":5,"owner":"CuaDriver","title":"Other title","kind":"permission"},
            {"windowId":6,"owner":"CuaDriver","kind":"password"},
            {"windowId":7,"owner":"SecurityAgent","kind":"permission"}
        ]}"#,
        )
        .unwrap();
        assert_eq!(
            parsed
                .windows
                .iter()
                .map(|w| w.window_id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6, 7]
        );
        let notice = snapshot_for(&parsed.windows);
        assert_eq!(notice["count"], 7);
        let groups = notice["dialogs"].as_array().unwrap();
        assert_eq!(groups.len(), 5, "Only the same owner, title and kind merge");
        assert!(groups.contains(&serde_json::json!({"owner":"CuaDriver", "title":null, "kind":"permission", "count":3})),
            "Missing titles remain unknown rather than an invented recording request");
        let mut reordered = parsed.windows.clone();
        reordered.reverse();
        assert_eq!(
            snapshot_for(&reordered),
            notice,
            "Window stacking order is not a new dialog"
        );
    }

    /// A different dialog can replace the old one without changing the badge.
    /// The host event must carry that identity change to a dismissed phone banner.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_different_dialog_with_the_same_count_emits_its_new_description() {
        let _guard = WATCH_TEST.lock().await;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        *COMPUTER_PUSH.lock().await = ComputerPush::default();
        SNAPSHOT_KNOWN.store(true, Ordering::SeqCst);
        let mut events = state.events.subscribe();
        for (id, title) in [(1, "Screen recording"), (2, "Automation")] {
            *DIALOG_WINDOWS.lock().unwrap() = vec![DialogWindow {
                window_id: id,
                owner: "CuaDriver".into(),
                title: Some(title.into()),
                kind: Some("permission".into()),
            }];
            sync_computer_push(&state, &HashSet::new()).await;
            let event = events
                .try_recv()
                .expect("A changed description publishes a host event");
            assert!(event.conversation_id.is_none());
            let wonder_api::WonderEvent::Activity {
                category,
                detail: Some(detail),
                ..
            } = event.event
            else {
                panic!("expected host activity")
            };
            assert_eq!(category, "mac_dialogs");
            let notice: serde_json::Value = serde_json::from_str(&detail).unwrap();
            assert_eq!(notice["count"], 1);
            assert_eq!(notice["dialogs"][0]["title"], title);
            sync_computer_push(&state, &HashSet::new()).await;
            assert!(
                events.try_recv().is_err(),
                "An unchanged snapshot is not another event"
            );
        }
        DIALOG_WINDOWS.lock().unwrap().clear();
    }

    // Uses the app fixtures, which are Unix-only (see lib.rs).
    #[cfg(unix)]
    #[tokio::test]
    async fn computer_count_tracks_distinct_windows_and_taps_follow_the_remaining_dialog() {
        let _guard = WATCH_TEST.lock().await;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let first = DialogWindow {
            window_id: 31_507,
            owner: "UserNotificationCenter".into(),
            title: None,
            kind: None,
        };
        let second = DialogWindow {
            window_id: 31_508,
            owner: "SecurityAgent".into(),
            title: None,
            kind: None,
        };
        let third = DialogWindow {
            window_id: 31_509,
            owner: "CoreServicesUIAgent".into(),
            title: None,
            kind: None,
        };
        let parsed = dialog_snapshot(br#"{"dialogs":[{"windowId":31507,"owner":"UserNotificationCenter"},{"windowId":31507,"owner":"UserNotificationCenter"},{"windowId":31508,"owner":"SecurityAgent"},{"windowId":31509,"owner":"CoreServicesUIAgent"}]}"#).unwrap();
        assert_eq!(
            parsed.windows,
            vec![first.clone(), second.clone(), third.clone()]
        );
        *DIALOG_WINDOWS.lock().unwrap() = parsed.windows;
        assert_eq!(snapshot()["count"], 3);
        let query =
            || Query(serde_json::from_value(serde_json::json!({"macDialogs": true})).unwrap());
        let denied =
            crate::computer_sessions::window_sources(State(state.clone()), None, query()).await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        let device = || {
            Some(Extension(AuthenticatedDevice {
                device_id: "owner".into(),
                session_binding: "fixture".into(),
            }))
        };
        SNAPSHOT_KNOWN.store(false, Ordering::SeqCst);
        let unknown =
            crate::computer_sessions::window_sources(State(state.clone()), device(), query()).await;
        assert_eq!(
            unknown.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "Unknown is never an empty measured snapshot"
        );
        SNAPSHOT_KNOWN.store(true, Ordering::SeqCst);
        let counted =
            crate::computer_sessions::window_sources(State(state.clone()), device(), query()).await;
        assert_eq!(counted.status(), StatusCode::OK);
        let body = axum::body::to_bytes(counted.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["count"],
            3
        );
        let mut events = state.events.subscribe();
        *COMPUTER_PUSH.lock().await = ComputerPush::default();
        sync_computer_push(&state, &HashSet::new()).await;
        assert_eq!(dialog_window(), Some(first.clone()));
        // A Mac-side close and a tap racing the watcher remove only that window.
        forget_window(&state, &first).await;
        assert_eq!(snapshot()["count"], 2);
        assert_eq!(dialog_window(), Some(second.clone()));
        forget_window(&state, &second).await;
        assert_eq!(snapshot()["count"], 1);
        assert_eq!(dialog_window(), Some(third.clone()));
        forget_window(&state, &third).await;
        assert_eq!(snapshot()["count"], 0);
        assert!(dialog_window().is_none());
        clear_gone(&state).await;
        let mut counts = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let wonder_api::WonderEvent::Activity {
                category,
                detail: Some(detail),
                ..
            } = event.event
            {
                if category == "mac_dialogs" {
                    assert!(
                        event.conversation_id.is_none(),
                        "Computer attention is host-scoped"
                    );
                    counts.push(
                        serde_json::from_str::<serde_json::Value>(&detail).unwrap()["count"]
                            .as_u64()
                            .unwrap(),
                    );
                }
            }
        }
        assert_eq!(
            counts,
            [3, 2, 1, 0],
            "Each count change is streamed once, including the last Mac-side close"
        );
    }

    /// The notification must bind capture to the observed ID and never substitute the
    /// whole screen, including when its dialog closes between observation and tap.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn dialog_session_raises_exact_window_and_clears_a_closed_dialog() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = WATCH_TEST.lock().await;
        for (answer, expected) in [
            ("raised", StatusCode::CREATED),
            ("gone", StatusCode::GONE),
            ("unavailable", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let (dir, mut state) = crate::ingestion::tests::fixture().await;
            let helper = dir.path().join("raise-dialog");
            std::fs::write(&helper, format!("#!/bin/sh\n[ \"$1\" = --raise-dialog ] && [ \"$2\" = 31507 ] && [ \"$3\" = UserNotificationCenter ] || exit 64\nprintf '%s\\n' '{{\"state\":\"{answer}\",\"windowId\":31507}}'\n")).unwrap();
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
            state.computer_use_bin = Some(helper);
            state.computer_use_enabled = false; // No capture or live model work in this contract test.
            let conversation = wonder_store::HOST_VIEW_CONVERSATION_ID;
            *DIALOG_WINDOWS.lock().unwrap() = vec![DialogWindow {
                window_id: 31507,
                owner: "UserNotificationCenter".into(),
                title: None,
                kind: None,
            }];
            let request = || {
                let value = serde_json::json!({
                    "clientRequestId":"dialog-tap", "conversationId":conversation,
                    "hostInstallationId":state.host_installation_id,
                    "source":{"id":"mac-dialog","kind":"window","name":"Mac dialog"}
                });
                crate::test_support::assert_http_contract("startComputerSessionRequest", &value);
                serde_json::from_value(value).unwrap()
            };
            let denied =
                crate::computer_sessions::create(State(state.clone()), None, Json(request())).await;
            assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
            let response = crate::computer_sessions::create(
                State(state.clone()),
                Some(Extension(AuthenticatedDevice {
                    device_id: "owner".into(),
                    session_binding: "fixture".into(),
                })),
                Json(request()),
            )
            .await;
            assert_eq!(response.status(), expected, "{answer}");
            if answer == "raised" {
                let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(value["source"]["id"], "app:UserNotificationCenter#31507");
                assert_eq!(value["source"]["kind"], "window");
                assert_eq!(value["control"]["available"], false);
            } else if answer == "gone" {
                assert!(dialog_window().is_none());
                assert!(state
                    .store
                    .active_computer_session_for_host(&state.host_installation_id)
                    .await
                    .unwrap()
                    .is_none());
            } else {
                assert!(
                    dialog_window().is_some(),
                    "A permission failure must remain retryable"
                );
            }
            DIALOG_WINDOWS.lock().unwrap().clear();
        }
    }
}
