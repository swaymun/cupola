//! Status truth table: does what the phone reads for a Project thread agree
//! with what the harness says, and how fast? Each row drives one Codex or
//! Claude scenario through the host's real loops (runtime transport, inbox
//! projection, the 5 s Project recovery, the 4 s desktop watcher, the HTTP
//! handlers and the push triggers) and samples every surface the phone reads
//! with a real clock. Rows measure today's behavior; they do not assert it.
//!
//! ```sh
//! WONDER_STATUS_TABLE=/tmp/status-truth.json \
//!   cargo test -p wonderd --lib status_truth_table -- --ignored --nocapture
//! ```
//!
//! Writes the JSON there and a Markdown table beside it (`.md`), and prints the
//! table. It takes about two minutes because it waits on the host's own poll
//! intervals, so it is ignored by default; `scripts/nightly-sim` runs it.
//!
//! Fixtures are synthetic. Event shapes follow the pinned Codex schema
//! (`TurnStatus` = completed|interrupted|failed|inProgress), the Claude bridge
//! (services/claude-runtime) and native records sampled once, read-only, for
//! shape and timing: Codex `thread_history_1.sqlite` turn rows (a terminal
//! status can lack `completed_at`; an abandoned turn stays `inProgress`),
//! rollout `task_started`/`task_complete`/`turn_aborted` (reason `interrupted`)
//! events, whose silences inside a turn stayed under five minutes in a sample of
//! 150 rollouts, and Claude Code session records (`status`, `statusUpdatedAt`,
//! `procStart`, `entrypoint`) and transcripts (`stop_reason`). No prompt, reply,
//! path, name, ID or count from those files is used here.
use crate::test_support::call;
use crate::*;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Host surfaces for a Wonder-owned turn: the event is already in the inbox.
const OWNED_MS: u64 = 1_000;
/// Runtime loss: `projects::recover` runs every 5 s (dispatch.rs) + 1 s.
const RECOVERY_MS: u64 = 6_000;
/// Host reads for desktop work are computed per request: 1 s.
const DESKTOP_MS: u64 = 1_000;
/// Desktop push: `external_push::POLL` is 4 s + 1 s.
const EXTERNAL_PUSH_MS: u64 = 5_000;
/// A Mac Claude reply notifies once it reads idle on `external_push::
/// SETTLE_POLLS` (2) polls in a row: 2 x 4 s + 1 s.
const CLAUDE_REPLY_PUSH_MS: u64 = 9_000;
/// Phone constants outside the host table (not measured here).
const PHONE_SIDEBAR_REFRESH_MS: u64 = 30_000; // apps/ios/Wonder/AppShell.swift fallback loop
/// ConnectionModel.swift `sidebarStatusDelay`: a `native_status` event's
/// coalesced sidebar read starts this long after the event arrives.
const PHONE_STATUS_COALESCE_MS: u64 = 300;
const PUSH_SENDER_MS: u64 = 3_000; // push::spawn intent-to-outbox interval
/// Live Activity loop: woken by host events and the desktop watcher, 2 s
/// backstop tick (`live_activity::spawn`), on top of the push budget.
const LIVE_ACTIVITY_MS: u64 = 2_500;

const CHAT: &str = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
const CLAUDE_CHAT: &str = "status-truth-claude";
const CLAUDE_SESSION: &str = "5e55a10a-0000-4000-8000-00000000c1a0";
const INIT_LINE: &str = "    if method == 'initialized': continue\n";
/// Lets a row make the fake App Server print a notification or exit, through
/// an allowed method so the host's own transport carries it.
const CODEX_PROBE: &str = "    probe = r.get('params') or {}
    if isinstance(probe, dict) and probe.get('wonderTestCrash'): sys.exit(1)
    if isinstance(probe, dict) and 'wonderTestEmit' in probe:
        print(json.dumps(probe['wonderTestEmit']), flush=True)
        print(json.dumps({'id':r['id'],'result':{}}), flush=True)
        continue
";
const CLAUDE_BRIDGE: &str = r#"import json, sys, os
root = os.path.dirname(__file__)
def harness():
    try:
        with open(root + '/claude-harness.json') as saved: return json.load(saved)
    except Exception: return {}
for line in sys.stdin:
    r = json.loads(line)
    m = r.get('method')
    if m is None or 'id' not in r: continue
    p = r.get('params') or {}
    if p.get('wonderTestCrash'): sys.exit(1)
    if 'wonderTestEmit' in p:
        print(json.dumps(p['wonderTestEmit']), flush=True)
        print(json.dumps({'id': r['id'], 'result': {}}), flush=True)
        continue
    h = harness(); result = {}
    if m == 'initialize': result = {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}, 'capabilities': {'experimentalApi': True}}
    elif m == 'account/read': result = {'connected': True}
    elif m == 'model/list': result = {'data': [{'id': 'claude:haiku', 'displayName': 'Haiku 4.5'}]}
    elif m == 'thread/list': result = {'data': [{'id': h['thread'], 'sessionId': h['session'], 'status': h['status']}] if h else [], 'nextCursor': None}
    elif m == 'thread/read': result = {'thread': {'id': p.get('threadId'), 'sessionId': h.get('session'), 'status': h.get('status', {'type': 'idle'})}}
    elif m == 'thread/turns/list': result = {'data': h.get('turns', []), 'nextCursor': None}
    elif m in ('thread/backgroundTasks/list', 'project/sessions/list'): result = {'data': [], 'nextCursor': None, 'partial': False}
    print(json.dumps({'id': r['id'], 'result': result}), flush=True)
"#;

// ---------------------------------------------------------------------------
// Surfaces and observation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    /// Sidebar row in the Project's thread list: `isWorking`.
    RowPage,
    /// Pinned row in the global Projects list: `isWorking`.
    RowPinned,
    /// The sidebar row's current step: `currentActivity.type`.
    RowStep,
    /// Pending approval or question for the chat, or the agent waiting on the
    /// Mac (`needsInputOnMac` on its row): the row's needs-you dot.
    NeedsInput,
    /// Conversation snapshot: newest runtime turn status (the header/composer).
    Header,
    /// `history/activity` latestTurnStatus.
    ActivityTurn,
    /// `history/activity` threadStatus.type.
    ActivityThread,
    /// `history/activity` runningElsewhere.
    ActivityElsewhere,
    /// Agent tasks dock: child statuses.
    Subagents,
    /// New push intents for the chat (`completed`/`attention`), else `none`.
    Push,
    /// A `native_status` event for the chat reached the event socket, which
    /// makes the phone read its sidebar rows again.
    SidebarEvent,
    /// The phase the Mac pushes to the phone's Live Activity (`live_activity`).
    LiveActivity,
}

impl Surface {
    fn name(self) -> &'static str {
        match self {
            Self::RowPage => "row.isWorking",
            Self::RowPinned => "pinned.isWorking",
            Self::RowStep => "row.currentStep",
            Self::NeedsInput => "row.needsInput",
            Self::Header => "header.turnStatus",
            Self::ActivityTurn => "activity.latestTurnStatus",
            Self::ActivityThread => "activity.threadStatus",
            Self::ActivityElsewhere => "activity.runningElsewhere",
            Self::Subagents => "subagents.status",
            Self::Push => "push.intent",
            Self::SidebarEvent => "event.nativeStatus",
            Self::LiveActivity => "liveActivity.phase",
        }
    }
}

struct Expect {
    surface: Surface,
    value: Value,
    budget_ms: u64,
}

fn expect(surface: Surface, value: impl Into<Value>, budget_ms: u64) -> Expect {
    Expect {
        surface,
        value: value.into(),
        budget_ms,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Check {
    surface: &'static str,
    expected: Value,
    observed: Value,
    /// When the surface last changed to the observed value, from the native event.
    delay_ms: Option<u64>,
    budget_ms: u64,
    pass: bool,
    /// Every distinct value seen, with when it was first read (ms).
    transitions: Vec<(u64, Value)>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PhaseResult {
    event: String,
    harness: String,
    checks: Vec<Check>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RowResult {
    name: String,
    family: String,
    scenario: String,
    gaps: Vec<u8>,
    native_events: Vec<String>,
    phases: Vec<PhaseResult>,
    pass: bool,
    notes: String,
    error: Option<String>,
    seconds: f64,
}

struct Probe {
    state: AppState,
    project: String,
    conversation: String,
    db: sqlx::SqlitePool,
    push_floor: i64,
    /// The host's event socket feed, as a connected phone receives it.
    events: std::sync::Mutex<tokio::sync::broadcast::Receiver<HostEventEnvelope>>,
    status_event: std::sync::atomic::AtomicBool,
}

impl Probe {
    async fn new(state: &AppState, dir: &FsPath, conversation: &str) -> Self {
        let project = state
            .store
            .project_conversation(conversation)
            .await
            .unwrap()
            .expect("row conversation")
            .project_id;
        let db = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}?mode=ro",
            dir.join("state.db").display()
        ))
        .await
        .unwrap();
        Self {
            state: state.clone(),
            project,
            conversation: conversation.into(),
            db,
            push_floor: 0,
            events: std::sync::Mutex::new(state.events.subscribe()),
            status_event: Default::default(),
        }
    }

    /// Whether a `native_status` event for this chat has arrived since `begin`.
    fn status_event(&self) -> bool {
        use std::sync::atomic::Ordering;
        let mut events = self.events.lock().unwrap();
        loop {
            match events.try_recv() {
                Ok(envelope) => {
                    if envelope.conversation_id.as_deref() == Some(self.conversation.as_str())
                        && matches!(&envelope.event, WonderEvent::Activity { category, .. }
                            if category == "native_status")
                    {
                        self.status_event.store(true, Ordering::SeqCst);
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        self.status_event.load(Ordering::SeqCst)
    }

    /// The moment of the native event: later push intents belong to it.
    async fn begin(&mut self) -> Instant {
        self.status_event();
        self.status_event
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.push_floor = sqlx::query_scalar("SELECT COALESCE(MAX(id),0) FROM push_intents")
            .fetch_one(&self.db)
            .await
            .unwrap();
        Instant::now()
    }

    async fn get(&self, path: &str) -> Result<Value, Value> {
        let (status, body) = call(&self.state, "GET", path, Value::Null).await;
        if status.is_success() {
            Ok(body)
        } else {
            Err(json!(format!("http {}", status.as_u16())))
        }
    }

    /// One reading per surface; each endpoint is read once per round, and a
    /// reading is timed when its response arrived.
    async fn read(&self, surfaces: &[Surface]) -> Vec<(Value, Instant)> {
        let wants = |list: &[Surface]| surfaces.iter().any(|s| list.contains(s));
        let page = if wants(&[Surface::RowPage, Surface::RowStep, Surface::NeedsInput]) {
            let path = format!("/api/v1/projects/{}/threads?limit=30", self.project);
            Some((self.get(&path).await, Instant::now()))
        } else {
            None
        };
        let pinned = if wants(&[Surface::RowPinned]) {
            Some((self.get("/api/v1/projects").await, Instant::now()))
        } else {
            None
        };
        let header = if wants(&[Surface::Header]) {
            let path = format!("/api/v1/conversations/{}", self.conversation);
            Some((self.get(&path).await, Instant::now()))
        } else {
            None
        };
        let activity = if wants(&[
            Surface::ActivityTurn,
            Surface::ActivityThread,
            Surface::ActivityElsewhere,
        ]) {
            let path = format!(
                "/api/v1/conversations/{}/history/activity",
                self.conversation
            );
            Some((self.get(&path).await, Instant::now()))
        } else {
            None
        };
        let mut readings = Vec::new();
        for surface in surfaces {
            let reading = match surface {
                Surface::RowPage | Surface::RowStep => {
                    let (page, at) = page.as_ref().unwrap();
                    let value = match page {
                        Err(error) => error.clone(),
                        Ok(page) => match page["threads"].as_array().and_then(|rows| {
                            rows.iter()
                                .find(|row| row["conversationId"] == self.conversation.as_str())
                        }) {
                            None => json!("missing"),
                            Some(row) if *surface == Surface::RowPage => row["isWorking"].clone(),
                            Some(row) => row
                                .pointer("/currentActivity/type")
                                .cloned()
                                .unwrap_or(Value::Null),
                        },
                    };
                    (value, *at)
                }
                Surface::RowPinned => {
                    let (library, at) = pinned.as_ref().unwrap();
                    let value = match library {
                        Err(error) => error.clone(),
                        Ok(library) => library["pinned"]
                            .as_array()
                            .and_then(|rows| {
                                rows.iter().find(|pin| {
                                    pin["thread"]["conversationId"] == self.conversation.as_str()
                                })
                            })
                            .map(|pin| pin["thread"]["isWorking"].clone())
                            .unwrap_or_else(|| json!("missing")),
                    };
                    (value, *at)
                }
                Surface::Header => {
                    let (snapshot, at) = header.as_ref().unwrap();
                    let value = match snapshot {
                        Err(error) => error.clone(),
                        Ok(snapshot) => header_status(snapshot),
                    };
                    (value, *at)
                }
                Surface::ActivityTurn | Surface::ActivityThread | Surface::ActivityElsewhere => {
                    let (activity, at) = activity.as_ref().unwrap();
                    let value = match activity {
                        Err(error) => error.clone(),
                        Ok(activity) => match surface {
                            Surface::ActivityTurn => activity["latestTurnStatus"].clone(),
                            Surface::ActivityThread => activity
                                .pointer("/threadStatus/type")
                                .cloned()
                                .unwrap_or(Value::Null),
                            _ => activity["runningElsewhere"].clone(),
                        },
                    };
                    (value, *at)
                }
                Surface::NeedsInput => {
                    let on_mac = page.as_ref().and_then(|(page, _)| {
                        page.as_ref().ok()?["threads"]
                            .as_array()?
                            .iter()
                            .find(|row| row["conversationId"] == self.conversation.as_str())
                            .map(|row| row["needsInputOnMac"] == true)
                    });
                    let wonder = self.needs_input().await;
                    let value = match (wonder, on_mac) {
                        (Value::Bool(wonder), Some(on_mac)) => json!(wonder || on_mac),
                        (Value::Bool(_), None) => json!("missing"),
                        (error, _) => error,
                    };
                    (value, Instant::now())
                }
                Surface::SidebarEvent => (json!(self.status_event()), Instant::now()),
                Surface::Subagents => {
                    let path = format!(
                        "/api/v1/project-conversations/{}/subagents",
                        self.conversation
                    );
                    let value = match self.get(&path).await {
                        Err(error) => error,
                        Ok(list) => {
                            let statuses = list["subagents"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|task| task["status"].as_str())
                                .collect::<Vec<_>>();
                            json!(if statuses.is_empty() {
                                "none".to_owned()
                            } else {
                                statuses.join(",")
                            })
                        }
                    };
                    (value, Instant::now())
                }
                Surface::Push => (self.push().await, Instant::now()),
                Surface::LiveActivity => (
                    self.state
                        .live_activity
                        .current()
                        .map(|aggregate| json!(aggregate.phase))
                        .unwrap_or_else(|| json!("none")),
                    Instant::now(),
                ),
            };
            readings.push(reading);
        }
        readings
    }

    async fn needs_input(&self) -> Value {
        let approvals = match self.get("/api/v1/approvals").await {
            Ok(list) => list,
            Err(error) => return error,
        };
        let approval = approvals
            .as_array()
            .into_iter()
            .flatten()
            .any(|approval| approval["conversationId"] == self.conversation.as_str());
        let path = format!("/api/v1/conversations/{}/questions", self.conversation);
        let question = match self.get(&path).await {
            Ok(list) => list.as_array().into_iter().flatten().any(|question| {
                !matches!(
                    question["state"].as_str(),
                    Some("answered" | "skipped" | "expired" | "cancelled")
                )
            }),
            Err(error) => return error,
        };
        json!(approval || question)
    }

    async fn push(&self) -> Value {
        let kinds: Vec<String> = sqlx::query_scalar(
            "SELECT kind FROM push_intents WHERE id>? AND (conversation_id=? \
             OR (conversation_id='' AND source_key LIKE 'approval:%')) ORDER BY id",
        )
        .bind(self.push_floor)
        .bind(&self.conversation)
        .fetch_all(&self.db)
        .await
        .unwrap();
        json!(if kinds.is_empty() {
            "none".to_owned()
        } else {
            kinds.join(",")
        })
    }

    /// Samples until every surface shows its expected value for at least the
    /// largest budget, or until `window_ms` has passed.
    async fn observe(
        &self,
        started: Instant,
        event: &str,
        harness: &str,
        expects: Vec<Expect>,
        window_ms: u64,
    ) -> PhaseResult {
        let surfaces: Vec<Surface> = expects.iter().map(|e| e.surface).collect();
        let settle = expects.iter().map(|e| e.budget_ms).max().unwrap_or(0);
        let mut timelines: Vec<Vec<(u64, Value)>> = vec![Vec::new(); expects.len()];
        loop {
            for (timeline, (value, at)) in timelines.iter_mut().zip(self.read(&surfaces).await) {
                let ms = at.saturating_duration_since(started).as_millis() as u64;
                if timeline.last().map(|(_, last)| last) != Some(&value) {
                    timeline.push((ms, value));
                }
            }
            let elapsed = started.elapsed().as_millis() as u64;
            let settled = timelines
                .iter()
                .zip(&expects)
                .all(|(timeline, e)| timeline.last().map(|(_, v)| v) == Some(&e.value));
            if elapsed >= window_ms || (settled && elapsed >= settle) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let checks = expects
            .into_iter()
            .zip(timelines)
            .map(|(e, timeline)| {
                let (at, observed) = timeline.last().cloned().unwrap_or((0, Value::Null));
                let reached = observed == e.value;
                Check {
                    surface: e.surface.name(),
                    pass: reached && at <= e.budget_ms,
                    delay_ms: reached.then_some(at),
                    expected: e.value,
                    observed,
                    budget_ms: e.budget_ms,
                    transitions: timeline,
                }
            })
            .collect();
        PhaseResult {
            event: event.into(),
            harness: harness.into(),
            checks,
        }
    }
}

/// The phone's header: the newest runtime turn's status, or `pending` while
/// a message waits for a turn (ConversationSnapshot.activeTurnID and
/// hasUnassignedPreTurnWork).
fn header_status(snapshot: &Value) -> Value {
    let pending = snapshot["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|message| {
            message["codexTurnId"].as_str().is_none_or(str::is_empty)
                && matches!(
                    message["state"].as_str(),
                    Some("accepted_by_wonder" | "dispatching_to_codex")
                )
        });
    let newest = snapshot["thread"]["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|turn| {
            !turn["id"]
                .as_str()
                .unwrap_or_default()
                .starts_with("local:")
        });
    match newest {
        Some(turn) if turn["status"] == "inProgress" || !pending => turn["status"].clone(),
        _ if pending => json!("pending"),
        _ => json!("none"),
    }
}

// ---------------------------------------------------------------------------
// Rigs: the host's loops around a fake Codex App Server or Claude bridge
// ---------------------------------------------------------------------------

struct Rig {
    dir: tempfile::TempDir,
    state: AppState,
    service: Option<crate::ingestion::NotificationService>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    children: Vec<std::process::Child>,
    /// The Codex chat's working folder, as Codex reports it back.
    cwd: String,
}

impl Rig {
    async fn new() -> Self {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let file = dir.path().join("runtime.py");
        let source = std::fs::read_to_string(&file).unwrap();
        assert!(source.contains(INIT_LINE), "fake App Server changed");
        std::fs::write(
            &file,
            source.replace(INIT_LINE, &format!("{INIT_LINE}{CODEX_PROBE}")),
        )
        .unwrap();
        let config = state.projects.config.lock().unwrap().clone();
        state
            .projects
            .codex
            .lock()
            .await
            .restart(config)
            .await
            .unwrap();
        state
            .store
            .register_push("owner", "status-truth", "http://127.0.0.1:9", "secret")
            .await
            .unwrap();
        // The Live Activity loop aggregates only while a phone is registered.
        // Its sender refuses at once, so reports retry with backoff.
        let cwd = state
            .store
            .project_conversation(CHAT)
            .await
            .unwrap()
            .unwrap()
            .cwd;
        Self {
            dir,
            state,
            service: None,
            tasks: Vec::new(),
            children: Vec::new(),
            cwd,
        }
    }

    fn path(&self) -> &FsPath {
        self.dir.path()
    }

    /// The daemon's background loops: inbox projection with runtime recovery,
    /// dispatch with the 5 s Project recovery, and optionally the desktop watcher.
    async fn start(&mut self, desktop_watcher: bool) {
        self.service = Some(crate::ingestion::spawn(self.state.clone()).await);
        self.tasks
            .push(crate::dispatch::spawn(self.state.clone()).await.unwrap());
        self.tasks
            .push(crate::live_activity::spawn(self.state.clone()));
        if desktop_watcher {
            self.tasks
                .push(crate::external_push::spawn(self.state.clone()));
        }
        crate::ingestion::tests::ready(&self.state).await;
    }

    async fn stop(&mut self) {
        self.service = None;
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }

    async fn shutdown(mut self) {
        self.stop().await;
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.state.projects.shutdown().await;
        let _ = self.state.projects.codex.lock().await.shutdown().await;
        if let Some(claude) = &self.state.claude {
            let _ = claude.client.lock().await.shutdown().await;
        }
    }

    fn write(&self, name: &str, value: Value) {
        std::fs::write(self.path().join(name), value.to_string()).unwrap();
    }

    /// What the fake App Server reports for the Cupola chat's native thread.
    fn codex_native(&self, mut thread: Value, turns: Value) {
        thread["cwd"] = json!(self.cwd);
        self.write("idle-fixture.json", json!({ "thread": thread }));
        self.write("turn-pages.json", json!([turns]));
    }

    /// The parent thread and one spawned agent thread that keeps running.
    fn codex_family(&self, parent: &str, turn: Value) {
        self.write(
            "project-subagents-fixture.json",
            json!({
                "threads": {
                    "thread": {"id":"thread","cwd": self.cwd,"parentThreadId":null,"status":{"type":parent}},
                    "child-thread": {"id":"child-thread","parentThreadId":"thread","cwd": self.cwd,
                        "source":{"subAgent":{"thread_spawn":{"parent_thread_id":"thread","depth":1,"agent_nickname":"Scout"}}},
                        "status":{"type":"active"}}
                },
                "turns": {"thread":[turn], "child-thread":[{"id":"child-turn","status":"inProgress"}]}
            }),
        );
    }

    /// Prints a notification or server request from the Codex runtime.
    async fn codex_emit(&self, message: Value) {
        let rpc = crate::projects::codex_rpc(&self.state).await.unwrap();
        let response = rpc
            .request(
                "thread/read",
                json!({"threadId": "status-truth", "wonderTestEmit": message}),
            )
            .await
            .unwrap();
        assert!(response.error.is_none());
    }

    async fn codex_crash(&self) {
        let rpc = self.state.projects.codex.lock().await.rpc();
        let _ = tokio::time::timeout(
            Duration::from_secs(60),
            rpc.request(
                "thread/read",
                json!({"threadId": "status-truth", "wonderTestCrash": true}),
            ),
        )
        .await;
    }

    async fn pin(&self, conversation: &str) {
        self.state
            .store
            .update_project_conversation(
                conversation,
                wonder_store::ProjectConversationPatch {
                    pinned: Some(true),
                    ..Default::default()
                },
                &crate::projects::now_text(),
            )
            .await
            .unwrap()
            .unwrap();
    }

    async fn last_used(&self, conversation: &str, ago: Duration) {
        let at = chrono::Utc::now() - chrono::Duration::from_std(ago).unwrap();
        let db = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            self.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query("UPDATE project_conversations SET last_activity_at=? WHERE conversation_id=?")
            .bind(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .bind(conversation)
            .execute(&db)
            .await
            .unwrap();
    }

    /// A Wonder-owned Codex turn the runtime accepted and is running.
    async fn codex_owned_turn(&self) -> wonder_store::StoredMessage {
        crate::test_support::bind_project_thread(
            &self.state,
            CHAT,
            AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        self.pin(CHAT).await;
        self.codex_native(
            json!({"id":"thread","status":{"type":"active","activeFlags":[]}}),
            json!([{"id":"turn","status":"inProgress","items":[]}]),
        );
        let message = self
            .message(CHAT, "status-truth-receipt", "thread", "turn")
            .await;
        // Projected when the daemon's loops start, as after a real dispatch.
        self.codex_emit(
            json!({"method":"turn/started","params":{"threadId":"thread",
            "turn":{"id":"turn","status":"inProgress","items":[]}}}),
        )
        .await;
        let runtime = crate::projects::codex_rpc(&self.state)
            .await
            .unwrap()
            .health()
            .id()
            .to_owned();
        self.state
            .projects
            .record_turn_generation(&message.id, &runtime);
        message
    }

    async fn message(
        &self,
        conversation: &str,
        receipt: &str,
        thread: &str,
        turn: &str,
    ) -> wonder_store::StoredMessage {
        let wonder_store::MessageInsert::Inserted(message) = self
            .state
            .store
            .insert_message(
                "owner",
                receipt,
                "Synthetic prompt",
                "hash",
                conversation,
                "now",
            )
            .await
            .unwrap()
        else {
            panic!("new receipt")
        };
        self.state
            .store
            .update_message_delivery(&message.id, "accepted_by_codex", Some(thread), Some(turn))
            .await
            .unwrap();
        // The event dispatch publishes when the runtime accepts the turn.
        crate::publish_message_state(
            &self.state,
            &message,
            DeliveryState::AcceptedByCodex,
            Some(thread),
            Some(turn),
        )
        .await;
        message
    }

    /// A fake Claude bridge with a Wonder-owned running reply.
    async fn claude_owned_turn(&mut self) -> wonder_store::StoredMessage {
        self.claude_harness(
            "active",
            json!([{"id":"claude-turn","status":"inProgress"}]),
        );
        self.claude_bridge().await;
        let cwd = self.state.data_dir.to_str().unwrap().to_owned();
        self.bind_claude(&cwd).await;
        self.claude_turn_started().await
    }

    /// Cupola's (fake) Claude bridge, connected like the real runtime.
    async fn claude_bridge(&mut self) {
        let entrypoint = self.path().join("claude-bridge.py");
        std::fs::write(&entrypoint, CLAUDE_BRIDGE).unwrap();
        let config = wonder_app_server::BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: self.path().join("claude-state"),
            npm_cli: None,
            claude_config_dir: None,
            wonder_version: "test".into(),
            family: "claude",
        };
        let client = Arc::new(tokio::sync::Mutex::new(
            wonder_app_server::AppServerClient::unavailable(
                "test".into(),
                crate::ingestion::notification_sink(self.state.store.clone()),
            ),
        ));
        client
            .lock()
            .await
            .restart_bridge(config.clone())
            .await
            .unwrap();
        crate::claude::discover_family(&self.state, &mut *client.lock().await, AgentFamily::Claude)
            .await
            .unwrap();
        self.state.claude = Some(Arc::new(crate::claude::Runtime { client, config }));
    }

    /// A phone message the bridge accepted as `claude-turn` and started.
    async fn claude_turn_started(&mut self) -> wonder_store::StoredMessage {
        let message = self
            .message(
                CLAUDE_CHAT,
                "status-truth-claude",
                &claude_thread(),
                "claude-turn",
            )
            .await;
        self.claude_emit(
            json!({"method":"turn/started","params":{"threadId": claude_thread(),
            "turn":{"id":"claude-turn","status":"inProgress","items":[]},"agentFamily":"claude"}}),
        )
        .await;
        let runtime = self
            .state
            .claude
            .as_ref()
            .unwrap()
            .client
            .lock()
            .await
            .health()
            .id()
            .to_owned();
        self.state
            .projects
            .record_turn_generation(&message.id, &runtime);
        message
    }

    /// A pinned Claude chat bound like a real one: the bare session as its
    /// native ID, the bridge thread as its runtime binding.
    async fn bind_claude(&self, cwd: &str) {
        crate::test_support::project_thread(
            &self.state.store,
            CLAUDE_CHAT,
            "Synthetic Claude thread",
            cwd,
            crate::providers::provider_store(&self.state, AgentFamily::Claude),
            Some("claude:haiku"),
            None,
            "now",
        )
        .await
        .unwrap();
        self.state
            .store
            .set_project_native_session(CLAUDE_CHAT, CLAUDE_SESSION, "now")
            .await
            .unwrap();
        self.state
            .store
            .bind_project_runtime(
                CLAUDE_CHAT,
                AgentFamily::Claude,
                crate::providers::provider_store(&self.state, AgentFamily::Claude),
                &claude_thread(),
                Some(CLAUDE_SESSION),
                "now",
            )
            .await
            .unwrap();
        self.pin(CLAUDE_CHAT).await;
    }

    fn claude_harness(&self, status: &str, turns: Value) {
        self.write(
            "claude-harness.json",
            json!({"thread": claude_thread(), "session": CLAUDE_SESSION,
                "status": {"type": status}, "turns": turns}),
        );
    }

    async fn claude_emit(&self, message: Value) {
        let rpc = self
            .state
            .claude
            .as_ref()
            .unwrap()
            .client
            .lock()
            .await
            .rpc();
        let response = rpc
            .request(
                "thread/read",
                json!({"threadId": claude_thread(), "wonderTestEmit": message}),
            )
            .await
            .unwrap();
        assert!(response.error.is_none());
    }

    async fn claude_crash(&self) {
        let rpc = self
            .state
            .claude
            .as_ref()
            .unwrap()
            .client
            .lock()
            .await
            .rpc();
        let _ = tokio::time::timeout(
            Duration::from_secs(60),
            rpc.request(
                "thread/read",
                json!({"threadId": claude_thread(), "wonderTestCrash": true}),
            ),
        )
        .await;
    }

    async fn interrupt(&self, conversation: &str, turn: &str) -> StatusCode {
        call(
            &self.state,
            "POST",
            &format!("/api/v1/conversations/{conversation}/turns/{turn}/interrupt"),
            json!({}),
        )
        .await
        .0
    }
}

fn claude_thread() -> String {
    format!("claude-{CLAUDE_SESSION}")
}

fn now_seconds() -> i64 {
    chrono::Utc::now().timestamp()
}

fn codex_turn(id: &str, status: &str, terminal: bool) -> Value {
    if terminal {
        json!({"id": id, "status": status, "items": [], "completedAt": now_seconds()})
    } else {
        json!({"id": id, "status": status, "items": []})
    }
}

fn codex_completed(turn: &str, status: &str) -> Value {
    json!({"method":"turn/completed","params":{"threadId":"thread",
        "turn":{"id": turn, "status": status, "items": []}}})
}

fn claude_completed(status: &str) -> Value {
    json!({"method":"turn/completed","params":{"threadId": claude_thread(), "turnId":"claude-turn",
        "turn":{"id":"claude-turn","status": status, "items": []}, "agentFamily":"claude"}})
}

// ---------------------------------------------------------------------------
// Codex's indexed lifecycle (a desktop app's turn) and Claude Code records
// ---------------------------------------------------------------------------

struct CodexIndex {
    db: sqlx::SqliteConnection,
    rollout: PathBuf,
    ordinal: i64,
    /// Codex keeps a loaded thread's rollout open for appending; this test
    /// process stands in for that live writer.
    _writer: std::fs::File,
}

impl CodexIndex {
    async fn new(home: &FsPath) -> Self {
        use sqlx::{Connection, Executor};
        let home = crate::canonical_path::canonicalize(home).unwrap();
        let day = home.join("sessions/2026/10/14");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-2026-10-14T00-00-00-thread.jsonl");
        std::fs::write(
            &rollout,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\"}}\n",
        )
        .unwrap();
        let mut db = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(home.join("thread_history_1.sqlite"))
                .create_if_missing(true),
        )
        .await
        .unwrap();
        db.execute("CREATE TABLE _sqlx_migrations(version INTEGER, success INTEGER);
            INSERT INTO _sqlx_migrations VALUES(1,1),(2,1),(3,1),(4,1),(5,1),(6,1),(7,1),(8,1);
            CREATE TABLE thread_turns(thread_id TEXT, turn_id TEXT, rollout_ordinal INTEGER, status TEXT,
                completed_at INTEGER, rollout_byte_offset INTEGER, rollout_end_byte_offset INTEGER, final_agent_item_id TEXT,
                PRIMARY KEY(thread_id,turn_id));
            CREATE UNIQUE INDEX idx_thread_turns_page ON thread_turns(thread_id,rollout_ordinal);
            CREATE TABLE thread_history_projection_state(thread_id TEXT PRIMARY KEY,next_rollout_byte_offset INTEGER);
            CREATE TABLE thread_items(thread_id TEXT,turn_id TEXT,item_id TEXT,item_json TEXT,PRIMARY KEY(thread_id,turn_id,item_id));").await.unwrap();
        let writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        let mut index = Self {
            db,
            rollout,
            ordinal: 0,
            _writer: writer,
        };
        sqlx::query("INSERT INTO thread_history_projection_state VALUES('thread',0)")
            .execute(&mut index.db)
            .await
            .unwrap();
        index.indexed().await;
        index
    }

    fn append(&self, value: Value) -> i64 {
        use std::io::Write;
        let offset = std::fs::metadata(&self.rollout).unwrap().len() as i64;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.rollout)
            .unwrap();
        writeln!(file, "{value}").unwrap();
        offset
    }

    async fn indexed(&mut self) {
        let length = std::fs::metadata(&self.rollout).unwrap().len() as i64;
        sqlx::query("UPDATE thread_history_projection_state SET next_rollout_byte_offset=?")
            .bind(length)
            .execute(&mut self.db)
            .await
            .unwrap();
    }

    async fn start(&mut self, turn: &str) {
        let offset = self.append(json!({"timestamp": chrono::Utc::now().to_rfc3339(),
            "type":"event_msg","payload":{"type":"task_started","turn_id":turn}}));
        self.ordinal += 1;
        sqlx::query("INSERT INTO thread_turns VALUES('thread',?,?,'inProgress',NULL,?,NULL,NULL)")
            .bind(turn)
            .bind(self.ordinal)
            .bind(offset)
            .execute(&mut self.db)
            .await
            .unwrap();
        self.indexed().await;
    }

    async fn finish(&mut self, turn: &str, status: &str) {
        let event = match status {
            "completed" => {
                json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"last_agent_message":"Synthetic reply."}})
            }
            "interrupted" => {
                json!({"type":"event_msg","payload":{"type":"turn_aborted","turn_id":turn,"reason":"interrupted"}})
            }
            _ => {
                json!({"type":"event_msg","payload":{"type":"error","message":"Synthetic failure"}})
            }
        };
        let end = self.append(event);
        sqlx::query("INSERT OR REPLACE INTO thread_items VALUES('thread',?,'reply',?)")
            .bind(turn)
            .bind(json!({"type":"agentMessage","text":"Synthetic reply."}).to_string())
            .execute(&mut self.db)
            .await
            .unwrap();
        sqlx::query("UPDATE thread_turns SET status=?, completed_at=?, rollout_end_byte_offset=?, final_agent_item_id='reply' WHERE turn_id=?")
            .bind(status)
            // Current desktop projection can omit the time while retaining
            // the exact terminal rollout position and turn identity.
            .bind((status == "failed").then(now_seconds))
            .bind(end)
            .bind(turn)
            .execute(&mut self.db)
            .await
            .unwrap();
        self.indexed().await;
    }

    fn silent_for(&self, quiet: Duration) {
        std::fs::File::options()
            .write(true)
            .open(&self.rollout)
            .unwrap()
            .set_modified(SystemTime::now() - quiet)
            .unwrap();
    }
}

impl Rig {
    /// The Cupola chat bound to a Codex thread the desktop app runs. Cupola's
    /// App Server has not loaded it (`notLoaded`); Codex lists it by recency.
    async fn codex_desktop(&self) -> CodexIndex {
        crate::test_support::bind_project_thread(
            &self.state,
            CHAT,
            AgentFamily::Codex,
            "thread",
            None,
            "now",
        )
        .await
        .unwrap();
        let index = CodexIndex::new(&self.state.codex_home).await;
        std::fs::write(self.path().join("archive-fixture"), "").unwrap();
        self.codex_desktop_native(json!([]), &index);
        index
    }

    fn codex_desktop_native(&self, turns: Value, index: &CodexIndex) {
        self.codex_native(
            json!({"id":"thread","name":"Synthetic desktop thread","updatedAt": now_seconds(),
                "status":{"type":"notLoaded"},"path": index.rollout}),
            turns,
        );
    }

    /// A pinned Claude chat whose session Claude Code on the Mac has open.
    async fn claude_desktop(&mut self) -> ClaudeDesktop {
        let config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap();
        let _ = std::fs::remove_dir_all(&config);
        let work = self.path().join("claude-work");
        std::fs::create_dir_all(&work).unwrap();
        let cwd = crate::canonical_path::canonicalize(&work).unwrap();
        let cwd = cwd.to_str().unwrap().to_owned();
        self.bind_claude(&cwd).await;
        let child = std::process::Command::new("/bin/sleep")
            .arg("600")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        self.children.push(child);
        let folder: String = cwd
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let transcripts = config.join("projects").join(folder);
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::create_dir_all(config.join("sessions")).unwrap();
        let desktop = ClaudeDesktop {
            record: config.join(format!("sessions/{pid}.json")),
            transcript: transcripts.join(format!("{CLAUDE_SESSION}.jsonl")),
            pid,
            cwd,
        };
        // A followed session already has a transcript. The prompt watcher
        // reads it forward from its end, so prompts written later are seen.
        desktop.transcript(&[]);
        desktop
    }
}

struct ClaudeDesktop {
    record: PathBuf,
    transcript: PathBuf,
    pid: i32,
    cwd: String,
}

impl ClaudeDesktop {
    /// Claude Code's own session record, as the desktop app writes it.
    fn status(&self, status: &str) {
        let record = json!({"pid": self.pid, "sessionId": CLAUDE_SESSION, "cwd": self.cwd,
            "startedAt": 1, "procStart": crate::desktop_activity::record_start_label(self.pid),
            "kind": "interactive", "entrypoint": "claude-desktop", "status": status,
            "statusUpdatedAt": chrono::Utc::now().timestamp_millis(),
            "updatedAt": chrono::Utc::now().timestamp_millis()});
        let temporary = self.record.with_extension("tmp");
        std::fs::write(&temporary, record.to_string()).unwrap();
        std::fs::rename(temporary, &self.record).unwrap();
    }

    fn transcript(&self, lines: &[Value]) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.transcript)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn prompt(&self) {
        self.transcript(&[
            json!({"type":"user","message":{"role":"user","content":"Synthetic prompt"}}),
        ]);
    }

    fn reply(&self) {
        self.transcript(&[json!({"type":"assistant","message":{"role":"assistant",
            "content":[{"type":"text","text":"Synthetic reply."}],"stop_reason":"end_turn"}})]);
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// The Live Activity's end state for a desktop Codex turn's status.
fn live_phase(status: &str) -> &'static str {
    match status {
        "completed" => "done",
        "interrupted" => "stopped",
        _ => "failed",
    }
}

struct Row {
    name: &'static str,
    family: &'static str,
    scenario: &'static str,
    gaps: &'static [u8],
    native_events: &'static [&'static str],
    notes: String,
    phases: Vec<PhaseResult>,
}

fn row(
    name: &'static str,
    family: &'static str,
    scenario: &'static str,
    gaps: &'static [u8],
    native_events: &'static [&'static str],
) -> Row {
    Row {
        name,
        family,
        scenario,
        gaps,
        native_events,
        notes: String::new(),
        phases: Vec::new(),
    }
}

use Surface::*;

/// Rig check before the native event: the turn shows as running.
async fn owned_running(probe: &mut Probe) -> PhaseResult {
    let started = probe.begin().await;
    probe
        .observe(
            started,
            "Before the event: the turn runs (rig check)",
            "active; turn inProgress",
            vec![
                expect(RowPage, true, OWNED_MS),
                expect(Header, "inProgress", OWNED_MS),
            ],
            1_500,
        )
        .await
}

async fn codex_owned_completes() -> Row {
    let mut out = row(
        "codex/owned-turn-completes",
        "codex",
        "A Wonder-sent Codex turn finishes normally (baseline).",
        &[],
        &[r#"turn/completed {turn:{status:"completed"}}; thread/read status idle"#],
    );
    let mut rig = Rig::new().await;
    rig.codex_owned_turn().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    rig.codex_native(
        json!({"id":"thread","status":{"type":"idle"}}),
        json!([codex_turn("turn", "completed", true)]),
    );
    rig.codex_emit(codex_completed("turn", "completed")).await;
    out.phases.push(
        probe
            .observe(
                started,
                "Codex emits turn/completed (completed)",
                "idle; turn completed",
                vec![
                    expect(RowPage, false, OWNED_MS),
                    expect(RowPinned, false, OWNED_MS),
                    expect(Header, "completed", OWNED_MS),
                    expect(ActivityTurn, "completed", OWNED_MS),
                    expect(ActivityThread, "idle", OWNED_MS),
                    expect(Push, "completed", OWNED_MS),
                    expect(LiveActivity, "done", OWNED_MS + LIVE_ACTIVITY_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn codex_owned_fails() -> Row {
    let mut out = row(
        "codex/owned-turn-fails",
        "codex",
        "A Wonder-sent Codex turn fails (baseline for the attention push).",
        &[],
        &[r#"turn/completed {turn:{status:"failed"}}"#],
    );
    let mut rig = Rig::new().await;
    rig.codex_owned_turn().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    rig.codex_native(
        json!({"id":"thread","status":{"type":"idle"}}),
        json!([codex_turn("turn", "failed", true)]),
    );
    rig.codex_emit(codex_completed("turn", "failed")).await;
    out.phases.push(
        probe
            .observe(
                started,
                "Codex emits turn/completed (failed)",
                "idle; turn failed",
                vec![
                    expect(RowPage, false, OWNED_MS),
                    expect(Header, "failed", OWNED_MS),
                    expect(ActivityTurn, "failed", OWNED_MS),
                    expect(Push, "attention", OWNED_MS),
                    expect(LiveActivity, "failed", OWNED_MS + LIVE_ACTIVITY_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn codex_unknown_status() -> Row {
    let mut out = row(
        "codex/owned-turn-ends-with-unknown-status",
        "codex",
        "turn/completed carries a status outside the pinned TurnStatus enum (a newer \
         Codex). Expected: not shown or announced as a finished reply.",
        &[2],
        &[r#"turn/completed {turn:{status:"unknownFutureStatus"}}"#],
    );
    let mut rig = Rig::new().await;
    rig.codex_owned_turn().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    rig.codex_native(
        json!({"id":"thread","status":{"type":"idle"}}),
        json!([codex_turn("turn", "unknownFutureStatus", true)]),
    );
    rig.codex_emit(codex_completed("turn", "unknownFutureStatus"))
        .await;
    out.phases.push(
        probe
            .observe(
                started,
                "Codex emits turn/completed (unknownFutureStatus)",
                "idle; turn outcome unknown to Wonder",
                vec![
                    expect(RowPage, false, OWNED_MS),
                    expect(Header, "unknown", OWNED_MS),
                    expect(Push, "attention", OWNED_MS),
                    expect(LiveActivity, "failed", OWNED_MS + LIVE_ACTIVITY_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn codex_stop_unconfirmed() -> Row {
    let mut out = row(
        "codex/stop-before-harness-confirms",
        "codex",
        "Stop: turn/interrupt returns but Codex keeps running until it emits \
         turn/completed(interrupted) 1.5 s later (modeled delay).",
        &[1],
        &[
            "turn/interrupt -> {} (acknowledged only)",
            r#"+1.5 s turn/completed {turn:{status:"interrupted"}}"#,
        ],
    );
    let mut rig = Rig::new().await;
    rig.codex_owned_turn().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    let status = rig.interrupt(CHAT, "turn").await;
    out.notes = format!("Stop returned HTTP {}.", status.as_u16());
    out.phases.push(
        probe
            .observe(
                started,
                "Owner presses Stop; turn/interrupt returns",
                "still active; turn inProgress",
                vec![
                    expect(RowPage, true, OWNED_MS),
                    expect(RowPinned, true, OWNED_MS),
                    expect(Header, "inProgress", OWNED_MS),
                    expect(ActivityThread, "active", OWNED_MS),
                    expect(Push, "none", OWNED_MS),
                    expect(LiveActivity, "running", OWNED_MS + LIVE_ACTIVITY_MS),
                ],
                1_500,
            )
            .await,
    );
    let started = probe.begin().await;
    rig.codex_native(
        json!({"id":"thread","status":{"type":"idle"}}),
        json!([codex_turn("turn", "interrupted", true)]),
    );
    rig.codex_emit(codex_completed("turn", "interrupted")).await;
    out.phases.push(
        probe
            .observe(
                started,
                "Codex emits turn/completed (interrupted)",
                "idle; turn interrupted",
                vec![
                    expect(RowPage, false, OWNED_MS),
                    expect(Header, "interrupted", OWNED_MS),
                    expect(ActivityTurn, "interrupted", OWNED_MS),
                    expect(Push, "none", OWNED_MS),
                    expect(LiveActivity, "stopped", OWNED_MS + LIVE_ACTIVITY_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn codex_runtime_crash() -> Row {
    let mut out = row(
        "codex/runtime-exits-mid-turn",
        "codex",
        "Cupola's Codex App Server exits while its turn runs. On restart Codex \
         reports the unfinished turn as interrupted (no completedAt), as \
         codex_activity.rs documents. Expected: stopped, not finished; owner told.",
        &[3],
        &[
            "App Server process exits",
            r#"restarted thread/turns/list: [{status:"interrupted"}]"#,
        ],
    );
    let mut rig = Rig::new().await;
    rig.codex_owned_turn().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    rig.codex_native(
        json!({"id":"thread","status":{"type":"idle"}}),
        json!([codex_turn("turn", "interrupted", false)]),
    );
    rig.codex_crash().await;
    out.phases.push(
        probe
            .observe(
                started,
                "Codex App Server exits",
                "no process; turn did not finish",
                vec![
                    expect(RowPage, false, RECOVERY_MS),
                    expect(Header, "failed", RECOVERY_MS),
                    expect(Push, "attention", RECOVERY_MS),
                    expect(LiveActivity, "failed", RECOVERY_MS + LIVE_ACTIVITY_MS),
                ],
                RECOVERY_MS + 3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn codex_host_restart() -> Row {
    let mut out = row(
        "codex/host-restarts-mid-turn",
        "codex",
        "wonderd restarts while its Codex turn runs; the App Server (its child) \
         ends with it. Expected after restart: stopped, not finished; owner told.",
        &[3],
        &[
            "daemon and App Server stop",
            "new daemon: thread/turns/list [{status:\"interrupted\"}]",
        ],
    );
    let mut rig = Rig::new().await;
    rig.codex_owned_turn().await;
    rig.start(false).await;
    let mut before = Probe::new(&rig.state, rig.path(), CHAT).await;
    out.phases.push(owned_running(&mut before).await);
    rig.stop().await;
    let _ = rig.state.projects.codex.lock().await.shutdown().await;
    rig.codex_native(
        json!({"id":"thread","status":{"type":"notLoaded"}}),
        json!([codex_turn("turn", "interrupted", false)]),
    );
    // A new daemon: a fresh store connection and runtime, no in-memory state.
    rig.state.store = Store::connect(&format!(
        "sqlite://{}",
        rig.path().join("state.db").display()
    ))
    .await
    .unwrap();
    let config = rig.state.projects.config.lock().unwrap().clone();
    rig.state.projects = crate::projects::ProjectRuntime::configured(
        config.codex_bin,
        "test".into(),
        rig.path(),
        rig.path(),
        crate::ingestion::notification_sink(rig.state.store.clone()),
    );
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    let started = probe.begin().await;
    rig.start(false).await;
    out.phases.push(
        probe
            .observe(
                started,
                "Daemon starts again",
                "no process; turn did not finish",
                vec![
                    expect(RowPage, false, RECOVERY_MS),
                    expect(Header, "failed", RECOVERY_MS),
                    expect(Push, "attention", RECOVERY_MS),
                    expect(LiveActivity, "failed", RECOVERY_MS + LIVE_ACTIVITY_MS),
                ],
                RECOVERY_MS + 3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn codex_owned_approval() -> Row {
    let mut out = row(
        "codex/owned-turn-asks-approval",
        "codex",
        "A Wonder-sent Codex turn requests command approval (baseline for needs-input).",
        &[],
        &["server request item/commandExecution/requestApproval; thread active [waitingOnApproval]"],
    );
    let mut rig = Rig::new().await;
    rig.codex_owned_turn().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    rig.codex_native(
        json!({"id":"thread","status":{"type":"active","activeFlags":["waitingOnApproval"]}}),
        json!([codex_turn("turn", "inProgress", false)]),
    );
    rig.codex_emit(json!({"id": 9001, "method":"item/commandExecution/requestApproval",
        "params":{"threadId":"thread","turnId":"turn","itemId":"command","availableDecisions":["accept","decline"]}}))
        .await;
    out.phases.push(
        probe
            .observe(
                started,
                "Codex requests approval",
                "active, waiting on approval",
                vec![
                    expect(NeedsInput, true, OWNED_MS),
                    expect(Header, "inProgress", OWNED_MS),
                    expect(Push, "attention", OWNED_MS),
                    expect(LiveActivity, "needsApproval", OWNED_MS + LIVE_ACTIVITY_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn codex_compaction() -> Row {
    let mut out = row(
        "codex/compaction-mid-turn",
        "codex",
        "Codex compacts context during a Wonder-sent turn. Expected: still \
         working, with compaction named as the current step.",
        &[12],
        &[r#"item/started {item:{type:"contextCompaction"}}"#],
    );
    let mut rig = Rig::new().await;
    rig.codex_owned_turn().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    rig.codex_emit(
        json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn",
        "item":{"type":"contextCompaction","id":"compaction-1"}}}),
    )
    .await;
    out.phases.push(
        probe
            .observe(
                started,
                "Codex starts compaction",
                "active; compacting",
                vec![
                    expect(RowPage, true, OWNED_MS),
                    expect(RowStep, "contextCompaction", OWNED_MS),
                    expect(Header, "inProgress", OWNED_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn codex_subagent_after_parent() -> Row {
    let mut out = row(
        "codex/agent-task-runs-after-parent-reply",
        "codex",
        "The parent turn completes while a spawned agent thread keeps running. \
         Codex reports the parent idle and the child active, so the parent row \
         is not Running and the agent tasks dock shows the child running.",
        &[8],
        &[
            r#"turn/completed {turn:{status:"completed"}} (parent)"#,
            r#"thread/list parentThreadId=thread -> [{status:{type:"active"}}]"#,
        ],
    );
    let mut rig = Rig::new().await;
    rig.codex_owned_turn().await;
    rig.codex_family("active", codex_turn("turn", "inProgress", false));
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    rig.codex_family("idle", codex_turn("turn", "completed", true));
    rig.codex_emit(codex_completed("turn", "completed")).await;
    out.phases.push(
        probe
            .observe(
                started,
                "Parent turn completes; child still active",
                "parent idle; child active",
                vec![
                    expect(RowPage, false, OWNED_MS),
                    expect(Header, "completed", OWNED_MS),
                    expect(Subagents, "active", OWNED_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn claude_owned_completes() -> Row {
    let mut out = row(
        "claude/owned-turn-completes",
        "claude",
        "A Wonder-sent Claude reply finishes normally (baseline).",
        &[],
        &[r#"turn/completed {turn:{status:"completed"},agentFamily:"claude"}"#],
    );
    let mut rig = Rig::new().await;
    rig.claude_owned_turn().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CLAUDE_CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    rig.claude_harness("idle", json!([{"id":"claude-turn","status":"completed"}]));
    rig.claude_emit(claude_completed("completed")).await;
    out.phases.push(
        probe
            .observe(
                started,
                "Claude bridge emits turn/completed (completed)",
                "idle; turn completed",
                vec![
                    expect(RowPage, false, OWNED_MS),
                    expect(RowPinned, false, OWNED_MS),
                    expect(Header, "completed", OWNED_MS),
                    expect(ActivityTurn, "completed", OWNED_MS),
                    expect(Push, "completed", OWNED_MS),
                    expect(LiveActivity, "done", OWNED_MS + LIVE_ACTIVITY_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn claude_stop_unconfirmed() -> Row {
    let mut out = row(
        "claude/stop-before-harness-confirms",
        "claude",
        "Stop: the bridge acknowledges turn/interrupt, and the reply ends when \
         the SDK returns its result 1.5 s later (modeled delay; bridge.mjs \
         finishes the turn on that result).",
        &[1],
        &[
            "turn/interrupt -> {} (acknowledged only)",
            r#"+1.5 s turn/completed {turn:{status:"interrupted"}}"#,
        ],
    );
    let mut rig = Rig::new().await;
    rig.claude_owned_turn().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CLAUDE_CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    let status = rig.interrupt(CLAUDE_CHAT, "claude-turn").await;
    out.notes = format!("Stop returned HTTP {}.", status.as_u16());
    out.phases.push(
        probe
            .observe(
                started,
                "Owner presses Stop; turn/interrupt returns",
                "still active; turn inProgress",
                vec![
                    expect(RowPage, true, OWNED_MS),
                    expect(RowPinned, true, OWNED_MS),
                    expect(Header, "inProgress", OWNED_MS),
                    expect(Push, "none", OWNED_MS),
                    expect(LiveActivity, "running", OWNED_MS + LIVE_ACTIVITY_MS),
                ],
                1_500,
            )
            .await,
    );
    let started = probe.begin().await;
    rig.claude_harness("idle", json!([{"id":"claude-turn","status":"interrupted"}]));
    rig.claude_emit(claude_completed("interrupted")).await;
    out.phases.push(
        probe
            .observe(
                started,
                "Claude bridge emits turn/completed (interrupted)",
                "idle; turn interrupted",
                vec![
                    expect(RowPage, false, OWNED_MS),
                    expect(Header, "interrupted", OWNED_MS),
                    expect(Push, "none", OWNED_MS),
                    expect(LiveActivity, "stopped", OWNED_MS + LIVE_ACTIVITY_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn claude_bridge_crash() -> Row {
    let mut out = row(
        "claude/bridge-exits-mid-reply",
        "claude",
        "Cupola's Claude bridge exits mid-reply. On restart it marks the unfinished \
         turn interrupted (sessions.mjs, STALE_TURN). Expected: stopped, not \
         finished; owner told.",
        &[3, 7],
        &[
            "bridge process exits",
            r#"restarted thread/turns/list: [{status:"interrupted"}]"#,
        ],
    );
    let mut rig = Rig::new().await;
    rig.claude_owned_turn().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CLAUDE_CHAT).await;
    out.phases.push(owned_running(&mut probe).await);
    let started = probe.begin().await;
    rig.claude_harness("idle", json!([{"id":"claude-turn","status":"interrupted"}]));
    rig.claude_crash().await;
    out.phases.push(
        probe
            .observe(
                started,
                "Claude bridge exits",
                "no process; reply did not finish",
                vec![
                    expect(RowPage, false, RECOVERY_MS),
                    expect(Header, "failed", RECOVERY_MS),
                    expect(Push, "attention", RECOVERY_MS),
                    expect(LiveActivity, "failed", RECOVERY_MS + LIVE_ACTIVITY_MS),
                ],
                RECOVERY_MS + 3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

/// The desktop app starts a turn: the page row and activity must say so.
async fn codex_desktop_start(probe: &mut Probe, rig: &Rig, index: &mut CodexIndex) -> PhaseResult {
    let started = probe.begin().await;
    index.start("desktop-turn").await;
    rig.codex_desktop_native(
        json!([codex_turn("desktop-turn", "interrupted", false)]),
        index,
    );
    probe
        .observe(
            started,
            "Codex desktop starts a turn (task_started, indexed inProgress)",
            "running in the Codex app",
            vec![
                expect(RowPage, true, DESKTOP_MS),
                expect(ActivityTurn, "inProgress", DESKTOP_MS),
                expect(ActivityElsewhere, true, DESKTOP_MS),
                expect(Push, "none", EXTERNAL_PUSH_MS),
                expect(LiveActivity, "running", EXTERNAL_PUSH_MS + LIVE_ACTIVITY_MS),
                expect(SidebarEvent, true, EXTERNAL_PUSH_MS),
            ],
            EXTERNAL_PUSH_MS + 1_000,
        )
        .await
}

async fn codex_desktop_ends(status: &'static str, push: &'static str) -> Row {
    let (name, scenario, gaps): (&'static str, &'static str, &'static [u8]) = match status {
        "completed" => (
            "codex-desktop/turn-runs-and-completes",
            "The Codex app runs and finishes a turn in a followed thread (baseline).",
            &[10],
        ),
        "failed" => (
            "codex-desktop/turn-fails",
            "The Codex app's turn fails. Expected: attention push, as for Cupola's own.",
            &[6],
        ),
        _ => (
            "codex-desktop/turn-interrupted-on-mac",
            "The owner stops the turn in the Codex app. Expected: no push, as for Cupola's own Stop.",
            &[6],
        ),
    };
    let mut out = row(
        name,
        "codex",
        scenario,
        gaps,
        &[
            "rollout task_started; thread_turns inProgress",
            "rollout task_complete|turn_aborted|error; thread_turns terminal with matching rollout end; completed_at optional",
        ],
    );
    let mut rig = Rig::new().await;
    let mut index = rig.codex_desktop().await;
    rig.start(true).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    out.phases
        .push(codex_desktop_start(&mut probe, &rig, &mut index).await);
    let started = probe.begin().await;
    index.finish("desktop-turn", status).await;
    rig.codex_desktop_native(json!([codex_turn("desktop-turn", status, true)]), &index);
    out.phases.push(
        probe
            .observe(
                started,
                &format!("Codex desktop turn ends ({status})"),
                &format!("idle; turn {status}"),
                vec![
                    expect(RowPage, false, DESKTOP_MS),
                    expect(ActivityTurn, status, DESKTOP_MS),
                    expect(ActivityElsewhere, false, DESKTOP_MS),
                    expect(Push, push, EXTERNAL_PUSH_MS),
                    expect(
                        LiveActivity,
                        live_phase(status),
                        EXTERNAL_PUSH_MS + LIVE_ACTIVITY_MS,
                    ),
                    expect(SidebarEvent, true, EXTERNAL_PUSH_MS),
                ],
                EXTERNAL_PUSH_MS + 2_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn codex_desktop_pinned(minutes_since_wonder: u64) -> Row {
    let (name, scenario) = if minutes_since_wonder > 15 {
        (
            "codex-desktop/pinned-thread-last-used-in-wonder-1h-ago",
            "The Codex app starts a turn in a pinned thread Cupola last used an hour ago.",
        )
    } else {
        (
            "codex-desktop/pinned-thread-used-in-wonder-recently",
            "The Codex app starts a turn in a pinned thread Cupola used a minute ago.",
        )
    };
    let mut out = row(
        name,
        "codex",
        scenario,
        &[4],
        &["rollout task_started; thread_turns inProgress"],
    );
    let mut rig = Rig::new().await;
    let mut index = rig.codex_desktop().await;
    rig.pin(CHAT).await;
    rig.last_used(CHAT, Duration::from_secs(minutes_since_wonder * 60))
        .await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    let started = probe.begin().await;
    index.start("desktop-turn").await;
    rig.codex_desktop_native(
        json!([codex_turn("desktop-turn", "interrupted", false)]),
        &index,
    );
    out.phases.push(
        probe
            .observe(
                started,
                "Codex desktop starts a turn",
                "running in the Codex app",
                vec![
                    expect(RowPage, true, DESKTOP_MS),
                    expect(RowPinned, true, DESKTOP_MS),
                    expect(ActivityElsewhere, true, DESKTOP_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn codex_desktop_silent() -> Row {
    let mut out = row(
        "codex-desktop/turn-silent-over-15-min",
        "codex",
        "A Codex app turn is still open (no end record) but its rollout has not \
         been written for 16 min (a long tool call), while the Codex process that \
         started it still holds the rollout open. Rare: silences in a sampled 150 \
         rollouts stayed under five minutes. Without that live writer the turn \
         reads unknown (codex_activity.rs unit tests).",
        &[6],
        &["thread_turns inProgress, index caught up; rollout mtime now-16 min; writer open since before the turn"],
    );
    let mut rig = Rig::new().await;
    let mut index = rig.codex_desktop().await;
    rig.start(false).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CHAT).await;
    index.start("desktop-turn").await;
    rig.codex_desktop_native(
        json!([codex_turn("desktop-turn", "interrupted", false)]),
        &index,
    );
    let started = probe.begin().await;
    index.silent_for(Duration::from_secs(16 * 60));
    out.phases.push(
        probe
            .observe(
                started,
                "16 minutes without a rollout write",
                "still running in the Codex app",
                vec![
                    expect(RowPage, true, DESKTOP_MS),
                    expect(ActivityTurn, "inProgress", DESKTOP_MS),
                    expect(ActivityElsewhere, true, DESKTOP_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn claude_desktop_start(probe: &mut Probe, desktop: &ClaudeDesktop) -> PhaseResult {
    let started = probe.begin().await;
    desktop.prompt();
    desktop.status("busy");
    probe
        .observe(
            started,
            "Claude Code starts a reply (record busy; transcript ends with the prompt)",
            "writing a reply on the Mac",
            vec![
                expect(RowPage, true, DESKTOP_MS),
                expect(RowPinned, true, DESKTOP_MS),
                expect(Push, "none", EXTERNAL_PUSH_MS),
                expect(LiveActivity, "running", EXTERNAL_PUSH_MS + LIVE_ACTIVITY_MS),
                expect(SidebarEvent, true, EXTERNAL_PUSH_MS),
            ],
            EXTERNAL_PUSH_MS + 1_000,
        )
        .await
}

async fn claude_desktop_completes() -> Row {
    let mut out = row(
        "claude-desktop/reply-runs-and-completes",
        "claude",
        "Claude Code on the Mac writes and finishes a reply in a followed chat (baseline).",
        &[10],
        &[
            r#"sessions/<pid>.json status busy; transcript user prompt"#,
            r#"transcript assistant stop_reason end_turn; status idle"#,
        ],
    );
    let mut rig = Rig::new().await;
    let desktop = rig.claude_desktop().await;
    desktop.status("idle");
    rig.start(true).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CLAUDE_CHAT).await;
    out.phases
        .push(claude_desktop_start(&mut probe, &desktop).await);
    let started = probe.begin().await;
    desktop.reply();
    desktop.status("idle");
    out.phases.push(
        probe
            .observe(
                started,
                "Claude Code finishes the reply",
                "idle",
                vec![
                    expect(RowPage, false, DESKTOP_MS),
                    expect(RowPinned, false, DESKTOP_MS),
                    expect(Push, "completed", CLAUDE_REPLY_PUSH_MS),
                    expect(LiveActivity, "done", EXTERNAL_PUSH_MS + LIVE_ACTIVITY_MS),
                    expect(SidebarEvent, true, EXTERNAL_PUSH_MS),
                ],
                CLAUDE_REPLY_PUSH_MS + 2_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn claude_desktop_waiting() -> Row {
    let mut out = row(
        "claude-desktop/permission-prompt-on-mac",
        "claude",
        "Claude Code on the Mac stops for a permission prompt mid-reply.",
        &[5],
        &[r#"sessions/<pid>.json status busy -> waiting"#],
    );
    let mut rig = Rig::new().await;
    let desktop = rig.claude_desktop().await;
    desktop.status("idle");
    rig.start(true).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CLAUDE_CHAT).await;
    out.phases
        .push(claude_desktop_start(&mut probe, &desktop).await);
    let started = probe.begin().await;
    desktop.status("waiting");
    out.phases.push(
        probe
            .observe(
                started,
                "Claude Code shows a permission prompt",
                "waiting on the owner",
                vec![
                    expect(NeedsInput, true, DESKTOP_MS),
                    expect(Push, "attention", EXTERNAL_PUSH_MS),
                    expect(
                        LiveActivity,
                        "needsInput",
                        EXTERNAL_PUSH_MS + LIVE_ACTIVITY_MS,
                    ),
                    expect(SidebarEvent, true, EXTERNAL_PUSH_MS),
                ],
                EXTERNAL_PUSH_MS + 2_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn claude_desktop_exits() -> Row {
    let mut out = row(
        "claude-desktop/process-exits-mid-reply",
        "claude",
        "The Claude Code process on the Mac exits mid-reply; its record file stays.",
        &[7],
        &["process exits (SIGKILL); sessions/<pid>.json still says busy"],
    );
    let mut rig = Rig::new().await;
    let desktop = rig.claude_desktop().await;
    desktop.status("idle");
    rig.start(true).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CLAUDE_CHAT).await;
    out.phases
        .push(claude_desktop_start(&mut probe, &desktop).await);
    let started = probe.begin().await;
    for child in &mut rig.children {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    out.phases.push(
        probe
            .observe(
                started,
                "Claude Code process exits",
                "no process; reply did not finish",
                vec![
                    expect(RowPage, false, DESKTOP_MS),
                    expect(RowPinned, false, DESKTOP_MS),
                    expect(Push, "attention", EXTERNAL_PUSH_MS),
                    expect(LiveActivity, "failed", EXTERNAL_PUSH_MS + LIVE_ACTIVITY_MS),
                    expect(SidebarEvent, true, EXTERNAL_PUSH_MS),
                ],
                EXTERNAL_PUSH_MS + 2_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

async fn claude_desktop_send_takes_over() -> Row {
    let mut out = row(
        "claude-desktop/phone-send-takes-over-mid-reply",
        "claude",
        "Claude Code on the Mac is replying when the phone sends: Cupola stops the \
         Mac's process and continues the session in its own bridge. Expected: the \
         Mac's reply ends interrupted and the new turn runs, then completes. \
         (2026-10-15: the bridge closed the turn it was admitting as stale, so the \
         phone showed it interrupted and stopped following it; bridge test \
         \"a read while a new turn is being saved\".)",
        &[],
        &[
            r#"sessions/<pid>.json status busy; transcript user prompt"#,
            "process stops (SIGINT); transcript [Request interrupted by user]",
            r#"bridge turn/started {turn:{status:"inProgress"}}"#,
            r#"turn/completed {turn:{status:"completed"},agentFamily:"claude"}"#,
        ],
    );
    let mut rig = Rig::new().await;
    let desktop = rig.claude_desktop().await;
    desktop.status("idle");
    rig.claude_harness("idle", json!([]));
    rig.claude_bridge().await;
    rig.start(true).await;
    let mut probe = Probe::new(&rig.state, rig.path(), CLAUDE_CHAT).await;
    out.phases
        .push(claude_desktop_start(&mut probe, &desktop).await);
    let started = probe.begin().await;
    for child in &mut rig.children {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    desktop.transcript(&[json!({"type":"user","message":{"role":"user",
        "content":[{"type":"text","text":"[Request interrupted by user]"}]}})]);
    desktop.status("idle");
    rig.claude_harness(
        "active",
        json!([{"id":"claude-turn","status":"inProgress"}]),
    );
    rig.claude_turn_started().await;
    out.phases.push(
        probe
            .observe(
                started,
                "Cupola stops the Mac's reply and starts the phone's turn",
                "active; Cupola's turn inProgress, not elsewhere",
                vec![
                    expect(RowPage, true, OWNED_MS),
                    expect(RowPinned, true, OWNED_MS),
                    expect(Header, "inProgress", OWNED_MS),
                    expect(ActivityTurn, "inProgress", OWNED_MS),
                    expect(ActivityElsewhere, false, OWNED_MS),
                    expect(Push, "none", EXTERNAL_PUSH_MS),
                ],
                EXTERNAL_PUSH_MS + 1_000,
            )
            .await,
    );
    let started = probe.begin().await;
    rig.claude_harness("idle", json!([{"id":"claude-turn","status":"completed"}]));
    rig.claude_emit(claude_completed("completed")).await;
    out.phases.push(
        probe
            .observe(
                started,
                "Claude bridge emits turn/completed (completed)",
                "idle; turn completed",
                vec![
                    expect(RowPage, false, OWNED_MS),
                    expect(Header, "completed", OWNED_MS),
                    expect(ActivityTurn, "completed", OWNED_MS),
                    expect(Push, "completed", OWNED_MS),
                ],
                3_000,
            )
            .await,
    );
    rig.shutdown().await;
    out
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

const ROWS: [&str; 22] = [
    "codex/owned-turn-completes",
    "codex/owned-turn-fails",
    "codex/owned-turn-asks-approval",
    "codex/stop-before-harness-confirms",
    "codex/owned-turn-ends-with-unknown-status",
    "codex/runtime-exits-mid-turn",
    "codex/host-restarts-mid-turn",
    "codex/compaction-mid-turn",
    "codex/agent-task-runs-after-parent-reply",
    "claude/owned-turn-completes",
    "claude/stop-before-harness-confirms",
    "claude/bridge-exits-mid-reply",
    "codex-desktop/turn-runs-and-completes",
    "codex-desktop/turn-fails",
    "codex-desktop/turn-interrupted-on-mac",
    "codex-desktop/pinned-thread-used-in-wonder-recently",
    "codex-desktop/pinned-thread-last-used-in-wonder-1h-ago",
    "codex-desktop/turn-silent-over-15-min",
    "claude-desktop/reply-runs-and-completes",
    "claude-desktop/permission-prompt-on-mac",
    "claude-desktop/process-exits-mid-reply",
    "claude-desktop/phone-send-takes-over-mid-reply",
];

async fn run_row(name: &'static str) -> Row {
    match name {
        "codex/owned-turn-completes" => codex_owned_completes().await,
        "codex/owned-turn-fails" => codex_owned_fails().await,
        "codex/owned-turn-asks-approval" => codex_owned_approval().await,
        "codex/stop-before-harness-confirms" => codex_stop_unconfirmed().await,
        "codex/owned-turn-ends-with-unknown-status" => codex_unknown_status().await,
        "codex/runtime-exits-mid-turn" => codex_runtime_crash().await,
        "codex/host-restarts-mid-turn" => codex_host_restart().await,
        "codex/compaction-mid-turn" => codex_compaction().await,
        "codex/agent-task-runs-after-parent-reply" => codex_subagent_after_parent().await,
        "claude/owned-turn-completes" => claude_owned_completes().await,
        "claude/stop-before-harness-confirms" => claude_stop_unconfirmed().await,
        "claude/bridge-exits-mid-reply" => claude_bridge_crash().await,
        "codex-desktop/turn-runs-and-completes" => {
            codex_desktop_ends("completed", "completed").await
        }
        "codex-desktop/turn-fails" => codex_desktop_ends("failed", "attention").await,
        "codex-desktop/turn-interrupted-on-mac" => codex_desktop_ends("interrupted", "none").await,
        "codex-desktop/pinned-thread-used-in-wonder-recently" => codex_desktop_pinned(1).await,
        "codex-desktop/pinned-thread-last-used-in-wonder-1h-ago" => codex_desktop_pinned(60).await,
        "codex-desktop/turn-silent-over-15-min" => codex_desktop_silent().await,
        "claude-desktop/reply-runs-and-completes" => claude_desktop_completes().await,
        "claude-desktop/permission-prompt-on-mac" => claude_desktop_waiting().await,
        "claude-desktop/process-exits-mid-reply" => claude_desktop_exits().await,
        "claude-desktop/phone-send-takes-over-mid-reply" => claude_desktop_send_takes_over().await,
        other => panic!("unknown row {other}"),
    }
}

fn finish(row: Row, seconds: f64) -> RowResult {
    let pass = !row.phases.is_empty()
        && row
            .phases
            .iter()
            .all(|phase| phase.checks.iter().all(|check| check.pass));
    RowResult {
        name: row.name.into(),
        family: row.family.into(),
        scenario: row.scenario.into(),
        gaps: row.gaps.to_vec(),
        native_events: row.native_events.iter().map(|e| (*e).to_owned()).collect(),
        phases: row.phases,
        pass,
        notes: row.notes,
        error: None,
        seconds,
    }
}

/// The phone's sidebar after desktop work changes: the measured host delay to
/// the `native_status` event, plus the phone's coalescing constant.
fn phone_rows(rows: &[RowResult]) -> Vec<RowResult> {
    let host = rows
        .iter()
        .filter(|row| row.name.contains("desktop/"))
        .flat_map(|row| row.phases.iter())
        .flat_map(|phase| phase.checks.iter())
        .filter(|check| check.surface == Surface::SidebarEvent.name())
        .map(|check| check.delay_ms)
        .collect::<Vec<_>>();
    // Every desktop phase must have produced its event; one that never did
    // leaves the phone on its 30 s fallback.
    let slowest = (!host.is_empty())
        .then(|| host.iter().copied().collect::<Option<Vec<_>>>())
        .flatten()
        .and_then(|delays| delays.into_iter().max());
    let bound = slowest.map(|ms| ms + PHONE_STATUS_COALESCE_MS);
    vec![RowResult {
        name: "desktop/phone-sidebar-refresh".into(),
        family: "codex+claude".into(),
        scenario: "Desktop work reaches the phone's sidebar through a native_status event \
            on the event socket, which the phone answers with one coalesced sidebar read \
            (ConnectionModel.swift). Bound = slowest measured host delay to the event \
            across the desktop rows + the phone's 300 ms coalescing constant; the 30 s \
            refresh remains the fallback."
            .into(),
        gaps: vec![10],
        native_events: vec!["desktop turn starts, waits, ends or its process exits".into()],
        phases: vec![PhaseResult {
            event: "desktop work changes".into(),
            harness: "running / waiting / idle".into(),
            checks: vec![Check {
                surface: "phone.sidebar (bound)",
                expected: json!("row follows"),
                observed: json!(bound.map_or("unknown".into(), |ms| format!("<= {ms} ms"))),
                delay_ms: bound,
                budget_ms: EXTERNAL_PUSH_MS,
                pass: bound.is_some_and(|ms| ms <= EXTERNAL_PUSH_MS),
                transitions: Vec::new(),
            }],
        }],
        pass: bound.is_some_and(|ms| ms <= EXTERNAL_PUSH_MS),
        notes: format!(
            "Host delays measured ({} desktop phases, slowest {} ms); the phone constant is \
             not measured here. The sidebar read itself adds one HTTP round trip.",
            host.len(),
            slowest.map_or("-".into(), |ms| ms.to_string())
        ),
        error: None,
        seconds: 0.0,
    }]
}

fn cell(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
    .replace('|', "/")
}

fn markdown(rows: &[RowResult], seconds: f64) -> String {
    let failed = rows.iter().filter(|row| !row.pass).count();
    let mut out = format!(
        "{} rows: {} pass, {} fail; {:.0} s. Budgets: Wonder-owned host surfaces {} ms; \
         after runtime loss {} ms (5 s recovery + 1 s); desktop host reads {} ms; desktop \
         push {} ms (4 s watcher + 1 s), a Mac Claude reply's {} ms (two idle polls + 1 s). Push columns are host intents; delivery adds up to \
         {} s (push sender interval, not measured).\n\n",
        rows.len(),
        rows.len() - failed,
        failed,
        seconds,
        OWNED_MS,
        RECOVERY_MS,
        DESKTOP_MS,
        EXTERNAL_PUSH_MS,
        CLAUDE_REPLY_PUSH_MS,
        PUSH_SENDER_MS / 1000
    );
    out += "| Row | Gaps | Result | Failing checks (expected -> observed @ ms / budget) |\n";
    out += "| --- | --- | --- | --- |\n";
    for row in rows {
        let failing = row
            .phases
            .iter()
            .flat_map(|phase| phase.checks.iter())
            .filter(|check| !check.pass)
            .map(|check| {
                format!(
                    "{}: {} -> {} @ {} / {}",
                    check.surface,
                    cell(&check.expected),
                    cell(&check.observed),
                    check.delay_ms.map_or("never".into(), |ms| ms.to_string()),
                    check.budget_ms
                )
            })
            .collect::<Vec<_>>();
        let result = match (&row.error, row.pass) {
            (Some(_), _) => "ERROR",
            (None, true) => "PASS",
            (None, false) => "FAIL",
        };
        let detail = match &row.error {
            Some(error) => error.replace('|', "/"),
            None if failing.is_empty() => "-".into(),
            None => failing.join("; "),
        };
        let gaps = row
            .gaps
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        out += &format!("| `{}` | {} | {} | {} |\n", row.name, gaps, result, detail);
    }
    out += "\n<details><summary>Every check</summary>\n\n";
    out += "| Row | Phase | Surface | Expected | Observed | Delay ms | Budget ms | |\n";
    out += "| --- | --- | --- | --- | --- | --- | --- | --- |\n";
    for row in rows {
        for phase in &row.phases {
            for check in &phase.checks {
                out += &format!(
                    "| `{}` | {} | {} | {} | {} | {} | {} | {} |\n",
                    row.name,
                    phase.event.replace('|', "/"),
                    check.surface,
                    cell(&check.expected),
                    cell(&check.observed),
                    check.delay_ms.map_or("-".into(), |ms| ms.to_string()),
                    check.budget_ms,
                    if check.pass { "PASS" } else { "FAIL" }
                );
            }
        }
    }
    out += "\n</details>\n\nNot covered by the host table (phone-side): Mac unreachable \
        while a turn runs (cached spinner and enabled Stop; gap 9) and a lost WebSocket \
        during a Wonder-owned turn (gap 11). The host surfaces above are read over HTTP and \
        do not depend on the event socket.\n";
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "about two minutes of real host timers; run by scripts/nightly-sim"]
async fn status_truth_table() {
    // Read only synthetic Claude Code and Codex records, never the owner's.
    let scratch = tempfile::tempdir().unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", scratch.path().join("claude-config"));
    std::env::set_var("CODEX_HOME", scratch.path().join("codex-home"));
    let only = std::env::var("WONDER_STATUS_TABLE_ONLY").ok();
    let started = Instant::now();
    let mut results = Vec::new();
    for name in ROWS {
        if only.as_deref().is_some_and(|only| !name.contains(only)) {
            continue;
        }
        let row_started = Instant::now();
        // A row that cannot be built is reported, and the others still run.
        let outcome = tokio::spawn(run_row(name)).await;
        let seconds = row_started.elapsed().as_secs_f64();
        eprintln!("status-truth: {name} ({seconds:.1} s)");
        results.push(match outcome {
            Ok(row) => finish(row, seconds),
            Err(error) => RowResult {
                name: name.into(),
                family: name.split('/').next().unwrap_or_default().into(),
                scenario: String::new(),
                gaps: Vec::new(),
                native_events: Vec::new(),
                phases: Vec::new(),
                pass: false,
                notes: String::new(),
                error: Some(
                    error
                        .try_into_panic()
                        .ok()
                        .and_then(|panic| {
                            panic
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                        })
                        .unwrap_or_else(|| "row did not finish".into())
                        .chars()
                        .take(300)
                        .collect(),
                ),
                seconds,
            },
        });
    }
    if only.is_none() {
        let derived = phone_rows(&results);
        results.extend(derived);
    }
    let seconds = started.elapsed().as_secs_f64();
    let table = markdown(&results, seconds);
    println!("{table}");
    if let Some(path) = std::env::var_os("WONDER_STATUS_TABLE") {
        let path = PathBuf::from(path);
        let report = json!({
            "generatedAt": chrono::Utc::now().to_rfc3339(),
            "seconds": seconds,
            "budgetsMs": {"owned": OWNED_MS, "recovery": RECOVERY_MS, "desktop": DESKTOP_MS,
                "externalPush": EXTERNAL_PUSH_MS, "claudeReplyPush": CLAUDE_REPLY_PUSH_MS, "phoneSidebarRefresh": PHONE_SIDEBAR_REFRESH_MS, "phoneStatusCoalesce": PHONE_STATUS_COALESCE_MS,
                "pushSender": PUSH_SENDER_MS},
            "rows": results,
            "notCovered": [
                {"gap": 9, "name": "phone/mac-unreachable", "detail": "Phone keeps its cached spinner and Stop; phone-side."},
                {"gap": 11, "name": "phone/lost-websocket", "detail": "Phone event socket loss; host surfaces are HTTP reads."},
            ],
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        std::fs::write(path.with_extension("md"), &table).unwrap();
    }
    let errors: Vec<_> = results.iter().filter(|row| row.error.is_some()).collect();
    assert!(
        errors.is_empty(),
        "rows could not be built: {:?}",
        errors.iter().map(|row| &row.name).collect::<Vec<_>>()
    );
}
