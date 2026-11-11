use super::pty::{ProcessIdentity, ProcessState};
use super::*;
use crate::test_support::call;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::sync::Condvar;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as WsMessage};
use tower::ServiceExt;

fn limits() -> TerminalLimits {
    TerminalLimits {
        per_device: 2,
        total: 3,
        scrollback_bytes: 64 * 1024,
        detached_idle: Duration::from_secs(60),
        max_input_frame: 4096,
    }
}

async fn fixture_with(limits: TerminalLimits) -> (tempfile::TempDir, AppState, PathBuf) {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    let state = AppState {
        terminals: Arc::new(TerminalHost::with_shell(limits, PathBuf::from("/bin/sh"))),
        live_activity: Arc::default(),
        ..state
    };
    let folder = dir.path().join("project folder");
    std::fs::create_dir(&folder).unwrap();
    crate::test_support::project_thread(
        &state.store,
        "conv-term",
        "Terminal",
        folder.to_str().unwrap(),
        "codex",
        None,
        None,
        "now",
    )
    .await
    .unwrap();
    let folder = crate::canonical_path::canonicalize(folder).unwrap();
    (dir, state, folder)
}

async fn open(state: &AppState) -> (StatusCode, Value) {
    call(
        state,
        "POST",
        "/api/v1/conversations/conv-term/terminals",
        json!({"cols": 80, "rows": 24}),
    )
    .await
}

async fn serve(state: &AppState) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(
    address: std::net::SocketAddr,
    id: &str,
    capability: Option<&str>,
) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{address}/api/v1/terminals/{id}/socket")
        .into_client_request()
        .unwrap();
    if let Some(capability) = capability {
        request
            .headers_mut()
            .insert("x-wonder-loopback-capability", capability.parse().unwrap());
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(socket, _)| socket)
}

/// Attaches and returns the ready frame.
async fn attach(socket: &mut Client, sequence: Option<u64>) -> Value {
    socket
        .send(WsMessage::Text(
            json!({"sequence": sequence, "cols": 80, "rows": 24})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    match next(socket).await {
        Frame::Control(value) => value,
        other => panic!("expected ready, got {other:?}"),
    }
}

#[derive(Debug)]
enum Frame {
    Output(u64, Vec<u8>),
    Control(Value),
    Closed,
    TimedOut,
}

async fn next(socket: &mut Client) -> Frame {
    loop {
        let Ok(message) = tokio::time::timeout(Duration::from_secs(10), socket.next()).await else {
            return Frame::TimedOut;
        };
        match message {
            Some(Ok(WsMessage::Binary(bytes))) => {
                let sequence = u64::from_be_bytes(bytes[..8].try_into().unwrap());
                return Frame::Output(sequence, bytes[8..].to_vec());
            }
            Some(Ok(WsMessage::Text(text))) => {
                return Frame::Control(serde_json::from_str(&text).unwrap())
            }
            Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => return Frame::Closed,
            Some(Ok(_)) => {}
        }
    }
}

/// Reads output until `needle` appears; returns everything read and the
/// sequence after it. Every frame must continue exactly where the last ended.
async fn read_until(socket: &mut Client, mut expected: u64, needle: &str) -> (String, u64) {
    let mut text = Vec::new();
    loop {
        match next(socket).await {
            Frame::Output(sequence, bytes) => {
                assert_eq!(sequence, expected, "output must not skip or repeat bytes");
                expected += bytes.len() as u64;
                text.extend(bytes);
                if String::from_utf8_lossy(&text).contains(needle) {
                    return (String::from_utf8_lossy(&text).into_owned(), expected);
                }
            }
            Frame::Control(_) => {}
            Frame::Closed | Frame::TimedOut => panic!(
                "closed before {needle:?}: {}",
                String::from_utf8_lossy(&text)
            ),
        }
    }
}

async fn type_line(socket: &mut Client, line: &str) {
    socket
        .send(WsMessage::Binary(format!("{line}\n").into_bytes().into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn shell_starts_in_project_folder_echoes_resizes_and_replays_without_duplicates() {
    let (_dir, state, folder) = fixture_with(limits()).await;
    let (status, body) = open(&state).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    crate::test_support::assert_http_contract("terminalSession", &body);
    assert_eq!(body["title"], "project folder");
    assert_eq!(body["shell"], "sh");
    let id = body["id"].as_str().unwrap().to_owned();
    let address = serve(&state).await;

    let mut socket = connect(address, &id, Some(&state.loopback_capability))
        .await
        .unwrap();
    let ready = attach(&mut socket, None).await;
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["reset"], true);
    let start = ready["sequence"].as_u64().unwrap();
    type_line(&mut socket, "echo marker-$((40+2)); pwd").await;
    let (text, sequence) = read_until(&mut socket, start, folder.to_str().unwrap()).await;
    assert!(text.contains("marker-42"));

    socket
        .send(WsMessage::Text(
            json!({"type": "resize", "cols": 101, "rows": 37})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    type_line(&mut socket, "stty size").await;
    let (_, after_size) = read_until(&mut socket, sequence, "37 101").await;
    drop(socket);

    // Output produced while detached is replayed exactly once on reattach.
    let session = state.terminals.get(&id).unwrap();
    session
        .input
        .send(b"echo detached-$((6*7))\n".to_vec())
        .await
        .unwrap();
    for _ in 0..1200 {
        if String::from_utf8_lossy(&session.output.lock().unwrap().slice(0, usize::MAX))
            .contains("detached-42")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut socket = connect(address, &id, Some(&state.loopback_capability))
        .await
        .unwrap();
    let ready = attach(&mut socket, Some(after_size)).await;
    assert_eq!(ready["reset"], false);
    assert_eq!(ready["sequence"], after_size);
    let (text, _) = read_until(&mut socket, after_size, "detached-42").await;
    assert!(!text.contains("37 101"), "already-held output was resent");

    let (status, list) = call(
        &state,
        "GET",
        "/api/v1/conversations/conv-term/terminals",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    crate::test_support::assert_http_contract("terminalList", &list);
    assert_eq!(list["terminals"][0]["id"], id.as_str());
    assert_eq!(list["terminals"][0]["cols"], 80);

    // Explicit close tells the attached client and ends the socket.
    let (status, _) = call(
        &state,
        "DELETE",
        &format!("/api/v1/terminals/{id}"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let mut saw_closed = false;
    loop {
        match next(&mut socket).await {
            Frame::Control(value) if value["type"] == "closed" => saw_closed = true,
            Frame::Closed => break,
            _ => {}
        }
    }
    assert!(saw_closed);
    let (status, _) = call(
        &state,
        "DELETE",
        &format!("/api/v1/terminals/{id}"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn close_kills_the_whole_process_group() {
    let (_dir, state, _) = fixture_with(limits()).await;
    let (_, body) = open(&state).await;
    let id = body["id"].as_str().unwrap().to_owned();
    let address = serve(&state).await;
    let mut socket = connect(address, &id, Some(&state.loopback_capability))
        .await
        .unwrap();
    let start = attach(&mut socket, None).await["sequence"]
        .as_u64()
        .unwrap();
    type_line(&mut socket, "sleep 600 & echo bg=$((0))$! done").await;
    let (mut text, end) = read_until(&mut socket, start, "bg=0").await;
    if !text.split("bg=0").nth(1).unwrap().contains(" done") {
        text += &read_until(&mut socket, end, " done").await.0;
    }
    let pid: i32 = text
        .split("bg=0")
        .nth(1)
        .unwrap()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap();
    // SAFETY: probing a pid with signal 0.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "background job is running"
    );
    assert!(state.terminals.close(&id));
    let mut gone = false;
    for _ in 0..1200 {
        // SAFETY: as above.
        if unsafe { libc::kill(pid, 0) } != 0 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "background job survived close");
}

/// Only call after fork: fixture policies must never change the shared test
/// process. A nohup launcher can leave HUP ignored, which a noninteractive sh
/// cannot trap; a blocked mask also survives exec.
fn fixture_hup_policy(ignored: bool, blocked: bool) -> std::io::Result<()> {
    // SAFETY: valid local signal structures and async-signal-safe operations.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = if ignored {
            libc::SIG_IGN
        } else {
            libc::SIG_DFL
        };
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGHUP, &action, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut mask = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGHUP);
        let error = libc::pthread_sigmask(
            if blocked {
                libc::SIG_BLOCK
            } else {
                libc::SIG_UNBLOCK
            },
            &mask,
            std::ptr::null_mut(),
        );
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error));
        }
    }
    Ok(())
}

struct FixtureChild(std::process::Child);

impl Drop for FixtureChild {
    fn drop(&mut self) {
        // An assertion failure must not leave a shell (or inherited subprocess
        // output pipes) alive. Only signal our still-unreaped direct child.
        if self.0.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// A stale cleanup record points at a fixture-owned replacement session. No
/// kernel PID churn is needed: the real cleanup owner must reject that record.
#[tokio::test]
async fn stale_session_cleanup_must_not_signal_replacement() {
    use std::os::unix::process::ExitStatusExt;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("hup");
    let ready = dir.path().join("ready");
    let mut command = std::process::Command::new("/bin/sh");
    command.arg("-c").arg(format!(
        "trap 'echo HUP >> {}' HUP; echo ready > {}; while :; do sleep 0.1; done",
        marker.display(),
        ready.display()
    ));
    unsafe {
        command.pre_exec(|| {
            fixture_hup_policy(false, false)?;
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut fixture = FixtureChild(command.spawn().unwrap());
    let replacement = &mut fixture.0;
    for _ in 0..6000 {
        if ready.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ready.exists());
    let (input, _) = tokio::sync::mpsc::channel(1);
    let (changed, _) = tokio::sync::watch::channel(0);
    let stale = Session {
        id: "stale-fixture".into(),
        device_id: "fixture".into(),
        conversation_id: "fixture".into(),
        title: "fixture".into(),
        shell_name: "sh".into(),
        created_at: "fixture".into(),
        processes: Arc::new(ProcessOwner {
            state: StdMutex::new(ProcessState {
                leader: replacement.id() as i32,
                // The old direct child was already reaped; the numeric SID is
                // now that of our replacement. No lifetime proof remains.
                child: None,
                deadline: None,
                hup_sent: false,
                redraw: false,
                result: Some(None),
            }),
            changed: Condvar::new(),
        }),
        pty: pty::Pty(std::fs::File::open("/dev/null").unwrap().into()),
        input,
        output: StdMutex::new(Output {
            ring: VecDeque::new(),
            start: 0,
            exit_code: None,
            exited: false,
            closed: false,
            cols: 80,
            rows: 24,
            viewers: Vec::new(),
            next_viewer: 0,
            last_activity: Instant::now(),
        }),
        capacity: 4096,
        changed,
    };
    let owner = Arc::clone(&stale.processes);
    let worker = std::thread::spawn(move || owner.run());
    stale.redraw();
    stale.terminate();
    stale.terminate();
    stale.processes.force_kill();
    worker.join().unwrap();
    // Keep the direct child unreaped until the real delayed escalation finishes.
    tokio::time::sleep(Duration::from_millis(2400)).await;
    let hup = marker.exists();
    let exit = replacement.try_wait().unwrap();
    eprintln!(
        "stale cleanup: replacement={} HUP_delivered={hup} exit_signal={:?}",
        replacement.id(),
        exit.and_then(|status| status.signal())
    );
    if exit.is_none() {
        replacement.kill().unwrap();
        replacement.wait().unwrap();
    }
    assert!(!hup, "stale cleanup delivered HUP to the replacement");
    assert!(exit.is_none(), "stale cleanup killed the replacement");
}

#[test]
fn inherited_auto_reap_policy_refuses_to_start_a_terminal() {
    const FLAG: &str = "WONDER_TERMINAL_AUTOREAP_FIXTURE";
    if std::env::var_os(FLAG).is_some() {
        // Change process-wide SIGCHLD only in this isolated test subprocess.
        let dir = tempfile::tempdir().unwrap();
        let mut ignored: libc::sigaction = unsafe { std::mem::zeroed() };
        ignored.sa_sigaction = libc::SIG_IGN;
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGCHLD, &ignored, &mut previous) },
            0
        );
        let result = Session::spawn(
            "fixture",
            "fixture",
            dir.path(),
            FsPath::new("/bin/sh"),
            80,
            24,
            4096,
        );
        // Restore before any assertion/drop, including a regressed successful
        // spawn, so the fixture's own child remains safe to clean up.
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGCHLD, &previous, std::ptr::null_mut()) },
            0
        );
        assert!(
            result.is_err(),
            "an auto-reaping parent cannot reserve a terminal SID"
        );
        eprintln!("auto-reap fixture: inherited_SIGCHLD_IGN=true terminal_spawn_refused=true");
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "terminal::tests::inherited_auto_reap_policy_refuses_to_start_a_terminal",
            "--nocapture",
        ])
        .env(FLAG, "1")
        .output()
        .unwrap();
    eprintln!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "isolated auto-reap fixture failed");
}

/// The job was killed. A killed orphan stays a zombie, which `kill(pid, 0)`
/// still finds, until init reaps it; some Linux inits take ~2 s.
fn job_gone(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return true;
    }
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        return stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'));
    }
    false
}

async fn wait_for_fixture_file(path: &FsPath) {
    for _ in 0..6000 {
        if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fixture never created {}", path.display());
}

/// A plain shell exit must reach the client-visible summary promptly, with its
/// actual status, while releasing workers even when replay retains the session.
#[tokio::test]
async fn plain_exit_reports_status_promptly_and_releases_workers() {
    let dir = tempfile::tempdir().unwrap();
    let host = TerminalHost::with_shell(limits(), PathBuf::from("/bin/sh"));
    let mut latencies = Vec::new();
    for code in [5, 0, 37] {
        let session = host
            .open("fixture-device", "fixture", dir.path(), 80, 24)
            .unwrap();
        let owner = Arc::clone(&session.processes);
        let started = Instant::now();
        session
            .input
            .send(format!("exit {code}\n").into_bytes())
            .await
            .unwrap();
        while !session.summary().exited && started.elapsed() < Duration::from_secs(60) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let elapsed = started.elapsed();
        assert!(session.summary().exited, "plain exit was never reported");
        // A PTY EOF arms the two-second cleanup grace; a plain exit must be
        // reported before it runs out instead of waiting for it.
        let grace = owner.state.lock().unwrap().deadline;
        assert!(
            grace.is_none_or(|deadline| Instant::now() < deadline),
            "plain exit waited for background-job grace ({elapsed:?})"
        );
        assert_eq!(session.summary().exit_code, Some(code));
        for _ in 0..6000 {
            if session.input.is_closed() && Arc::strong_count(&owner) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            session.input.is_closed(),
            "plain exit retained input writer"
        );
        assert_eq!(Arc::strong_count(&owner), 2, "plain exit retained workers");
        assert!(owner.state.lock().unwrap().child.is_none());
        eprintln!(
            "plain-exit: code={code} reported_ms={} writer_closed=true worker_refs=0 child_reaped=true",
            elapsed.as_millis()
        );
        assert!(host.close(&session.id));
        assert!(!host.close(&session.id));
        owner.force_kill();
        owner.redraw();
        latencies.push(elapsed);
    }
    eprintln!("plain-exit latencies: {latencies:?}");
}

/// The existing real-PTY cleanup contract also owns independent job groups,
/// HUP resistance, natural leader exit, all close entry points and retirement.
#[tokio::test]
async fn cleanup_owns_jobs_after_leader_exit_and_retires_after_every_close_path() {
    for mode in [
        "close",
        "ignored-hup",
        "revocation",
        "idle",
        "shutdown",
        "close-then-shutdown",
        "drop",
        "leader-exit",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("job-pid");
        let hup_file = dir.path().join("hup");
        let host = TerminalHost::with_shell(limits(), PathBuf::from("/bin/sh"));
        let session = host
            .open("fixture-device", "fixture", dir.path(), 80, 24)
            .unwrap();
        let owner = Arc::clone(&session.processes);
        let leader = owner.state.lock().unwrap().leader;
        // The job records HUP and keeps running. set -m gives it a separate
        // job-control group; its foreground sleep also belongs to the session.
        let handler = if mode == "ignored-hup" {
            String::new()
        } else {
            format!("echo HUP >> {}", hup_file.display())
        };
        let line = format!(
            r#"set -m; /bin/sh -c 'trap "{}" HUP; echo $$ > {}; while :; do sleep 0.1; done' &
"#,
            handler,
            pid_file.display()
        );
        session.input.send(line.into_bytes()).await.unwrap();
        wait_for_fixture_file(&pid_file).await;
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(
            unsafe { libc::getpgid(pid) },
            leader,
            "fixture needs a separate group"
        );
        assert_eq!(unsafe { libc::getsid(pid) }, leader);
        let started = Instant::now();
        if mode == "leader-exit" {
            session.input.send(b"exit 37\n".to_vec()).await.unwrap();
            for _ in 0..6000 {
                if owner
                    .state
                    .lock()
                    .unwrap()
                    .exited_unreaped()
                    .unwrap_or(false)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(owner.state.lock().unwrap().exited_unreaped().unwrap());
            assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "job must outlive leader");
            eprintln!("leader-exit: leader={leader} waitable_zombie=true job={pid} alive=true");
        }
        match mode {
            "close" | "ignored-hup" => {
                assert!(host.close(&session.id));
                assert!(!host.close(&session.id));
            }
            "revocation" => {
                assert_eq!(host.close_device("fixture-device"), 1);
                assert_eq!(host.close_device("fixture-device"), 0);
            }
            "idle" => {
                assert_eq!(host.reap_idle(Instant::now() + Duration::from_secs(61)), 1);
                assert_eq!(host.reap_idle(Instant::now() + Duration::from_secs(61)), 0);
            }
            "shutdown" | "close-then-shutdown" => {
                if mode == "close-then-shutdown" {
                    assert!(host.close(&session.id));
                }
                host.close_all();
                host.close_all();
                if mode == "close-then-shutdown" {
                    // Shutdown reached the closed session's in-flight cleanup
                    // owner: its two-second close grace is cut short to now.
                    assert!(
                        owner
                            .state
                            .lock()
                            .unwrap()
                            .deadline
                            .is_some_and(|deadline| deadline <= Instant::now()),
                        "shutdown lost an in-flight cleanup owner"
                    );
                }
            }
            "drop" | "leader-exit" => {}
            _ => unreachable!(),
        }
        if mode == "ignored-hup" {
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                0,
                "SIG_IGN job must survive HUP"
            );
            eprintln!("cleanup ignored-hup: job={pid} SIG_IGN_HUP=true alive_after_200ms=true");
        } else if mode != "drop" {
            wait_for_fixture_file(&hup_file).await;
            let survived = unsafe { libc::kill(pid, 0) } == 0;
            if !matches!(mode, "shutdown" | "close-then-shutdown") {
                assert!(survived, "HUP-resistant job must survive HUP");
            }
            eprintln!("cleanup {mode}: leader={leader} job={pid} HUP_delivered=true alive_after_close={survived}");
        }
        if mode == "leader-exit" {
            // No explicit close: natural leader exit itself must retire the
            // child and release all three workers while replay stays available.
            for _ in 0..6000 {
                if session.summary().exited
                    && session.input.is_closed()
                    && Arc::strong_count(&owner) == 2
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(session.summary().exit_code, Some(37));
            assert!(session.summary().exited);
            assert!(
                session.input.is_closed(),
                "idle writer survived natural exit"
            );
            assert_eq!(
                Arc::strong_count(&owner),
                2,
                "workers retained exited replay session"
            );
            assert!(host.close(&session.id));
            assert!(!host.close(&session.id));
            eprintln!("retained replay: exit_code=37 writer_closed=true worker_refs=0");
        }
        let weak_session = Arc::downgrade(&session);
        drop(session);
        drop(host);
        let mut gone = false;
        for _ in 0..6000 {
            if job_gone(pid) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "{mode}: background job survived cleanup");
        if mode == "close-then-shutdown" {
            eprintln!(
                "in-flight shutdown: job={pid} elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
        for _ in 0..6000 {
            if owner.state.lock().unwrap().result.is_some() && Arc::strong_count(&owner) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let result = owner.state.lock().unwrap().result;
        assert!(result.is_some(), "{mode}: child was not reaped");
        assert!(owner.state.lock().unwrap().child.is_none());
        assert_eq!(
            Arc::strong_count(&owner),
            1,
            "{mode}: lifecycle/reader retained ownership"
        );
        assert!(weak_session.upgrade().is_none());
        if mode == "leader-exit" {
            assert_eq!(result, Some(Some(37)));
        }
        // A retired owner must not regain permission through repeat cleanup.
        owner.close_after(Duration::ZERO);
        owner.force_kill();
        owner.redraw();
        eprintln!(
            "cleanup {mode}: job={pid} absent=true reaped={result:?} owner_refs=1 elapsed_ms={}",
            started.elapsed().as_millis()
        );
    }
}

/// Exercise the same signal primitive used after session membership validation.
/// A generation mismatch or an exit between capture and delivery must fail shut.
#[tokio::test]
async fn captured_process_identity_rejects_changed_generation_and_exit_before_signal() {
    const FLAG: &str = "WONDER_TERMINAL_IDENTITY_FIXTURE";
    let Some(policy) = std::env::var_os(FLAG) else {
        // Prove the same identity contract under each launcher policy, without
        // leaking process-wide signal changes into concurrently running tests.
        for policy in ["default", "ignored", "blocked"] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "terminal::tests::captured_process_identity_rejects_changed_generation_and_exit_before_signal",
                    "--nocapture",
                ])
                .env(FLAG, policy);
            // SAFETY: changes only this isolated subprocess, before exec.
            unsafe {
                command
                    .pre_exec(move || fixture_hup_policy(policy == "ignored", policy == "blocked"));
            }
            let output = command.output().unwrap();
            eprintln!(
                "launcher HUP policy={policy}:\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.status.success(), "identity fixture failed: {policy}");
        }
        return;
    };
    // Check the regression really entered with the requested inherited state.
    let mut inherited: libc::sigaction = unsafe { std::mem::zeroed() };
    let mut mask = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut inherited) },
        0
    );
    assert_eq!(
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask) },
        0
    );
    let ignored = inherited.sa_sigaction == libc::SIG_IGN;
    let blocked = unsafe { libc::sigismember(&mask, libc::SIGHUP) } == 1;
    assert_eq!(ignored, policy == "ignored");
    assert_eq!(blocked, policy == "blocked");
    eprintln!("identity launcher: HUP_ignored={ignored} HUP_blocked={blocked}");

    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("hup");
    let ready = dir.path().join("ready");
    let mut command = std::process::Command::new("/bin/sh");
    command.arg("-c").arg(format!(
        "trap 'echo HUP >> {}' HUP; echo ready > {}; while :; do sleep 0.1; done",
        marker.display(),
        ready.display()
    ));
    // SAFETY: only the fixture child gets a trappable, unblocked HUP. Preserve
    // the launcher's policy in the test process and unrelated child fixtures.
    unsafe {
        command.pre_exec(|| fixture_hup_policy(false, false));
    }
    let mut fixture = FixtureChild(command.spawn().unwrap());
    let child = &mut fixture.0;
    wait_for_fixture_file(&ready).await;
    let identity = ProcessIdentity::capture(child.id() as i32).unwrap();
    #[cfg(target_os = "macos")]
    {
        let mut changed = ProcessIdentity(identity.0);
        changed.0[7] = changed.0[7].wrapping_add(1);
        let rejected = changed.signal(libc::SIGHUP).unwrap_err();
        assert_eq!(rejected.raw_os_error(), Some(libc::ESRCH));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!marker.exists());
        assert!(child.try_wait().unwrap().is_none());
        eprintln!("identity: pid={} changed_generation_HUP=false", child.id());
    }
    identity.signal(libc::SIGHUP).unwrap();
    wait_for_fixture_file(&marker).await;
    eprintln!("identity: pid={} valid_generation_HUP=true", child.id());
    // The direct Child has not been reaped, so kill cannot target a replacement.
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        identity.signal(libc::SIGKILL).unwrap_err().raw_os_error(),
        Some(libc::ESRCH)
    );
    eprintln!("identity: exit_after_capture=true later_KILL=ESRCH");
}

#[tokio::test]
async fn sessions_are_bounded_per_device_and_in_total() {
    let mut bounds = limits();
    bounds.per_device = 2;
    bounds.total = 2;
    let (_dir, state, folder) = fixture_with(bounds).await;
    assert_eq!(open(&state).await.0, StatusCode::CREATED);
    assert_eq!(open(&state).await.0, StatusCode::CREATED);
    let (status, body) = open(&state).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    // Another device still meets the host-wide bound.
    let other = state
        .terminals
        .open("phone-b", "conv-term", &folder, 80, 24)
        .err()
        .map(|(status, _)| status);
    assert_eq!(other, Some(StatusCode::TOO_MANY_REQUESTS));
    let (status, _) = call(
        &state,
        "POST",
        "/api/v1/conversations/conv-term/terminals",
        json!({"cols": 1, "rows": 24}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    state.terminals.close_all();
}

#[tokio::test]
async fn detached_sessions_close_after_idle_and_revoked_devices_lose_theirs() {
    let (_dir, state, folder) = fixture_with(limits()).await;
    let host = &state.terminals;
    let detached = host.open("phone-a", "conv-term", &folder, 80, 24).unwrap();
    let attached = host.open("phone-b", "conv-term", &folder, 80, 24).unwrap();
    attached.attach(None);
    assert_eq!(host.reap_idle(Instant::now()), 0);
    let later = Instant::now() + Duration::from_secs(61);
    assert_eq!(host.reap_idle(later), 1);
    assert!(host.get(&detached.id).is_none());
    assert!(host.get(&attached.id).is_some());
    assert_eq!(host.close_device("phone-b"), 1);
    assert!(host.get(&attached.id).is_none());
}

#[tokio::test]
async fn sockets_and_routes_require_owner_authentication() {
    let (_dir, state, _) = fixture_with(limits()).await;
    let (_, body) = open(&state).await;
    let id = body["id"].as_str().unwrap().to_owned();
    let address = serve(&state).await;
    // No capability and no session cookie: the shared middleware refuses the upgrade.
    assert!(connect(address, &id, None).await.is_err());
    assert!(connect(address, &id, Some("wrong")).await.is_err());
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/conversations/conv-term/terminals")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"cols":80,"rows":24}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));
    // A paired-device session must still prove itself in the first frame.
    let stale = Attach {
        device_id: Some("phone".into()),
        csrf_token: Some("csrf".into()),
        challenge_id: Some("challenge".into()),
        signature: Some("signature".into()),
        sequence: None,
        cols: None,
        rows: None,
        grid: false,
    };
    assert!(authenticate(&state, &stale, Some("not-a-session"))
        .await
        .is_none());
    assert!(authenticate(&state, &stale, None).await.is_none());
    state.terminals.close_all();
}

/// Reads frames until the socket closes; the close code it ended with.
async fn close_code(socket: &mut Client) -> Option<u16> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
            Ok(Some(Ok(WsMessage::Close(frame)))) => return frame.map(|frame| frame.code.into()),
            Ok(Some(Ok(_))) => {}
            _ => return None,
        }
    }
}

/// A refused sign-in, and access that ends while attached, close with the
/// typed code, so the phone stops reconnecting and says what happened.
#[tokio::test]
async fn refused_sign_in_and_ended_access_close_with_a_typed_reason() {
    let (_dir, state, _) = fixture_with(limits()).await;
    let (_, body) = open(&state).await;
    let id = body["id"].as_str().unwrap().to_owned();

    // An owner-authorized upgrade (as a paired session passes the middleware)
    // whose first frame does not prove this device.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let paired = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/api/v1/terminals/{terminal_id}/socket",
            axum::routing::get(socket),
        )
        .layer(Extension(OwnerAuthority))
        .with_state(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut refused = connect(paired, &id, None).await.unwrap();
    refused
        .send(WsMessage::Text(
            json!({"deviceId": "phone", "csrfToken": "csrf", "challengeId": "c", "signature": "s", "cols": 80, "rows": 24})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(close_code(&mut refused).await, Some(ACCESS_ENDED_CLOSE));

    // Revoking the attached device ends the stream with the same reason.
    let address = serve(&state).await;
    let mut attached = connect(address, &id, Some(&state.loopback_capability))
        .await
        .unwrap();
    attach(&mut attached, None).await;
    state.revocations.send("local".into()).unwrap();
    assert_eq!(close_code(&mut attached).await, Some(ACCESS_ENDED_CLOSE));
    state.terminals.close_all();
}

#[tokio::test]
async fn terminals_open_only_in_verified_project_folders() {
    let (dir, state, folder) = fixture_with(limits()).await;
    let (status, _) = call(
        &state,
        "POST",
        "/api/v1/conversations/not-a-project/terminals",
        json!({"cols": 80, "rows": 24}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &state,
        "POST",
        "/api/v1/conversations/project-files:p:r/terminals",
        json!({"cols": 80, "rows": 24}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A folder Cupola protects is refused even for an existing Project.
    let denied = AppState {
        denied_roots: vec![dir.path().to_string_lossy().into_owned()],
        ..state.clone()
    };
    let (status, body) = open(&denied).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // A folder replaced after the Project was saved is refused too.
    std::fs::remove_dir(&folder).unwrap();
    std::os::unix::fs::symlink("/tmp", &folder).unwrap();
    let (status, body) = open(&state).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(state.terminals.summaries("conv-term").is_empty());
}

#[test]
fn ring_keeps_the_newest_bytes_from_a_line_start() {
    let mut output = Output {
        ring: VecDeque::new(),
        start: 0,
        exit_code: None,
        exited: false,
        closed: false,
        cols: 80,
        rows: 24,
        viewers: Vec::new(),
        next_viewer: 0,
        last_activity: Instant::now(),
    };
    output.append(b"first line\nsecond line\n", 16);
    assert_eq!(output.slice(output.start, usize::MAX), b"second line\n");
    assert_eq!(output.start, 11);
    assert_eq!(output.end(), 23);
    // Without a nearby newline the cut is exact.
    output.append(&[b'x'; 40], 16);
    assert_eq!(output.ring.len(), 16);
    assert_eq!(output.end(), 63);
    assert_eq!(output.slice(60, 2), b"xx");
}

#[test]
fn environment_carries_no_wonder_or_provider_secrets() {
    let env = sanitized_environment(FsPath::new("/bin/zsh"));
    let allowed = [
        "HOME",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "LANG",
        "SHELL",
        "PATH",
        "TERM",
        "COLORTERM",
        "TERM_PROGRAM",
    ];
    for (key, _) in &env {
        assert!(
            allowed.contains(&key.as_str()) || key.starts_with("LC_"),
            "unexpected {key}"
        );
    }
    assert!(env
        .iter()
        .any(|(k, v)| k == "TERM" && v == "xterm-256color"));
}

async fn wait_for_output(session: &Session, bytes: u64) {
    for _ in 0..6000 {
        if session.output.lock().unwrap().end() >= bytes {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the shell never produced {bytes} bytes");
}

/// Closing a terminal whose program is still writing must not leave the shell
/// stuck: the reader releases the PTY before waiting for the shell. The silent
/// drain bound is checked separately at the backend, without a controlling
/// session whose exit can revoke the slave on macOS.
#[tokio::test]
async fn high_output_close_releases_the_terminal_and_ends_the_shell() {
    // A program that ignores HUP ends only when its terminal goes away (or by
    // the kill after the close grace). Releasing the PTY must end it first.
    let dir = tempfile::tempdir().unwrap();
    let host = TerminalHost::with_shell(limits(), PathBuf::from("/bin/sh"));
    let session = host
        .open("fixture-device", "fixture", dir.path(), 80, 24)
        .unwrap();
    let owner = Arc::clone(&session.processes);
    session
        .input
        .send(b"trap '' HUP; yes high-output\n".to_vec())
        .await
        .unwrap();
    wait_for_output(&session, 4 * limits().scrollback_bytes as u64).await;
    let started = Instant::now();
    assert!(host.close(&session.id));
    drop(session);
    for _ in 0..6000 {
        if owner.finished() && Arc::strong_count(&owner) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let result = owner.state.lock().unwrap().result;
    let elapsed = started.elapsed();
    assert!(result.is_some(), "the shell was never reaped");
    assert_ne!(
        result,
        Some(Some(128 + libc::SIGKILL)),
        "the shell stayed stuck writing until the close grace killed it ({elapsed:?})"
    );
    assert_eq!(Arc::strong_count(&owner), 1, "terminal workers retained");
    eprintln!(
        "high-output close: result={result:?} elapsed_ms={}",
        elapsed.as_millis()
    );
}

/// A shell that has not exited by the reap bound stays owned: it is reaped
/// later instead of being left a zombie, and nothing forgets its PID.
#[test]
fn slow_exit_is_reaped_later_not_abandoned() {
    // The child exits only when the test closes its input, so the reap
    // bound has passed while it still runs.
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let release = child.stdin.take().unwrap();
    let pid = child.id();
    assert_eq!(pty::reap_within(child, Duration::ZERO), None);
    assert!(
        pty::late_reaps_pending(pid),
        "slow exit was not kept for reaping"
    );
    #[cfg(target_os = "linux")]
    assert!(
        std::fs::metadata(format!("/proc/{pid}")).is_ok(),
        "PID was released early"
    );
    drop(release);
    let started = Instant::now();
    while pty::late_reaps_pending(pid) && started.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!pty::late_reaps_pending(pid), "slow exit was never reaped");
    // Reaped: no zombie remains under this PID (or it was already reused).
    #[cfg(target_os = "linux")]
    assert!(std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .is_none_or(|stat| !stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))));
    let quick = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 7"])
        .spawn()
        .unwrap();
    assert_eq!(pty::reap_within(quick, Duration::from_secs(10)), Some(7));
}

async fn attach_view(socket: &mut Client, cols: u16, rows: u16, grid: bool) -> Value {
    socket
        .send(WsMessage::Text(
            json!({"cols": cols, "rows": rows, "grid": grid})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    match next(socket).await {
        Frame::Control(value) => value,
        other => panic!("expected ready, got {other:?}"),
    }
}

async fn resize_view(socket: &mut Client, cols: u16, rows: u16) {
    socket
        .send(WsMessage::Text(
            json!({"type": "resize", "cols": cols, "rows": rows})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
}

/// Reads until output contains `needle`; returns the grid frames seen before it.
async fn grids_until(socket: &mut Client, needle: &str) -> Vec<(u64, u64)> {
    let (mut grids, mut text) = (Vec::new(), String::new());
    loop {
        match next(socket).await {
            Frame::Output(_, bytes) => {
                text += &String::from_utf8_lossy(&bytes);
                if text.contains(needle) {
                    return grids;
                }
            }
            Frame::Control(value) if value["type"] == "grid" => grids.push((
                value["cols"].as_u64().unwrap(),
                value["rows"].as_u64().unwrap(),
            )),
            Frame::Control(_) => {}
            other => panic!("closed before {needle:?}: {other:?} {text}"),
        }
    }
}

/// Two differently sized views share one shell grid: the view in use owns it,
/// other views are told that grid to render, and a burst of resizes ends at
/// the final one. Views that predate grid frames never receive them.
#[tokio::test]
async fn views_share_the_owners_grid_instead_of_fighting_over_it() {
    let (_dir, state, _) = fixture_with(limits()).await;
    let (_, body) = open(&state).await;
    let id = body["id"].as_str().unwrap().to_owned();
    let address = serve(&state).await;
    let capability = Some(state.loopback_capability.as_str());
    let mut phone = connect(address, &id, capability).await.unwrap();
    let mut tablet = connect(address, &id, capability).await.unwrap();
    let mut legacy = connect(address, &id, capability).await.unwrap();
    assert_eq!(attach_view(&mut phone, 50, 20, true).await["cols"], 50);
    // Opening a view is its latest use: it owns the grid, and the phone is
    // told to render it.
    let ready = attach_view(&mut tablet, 120, 40, true).await;
    assert_eq!(
        (ready["cols"].as_u64(), ready["rows"].as_u64()),
        (Some(120), Some(40))
    );
    let ready = attach_view(&mut legacy, 90, 30, false).await;
    assert_eq!(ready["cols"], 90);
    // The legacy view attached last and owns the grid until the tablet types.
    type_line(&mut tablet, "stty size; echo tablet-$((1+1))").await;
    assert_eq!(
        grids_until(&mut tablet, "tablet-2").await,
        vec![(90, 30), (120, 40)]
    );
    assert!(grids_until(&mut phone, "tablet-2")
        .await
        .ends_with(&[(120, 40)]));

    // A view that is not in use resizing (rotation, keyboard) changes nothing.
    resize_view(&mut phone, 60, 22).await;
    // Past the settle window, so an applied resize would already show.
    tokio::time::sleep(RESIZE_SETTLE * 6).await;
    type_line(&mut tablet, "stty size; echo still-$((2+1))").await;
    let grids = grids_until(&mut tablet, "still-3").await;
    assert!(
        grids.is_empty(),
        "a passive view resized the shell: {grids:?}"
    );
    let output = session_text(&state, &id);
    assert!(
        output.contains("40 120\r\n") && !output.contains("22 60"),
        "{output}"
    );

    // Typing hands the phone ownership with its latest grid, ahead of its
    // input; the tablet is told before the output written for that grid.
    type_line(&mut phone, "stty size; echo phone-$((3+1))").await;
    assert_eq!(grids_until(&mut phone, "phone-4").await, vec![(60, 22)]);
    assert_eq!(grids_until(&mut tablet, "phone-4").await, vec![(60, 22)]);
    assert!(session_text(&state, &id).contains("22 60\r\n"));

    // A burst of resizes from the owner ends at the final grid.
    for (cols, rows) in [(70, 23), (71, 24), (72, 25)] {
        resize_view(&mut phone, cols, rows).await;
    }
    type_line(&mut phone, "stty size; echo burst-$((4+1))").await;
    assert_eq!(
        grids_until(&mut phone, "burst-5").await.last(),
        Some(&(72, 25))
    );
    assert!(session_text(&state, &id).contains("25 72\r\n"));
    assert_eq!(
        grids_until(&mut legacy, "burst-5").await,
        vec![],
        "legacy view got a grid frame"
    );
    assert_eq!(
        grids_until(&mut tablet, "burst-5").await.last(),
        Some(&(72, 25))
    );

    // When the owner leaves, the most recently used remaining view (the
    // tablet, which typed after the legacy view attached) takes over.
    drop(phone);
    let mut grids = Vec::new();
    for _ in 0..100 {
        if let Frame::Control(value) = next(&mut tablet).await {
            if value["type"] == "grid" {
                grids.push((
                    value["cols"].as_u64().unwrap(),
                    value["rows"].as_u64().unwrap(),
                ));
                break;
            }
        }
    }
    assert_eq!(
        grids,
        vec![(120, 40)],
        "ownership did not pass to the latest view"
    );
}

fn session_text(state: &AppState, id: &str) -> String {
    let session = state.terminals.get(id).unwrap();
    let output = session.output.lock().unwrap();
    String::from_utf8_lossy(&output.slice(0, usize::MAX)).into_owned()
}
