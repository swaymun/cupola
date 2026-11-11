//! Live Activity rows follow the host's status transitions (the status
//! truth table's scenarios: owned turns that end only on harness
//! confirmation, interrupts, failures, approvals, questions and desktop work)
//! and carry no private text. Each Mac reports only its own chats.
use super::*;
use axum::{body::Body, http::Request};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use wonder_api::DeliveryState;

const CHAT: &str = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const START_TOKEN: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const PROMPT: &str = "SECRET prompt about payroll.xlsx";

fn inputs(
    owned: &[&str],
    desktop: &[(&str, Desktop)],
    approvals: &[&str],
    questions: &[&str],
) -> Inputs {
    Inputs {
        muted: BTreeSet::new(),
        muted_activity: false,
        owned: owned.iter().map(|s| s.to_string()).collect(),
        desktop: desktop
            .iter()
            .map(|(id, state)| (id.to_string(), *state))
            .collect(),
        approvals: approvals.iter().map(|s| s.to_string()).collect(),
        questions: questions.iter().map(|s| s.to_string()).collect(),
    }
}

// The stretch timestamp belongs to observation/push-start scheduling, not a
// provider turn's age. A restarted daemon observes old work as a new stretch.
#[test]
fn stretch_start_is_first_observation_and_survives_updates() {
    for observed in [
        inputs(&["codex-owned"], &[], &[], &[]),
        inputs(&["claude-owned"], &[], &[], &[]),
        inputs(&[], &[("codex-desktop", Desktop::Working)], &[], &[]),
        inputs(&[], &[("claude-desktop", Desktop::Working)], &[], &[]),
    ] {
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker
                .step(&observed, 1_000, |_, _| Phase::Done)
                .started_at,
            1_000
        );
        assert_eq!(
            tracker
                .step(&observed, 11_800, |_, _| Phase::Done)
                .started_at,
            1_000
        );
        let mut restarted = Tracker::default();
        let after_restart = restarted.step(&observed, 11_800, |_, _| Phase::Done);
        assert_eq!(after_restart.started_at, 11_800);
        assert_eq!(22_600 - after_restart.started_at, 10_800); // three hours, not six
    }
}

// Contract: one active parent is either working or waiting on the owner.
// Repeated approvals, questions and desktop observations cannot inflate counts.
#[test]
fn counts_partition_work_and_deduplicate_attention_across_sources() {
    let observed = inputs(
        &["codex", "claude", "opencode"],
        &[("codex", Desktop::Working), ("desktop", Desktop::Waiting)],
        &["codex", "codex", "claude"],
        &["codex", "claude", "desktop"],
    );
    let mut tracker = Tracker::default();
    let aggregate = tracker.step(&observed, 100, |_, _| Phase::Done);
    assert_eq!(aggregate.phase, Phase::NeedsApproval);
    assert_eq!((aggregate.running, aggregate.needs_input), (4, 3));
    assert_eq!(aggregate.running - aggregate.needs_input, 1);
    let resumed = tracker.step(
        &inputs(&["codex", "claude", "opencode"], &[], &[], &[]),
        110,
        |_, _| Phase::Done,
    );
    assert_eq!((resumed.running, resumed.needs_input), (3, 0));
}

// Provider records enter the same projection without dispatching any model work.
#[tokio::test]
async fn saved_work_counts_all_owned_providers_once_each() {
    use wonder_store::{AgentFamily, ProjectConversationInsert};
    let (_dir, state, _) = crate::projects::tests::handoff_fixture().await;
    let project = state.store.project("project").await.unwrap().unwrap();
    for (id, family) in [
        ("claude-chat", AgentFamily::Claude),
        ("opencode-chat", AgentFamily::OpenCode),
        ("cursor-chat", AgentFamily::Cursor),
    ] {
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: id,
                project_id: "project",
                family,
                provider_store: "fixture",
                native_session_id: Some(id),
                cwd: &project.primary_root().path,
                roots_revision: 1,
                title: "Synthetic work",
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
        let wonder_store::MessageInsert::Inserted(message) = state
            .store
            .insert_message("owner", id, PROMPT, "hash", id, "now")
            .await
            .unwrap()
        else {
            panic!("new receipt")
        };
        state
            .store
            .update_message_delivery(&message.id, "streaming", Some(id), Some("turn"))
            .await
            .unwrap();
        state
            .store
            .save_async_question(id, id, "turn", "ask", "[]", i64::MAX)
            .await
            .unwrap();
    }
    let observed = read_inputs(&state).await.unwrap();
    assert_eq!(
        observed.owned,
        [
            "project-chat",
            "claude-chat",
            "opencode-chat",
            "cursor-chat"
        ]
        .map(str::to_owned)
        .into()
    );
    let aggregate = Tracker::default().step(&observed, 100, |_, _| Phase::Done);
    assert_eq!((aggregate.running, aggregate.needs_input), (4, 3));
    state
        .store
        .set_chat_notification_level("claude-chat", wonder_store::ChatNotificationLevel::Mute)
        .await
        .unwrap();
    let observed = read_inputs(&state).await.unwrap();
    assert!(!observed.owned.contains("claude-chat"));
    assert!(!observed.questions.contains("claude-chat"));
    let aggregate = evaluate(&state, &mut Tracker::default(), 110)
        .await
        .unwrap();
    assert_eq!(
        (aggregate.running, aggregate.needs_input),
        (3, 2),
        "other chats still update"
    );
    state.projects.shutdown().await;
}

/// Status-table rows as aggregate steps: what the activity shows after each
/// host transition, and how it ends.
#[test]
fn aggregate_follows_each_status_transition() {
    use Desktop::*;
    use Phase::*;
    type Step = (
        Inputs,
        Option<(&'static str, Phase)>,
        Phase,
        u32,
        Option<&'static str>,
    );
    let rows: Vec<(&str, Vec<Step>)> = vec![
        (
            "codex owned turn completes",
            vec![
                (inputs(&["a"], &[], &[], &[]), None, Running, 1, Some("a")),
                (inputs(&[], &[], &[], &[]), Some(("a", Done)), Done, 0, None),
            ],
        ),
        (
            "owned turn interrupted by Stop",
            vec![
                (inputs(&["a"], &[], &[], &[]), None, Running, 1, Some("a")),
                (
                    inputs(&[], &[], &[], &[]),
                    Some(("a", Stopped)),
                    Stopped,
                    0,
                    None,
                ),
            ],
        ),
        (
            "runtime crash reads failed",
            vec![
                (inputs(&["a"], &[], &[], &[]), None, Running, 1, Some("a")),
                (
                    inputs(&[], &[], &[], &[]),
                    Some(("a", Failed)),
                    Failed,
                    0,
                    None,
                ),
            ],
        ),
        (
            "approval, then resolved, then done",
            vec![
                (inputs(&["a"], &[], &[], &[]), None, Running, 1, Some("a")),
                (
                    inputs(&["a"], &[], &["a"], &[]),
                    None,
                    NeedsApproval,
                    1,
                    Some("a"),
                ),
                (inputs(&["a"], &[], &[], &[]), None, Running, 1, Some("a")),
                (inputs(&[], &[], &[], &[]), Some(("a", Done)), Done, 0, None),
            ],
        ),
        (
            "unrouted approval still needs the owner",
            vec![
                (inputs(&[], &[], &[""], &[]), None, NeedsApproval, 1, None),
                (inputs(&[], &[], &[], &[]), None, Done, 0, None),
            ],
        ),
        (
            "question waits for input",
            vec![
                (
                    inputs(&["a"], &[], &[], &["a"]),
                    None,
                    NeedsInput,
                    1,
                    Some("a"),
                ),
                (inputs(&[], &[], &[], &[]), Some(("a", Done)), Done, 0, None),
            ],
        ),
        (
            "claude desktop: working, waiting at the Mac, idle",
            vec![
                (
                    inputs(&[], &[("d", Working)], &[], &[]),
                    None,
                    Running,
                    1,
                    Some("d"),
                ),
                (
                    inputs(&[], &[("d", Waiting)], &[], &[]),
                    None,
                    NeedsInput,
                    1,
                    Some("d"),
                ),
                (inputs(&[], &[], &[], &[]), Some(("d", Done)), Done, 0, None),
            ],
        ),
        (
            "codex desktop turn fails",
            vec![
                (
                    inputs(&[], &[("d", Working)], &[], &[]),
                    None,
                    Running,
                    1,
                    Some("d"),
                ),
                (
                    inputs(&[], &[], &[], &[]),
                    Some(("d", Failed)),
                    Failed,
                    0,
                    None,
                ),
            ],
        ),
        (
            "several agents: one aggregate, worst outcome",
            vec![
                (
                    inputs(&["a"], &[("d", Working)], &[], &[]),
                    None,
                    Running,
                    2,
                    None,
                ),
                (
                    inputs(&["a"], &[], &[], &[]),
                    Some(("d", Failed)),
                    Running,
                    1,
                    Some("a"),
                ),
                (
                    inputs(&["a", "b"], &[], &["b"], &[]),
                    None,
                    NeedsApproval,
                    2,
                    Some("b"),
                ),
                (
                    inputs(&[], &[], &[], &[]),
                    Some(("a", Done)),
                    Failed,
                    0,
                    None,
                ),
            ],
        ),
    ];
    for (name, steps) in rows {
        let mut tracker = Tracker::default();
        let mut outcomes: HashMap<String, Phase> = HashMap::new();
        let mut started = None;
        for (index, (input, ended, phase, running, target)) in steps.into_iter().enumerate() {
            if let Some((id, outcome)) = ended {
                outcomes.insert(id.into(), outcome);
            }
            let now = 1_000 + index as i64 * 10;
            let aggregate = tracker.step(&input, now, |id, _| {
                outcomes.get(id).copied().unwrap_or(Done)
            });
            assert_eq!(aggregate.phase, phase, "{name} step {index}");
            assert_eq!(aggregate.running, running, "{name} step {index}");
            assert_eq!(aggregate.target.as_deref(), target, "{name} step {index}");
            // Push-to-start scheduling preserves this observation boundary.
            let first = *started.get_or_insert(aggregate.started_at);
            assert_eq!(aggregate.started_at, first, "{name} step {index}");
            if phase.is_finished() {
                assert_eq!(aggregate.needs_input, 0, "{name} step {index}");
            }
        }
    }
    // Nothing has run since the Mac started: an activity ends at once.
    let mut idle = Tracker::default();
    let aggregate = idle.step(&Inputs::default(), 5, |_, _| Phase::Running);
    assert_eq!(
        (aggregate.phase, aggregate.running, aggregate.needs_input),
        (Phase::Done, 0, 0)
    );
    // A new stretch restarts the push-start grace and the outcome.
    let mut tracker = Tracker::default();
    tracker.step(&inputs(&["a"], &[], &[], &[]), 10, |_, _| Phase::Failed);
    assert_eq!(
        tracker
            .step(&Inputs::default(), 20, |_, _| Phase::Failed)
            .phase,
        Phase::Failed
    );
    let next = tracker.step(&inputs(&["b"], &[], &[], &[]), 30, |_, _| Phase::Done);
    assert_eq!((next.phase, next.started_at), (Phase::Running, 30));
    assert_eq!(
        tracker
            .step(&Inputs::default(), 40, |_, _| Phase::Done)
            .phase,
        Phase::Done
    );
}

#[test]
fn a_vanished_desktop_thread_is_held_for_one_reading() {
    let hub = Hub::default();
    hub.desktop_reading([("d".to_string(), Desktop::Working)].into(), vec![]);
    hub.desktop_reading(HashMap::new(), vec![]);
    assert_eq!(
        hub.desktop.lock().unwrap().get("d"),
        Some(&Desktop::Working)
    );
    hub.desktop_reading(HashMap::new(), vec![]);
    assert!(hub.desktop.lock().unwrap().is_empty());
    // A known ending is not held.
    hub.desktop_reading([("d".to_string(), Desktop::Working)].into(), vec![]);
    hub.desktop_reading(HashMap::new(), vec![("d".into(), Phase::Failed)]);
    assert!(hub.desktop.lock().unwrap().is_empty());
}

// Child-task questions share the same verified parent as running turns, so
// the Chats visibility projection keeps their attention phase and counts once.
#[tokio::test]
async fn child_questions_follow_the_visible_parent_conversation() {
    let (_dir, state, _) = crate::projects::tests::handoff_fixture().await;
    for child in ["child-a", "child-b"] {
        let thread = format!("{child}-thread");
        state
            .store
            .register_subagent_ownership(
                child,
                "project-chat",
                &thread,
                "thread",
                Some(child),
                None,
                None,
                "{}",
                None,
                Some(false),
                "active",
                Some(false),
                "now",
            )
            .await
            .unwrap();
        state
            .store
            .save_async_question(child, &thread, "turn", "ask", "[]", i64::MAX)
            .await
            .unwrap();
    }
    // Only child questions are pending; neither has a top-level chat row.
    let observed = read_inputs(&state).await.unwrap();
    assert_eq!(observed.questions, ["project-chat".to_owned()].into());
    assert_eq!(observed.owned, ["project-chat".to_owned()].into());
    let aggregate = Tracker::default().step(&observed, 100, |_, _| Phase::Done);
    assert_eq!(aggregate.phase, Phase::NeedsInput);
    assert_eq!(aggregate.running, 1);
    assert_eq!(aggregate.needs_input, 1);
    assert_eq!(aggregate.target.as_deref(), Some("project-chat"));
    state
        .store
        .set_project_conversation_wonder_archived("project-chat", true, "now")
        .await
        .unwrap();
    let hidden = read_inputs(&state).await.unwrap();
    assert!(hidden.owned.is_empty());
    assert!(
        hidden.questions.is_empty(),
        "a hidden parent cannot contribute child questions"
    );
    state.projects.shutdown().await;
}

/// A local push Worker that records each request and answers with `status`.
struct FakeWorker {
    endpoint: String,
    /// Row reports (`.../activity`), as `{"rows":[...]}`.
    requests: Arc<Mutex<Vec<Value>>>,
    /// The registration each report came to, in order.
    registrations: Arc<Mutex<Vec<String>>>,
    /// The phone's forwarded word (`.../activity-state`).
    states: Arc<Mutex<Vec<Value>>>,
    status: Arc<Mutex<u16>>,
    /// The JSON a report is answered with.
    reply: Arc<Mutex<Value>>,
    state_reply: Arc<Mutex<Value>>,
    received: Arc<Mutex<Vec<std::time::Instant>>>,
}

const REGISTRATION: &str = "11111111-1111-4111-8111-111111111111";

async fn fake_worker() -> FakeWorker {
    let worker = FakeWorker {
        endpoint: String::new(),
        requests: Arc::default(),
        registrations: Arc::default(),
        states: Arc::default(),
        status: Arc::new(Mutex::new(200u16)),
        reply: Arc::new(Mutex::new(json!({"stored": true}))),
        state_reply: Arc::new(Mutex::new(json!({"registered": true}))),
        received: Arc::default(),
    };
    let handler = |report: bool, worker: &FakeWorker| {
        let (log, ids, states, answer, reply, state_reply, arrival) = (
            worker.requests.clone(),
            worker.registrations.clone(),
            worker.states.clone(),
            worker.status.clone(),
            worker.reply.clone(),
            worker.state_reply.clone(),
            worker.received.clone(),
        );
        move |Path(id): Path<String>, headers: axum::http::HeaderMap, Json(body): Json<Value>| {
            let (log, ids, states, answer, reply, state_reply, arrival) = (
                log.clone(),
                ids.clone(),
                states.clone(),
                answer.clone(),
                reply.clone(),
                state_reply.clone(),
                arrival.clone(),
            );
            async move {
                assert_eq!(
                    headers.get("authorization").unwrap().to_str().unwrap(),
                    format!("Bearer {}", "s".repeat(43))
                );
                let status = StatusCode::from_u16(*answer.lock().unwrap()).unwrap();
                if report {
                    arrival.lock().unwrap().push(std::time::Instant::now());
                    log.lock().unwrap().push(body);
                    ids.lock().unwrap().push(id);
                    (status, Json(reply.lock().unwrap().clone()))
                } else {
                    states.lock().unwrap().push(body);
                    (status, Json(state_reply.lock().unwrap().clone()))
                }
            }
        }
    };
    let app = Router::new()
        .route(
            "/v1/registrations/{id}/activity",
            post(handler(true, &worker)),
        )
        .route(
            "/v1/registrations/{id}/activity-state",
            post(handler(false, &worker)),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeWorker { endpoint, ..worker }
}

async fn paired(state: &AppState, worker: &FakeWorker) {
    state
        .store
        .register_push("owner", REGISTRATION, &worker.endpoint, &"s".repeat(43))
        .await
        .unwrap();
}

async fn owned_turn(state: &AppState, receipt: &str, turn: &str) -> wonder_store::StoredMessage {
    let wonder_store::MessageInsert::Inserted(message) = state
        .store
        .insert_message("owner", receipt, PROMPT, "hash", CHAT, "now")
        .await
        .unwrap()
    else {
        panic!("new receipt")
    };
    state
        .store
        .update_message_delivery(&message.id, "accepted_by_codex", Some("thread"), Some(turn))
        .await
        .unwrap();
    message
}

async fn finish(state: &AppState, message: &wonder_store::StoredMessage, outcome: &str) {
    state
        .store
        .update_message_delivery(
            &message.id,
            outcome,
            Some("thread"),
            message.codex_turn_id.as_deref(),
        )
        .await
        .unwrap();
    let delivery = match outcome {
        "completed" => DeliveryState::Completed,
        "failed" => DeliveryState::Failed,
        _ => DeliveryState::Interrupted,
    };
    crate::publish_message_state(state, message, delivery, Some("thread"), None).await;
}

async fn step(state: &AppState, tracker: &mut Tracker, now: i64) -> Aggregate {
    let aggregate = evaluate(state, tracker, now).await.unwrap();
    deliver(state, &aggregate, now).await;
    aggregate
}

fn take(worker: &FakeWorker) -> Vec<Value> {
    std::mem::take(&mut *worker.requests.lock().unwrap())
}

/// The statuses of a report's rows, in order.
fn statuses(body: &Value) -> Vec<String> {
    body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["status"].as_str().unwrap().to_owned())
        .collect()
}

/// Every key the Worker may receive; anything else could carry text.
fn assert_private(body: &Value, state: &AppState) {
    let keys: BTreeSet<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, BTreeSet::from(["rows"]), "{body}");
    for row in body["rows"].as_array().unwrap() {
        let fields: BTreeSet<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            fields,
            BTreeSet::from(["routeId", "title", "project", "status", "startedAt"]),
            "{body}"
        );
        assert!(uuid::Uuid::parse_str(row["routeId"].as_str().unwrap()).is_ok());
    }
    let text = body.to_string();
    for private in [
        CHAT,
        "SECRET",
        "payroll",
        &state.host_installation_id,
        "codex",
        "claude",
        "Codex",
        "Claude",
    ] {
        assert!(!text.contains(private), "{private} in {text}");
    }
}

#[tokio::test]
async fn host_reports_each_owned_transition_as_its_chat_row() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let worker = fake_worker().await;
    paired(&state, &worker).await;
    // An approval names its chat through the Project thread it belongs to.
    crate::test_support::bind_project_thread(
        &state,
        CHAT,
        wonder_store::AgentFamily::Codex,
        "thread",
        None,
        "now",
    )
    .await
    .unwrap();
    let mut tracker = Tracker::default();

    // Running: one Wonder-owned turn is one row.
    let message = owned_turn(&state, "receipt-1", "turn").await;
    step(&state, &mut tracker, 100).await;
    let sent = take(&worker);
    assert_eq!(sent.len(), 1);
    assert_eq!(statuses(&sent[0]), ["running"]);
    assert_eq!(sent[0]["rows"][0]["startedAt"], 100);
    assert_eq!(*worker.registrations.lock().unwrap(), [REGISTRATION]);
    // A tap resolves the route through the existing device-scoped lookup.
    let route = sent[0]["rows"][0]["routeId"].as_str().unwrap().to_owned();
    assert_eq!(
        state
            .store
            .push_route("owner", &route)
            .await
            .unwrap()
            .as_deref(),
        Some(CHAT)
    );
    assert_private(&sent[0], &state);

    // No change, no report; a quiet step re-reports before the Worker's rows lapse.
    step(&state, &mut tracker, 110).await;
    assert!(take(&worker).is_empty());
    step(&state, &mut tracker, 100 + HEARTBEAT_SECONDS).await;
    let sent = take(&worker);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["rows"][0]["routeId"],
        route.as_str(),
        "same chat keeps its route"
    );

    // Needs approval: the approval routes to this chat.
    state
        .store
        .insert_pending_approval(
            "approval-1",
            "item/commandExecution/requestApproval",
            &json!({"threadId":"thread","turnId":"turn","itemId":"item","command":"rm -rf SECRET"})
                .to_string(),
            "now",
        )
        .await
        .unwrap();
    step(&state, &mut tracker, 400).await;
    let sent = take(&worker);
    assert_eq!(statuses(&sent[0]), ["needsApproval"]);
    assert_private(&sent[0], &state);
    state
        .store
        .resolve_approval_by_server_request_id("approval-1", "accept", "now")
        .await
        .unwrap();
    step(&state, &mut tracker, 410).await;
    assert_eq!(statuses(&take(&worker)[0]), ["running"]);

    // Done only when the harness confirmed the turn; its row says when it ended.
    finish(&state, &message, "completed").await;
    step(&state, &mut tracker, 420).await;
    let sent = take(&worker);
    assert_eq!(sent.len(), 1);
    assert_eq!(statuses(&sent[0]), ["done"]);
    assert_eq!(sent[0]["rows"][0]["startedAt"], 420);
    assert_private(&sent[0], &state);
    step(&state, &mut tracker, 430).await;
    assert!(take(&worker).is_empty());

    // Interrupted reads stopped, a failure failed.
    for (receipt, outcome, status) in [
        ("receipt-2", "interrupted", "stopped"),
        ("receipt-3", "failed", "failed"),
    ] {
        let message = owned_turn(&state, receipt, receipt).await;
        step(&state, &mut tracker, 500).await;
        assert_eq!(statuses(&take(&worker)[0]), ["running"]);
        finish(&state, &message, outcome).await;
        step(&state, &mut tracker, 510).await;
        assert_eq!(statuses(&take(&worker)[0]), [status]);
    }

    // A question waits for input.
    let message = owned_turn(&state, "receipt-4", "turn-4").await;
    state
        .store
        .save_async_question(
            CHAT,
            "thread",
            "turn-4",
            "item",
            "[]",
            now_ms() as i64 + 60_000,
        )
        .await
        .unwrap();
    step(&state, &mut tracker, 600).await;
    let sent = take(&worker);
    assert_eq!(statuses(sent.last().unwrap()), ["needsInput"]);
    assert_private(sent.last().unwrap(), &state);
    finish(&state, &message, "completed").await;
}

// Contract: mute removes the chat from the rows (the Worker drops what this Mac
// stops reporting as working), and a new tracker after host restart stays silent.
#[tokio::test]
async fn chat_notification_levels_gate_live_activity_without_losing_requests() {
    use wonder_store::ChatNotificationLevel::{Default, Follow, Mute};
    for level in [Default, Follow, Mute] {
        for event in ["completed", "failed", "question", "approval"] {
            let (_dir, state) = crate::ingestion::tests::fixture().await;
            let worker = fake_worker().await;
            paired(&state, &worker).await;
            crate::test_support::bind_project_thread(
                &state,
                CHAT,
                wonder_store::AgentFamily::Codex,
                "thread",
                None,
                "now",
            )
            .await
            .unwrap();
            let message = owned_turn(&state, "receipt", "turn").await;
            assert_eq!(
                state
                    .store
                    .approval_push_conversation("thread", "turn")
                    .await
                    .unwrap()
                    .as_deref(),
                Some(CHAT)
            );
            let mut tracker = Tracker::default();
            // Also exercise switching a running chat to Mute.
            step(&state, &mut tracker, 100).await;
            assert_eq!(statuses(&take(&worker)[0]), ["running"]);
            state
                .store
                .set_chat_notification_level(CHAT, level)
                .await
                .unwrap();
            match event {
                "question" => {
                    state
                        .store
                        .save_async_question(CHAT, "thread", "turn", "ask", "[]", i64::MAX)
                        .await
                        .unwrap();
                }
                "approval" => {
                    state
                        .store
                        .insert_pending_approval(
                            "approval",
                            "item/commandExecution/requestApproval",
                            &json!({"threadId":"thread","turnId":"turn","itemId":"item"})
                                .to_string(),
                            "now",
                        )
                        .await
                        .unwrap();
                }
                outcome => finish(&state, &message, outcome).await,
            }
            for restarted in [false, true] {
                if restarted {
                    tracker = Tracker {
                        initial: state
                            .store
                            .newest_owned_outcome()
                            .await
                            .unwrap()
                            .map(|(_, outcome)| Phase::from_message_state(Some(&outcome))),
                        ..Tracker::default()
                    };
                }
                let aggregate = evaluate(&state, &mut tracker, 200).await;
                if level == Mute {
                    assert!(aggregate.is_none(), "{event}: restarted={restarted}");
                    assert!(state.live_activity.current().is_none());
                } else {
                    let aggregate = aggregate.unwrap();
                    let phase = match event {
                        "completed" => Phase::Done,
                        "failed" => Phase::Failed,
                        "question" => Phase::NeedsInput,
                        _ => Phase::NeedsApproval,
                    };
                    assert_eq!(aggregate.phase, phase, "{level:?} {event}");
                    deliver(&state, &aggregate, 200).await;
                }
            }
            let sent = take(&worker);
            if level == Mute {
                // The chat shown before the mute is dropped once, then silence.
                assert_eq!(sent.len(), 1, "{event}");
                assert!(statuses(&sent[0]).is_empty(), "{event}");
                assert_private(&sent[0], &state);
            } else {
                assert!(!sent.is_empty(), "{level:?} {event}");
            }
            if event == "approval" {
                assert_eq!(state.store.list_pending_approvals().await.unwrap().len(), 1);
            }
            if event == "question" {
                assert_eq!(
                    state
                        .store
                        .owned_activity(now_ms() as i64)
                        .await
                        .unwrap()
                        .questions
                        .len(),
                    1
                );
            }
        }
    }
}

#[tokio::test]
async fn desktop_work_is_a_row_and_the_worker_can_ask_for_another_report() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let worker = fake_worker().await;
    paired(&state, &worker).await;
    let mut tracker = Tracker::default();
    // The Worker holds a push-to-start back while the phone starts its own card.
    *worker.reply.lock().unwrap() = json!({"stored": true, "revisit": 11});

    // Work in the Codex app is a running row.
    state
        .live_activity
        .desktop_reading([(CHAT.to_string(), Desktop::Working)].into(), vec![]);
    step(&state, &mut tracker, 1_000).await;
    let sent = take(&worker);
    assert_eq!(statuses(&sent[0]), ["running"]);
    assert_private(&sent[0], &state);
    *worker.reply.lock().unwrap() = json!({"stored": true});
    step(&state, &mut tracker, 1_005).await;
    assert!(take(&worker).is_empty());
    step(&state, &mut tracker, 1_011).await;
    assert_eq!(
        statuses(&take(&worker)[0]),
        ["running"],
        "asked to report again"
    );
    step(&state, &mut tracker, 1_020).await;
    assert!(take(&worker).is_empty());

    // Waiting at the Mac, then the turn fails: the row says so without the phone.
    state
        .live_activity
        .desktop_reading([(CHAT.to_string(), Desktop::Waiting)].into(), vec![]);
    step(&state, &mut tracker, 1_040).await;
    assert_eq!(statuses(&take(&worker)[0]), ["needsInput"]);
    state
        .live_activity
        .desktop_reading(HashMap::new(), vec![(CHAT.into(), Phase::Failed)]);
    step(&state, &mut tracker, 1_050).await;
    assert_eq!(statuses(&take(&worker)[0]), ["failed"]);
}

#[tokio::test]
async fn failed_reports_back_off_and_a_phone_without_a_card_is_asked_again_later() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let worker = fake_worker().await;
    paired(&state, &worker).await;
    let mut tracker = Tracker::default();
    let _message = owned_turn(&state, "receipt", "turn").await;
    *worker.status.lock().unwrap() = 503;
    step(&state, &mut tracker, 100).await;
    assert_eq!(take(&worker).len(), 1);
    step(&state, &mut tracker, 101).await;
    assert!(
        take(&worker).is_empty(),
        "a failed report waits before retrying"
    );
    // A refused report (the registration is gone) backs off too.
    *worker.status.lock().unwrap() = 410;
    step(&state, &mut tracker, 200).await;
    assert_eq!(take(&worker).len(), 1);
    step(&state, &mut tracker, 201).await;
    assert!(take(&worker).is_empty());
    // A phone that has asked for no card stores nothing: ask again in a minute.
    *worker.status.lock().unwrap() = 200;
    *worker.reply.lock().unwrap() = json!({"stored": false});
    step(&state, &mut tracker, 400).await;
    assert_eq!(take(&worker).len(), 1);
    step(&state, &mut tracker, 430).await;
    assert!(take(&worker).is_empty());
    *worker.reply.lock().unwrap() = json!({"stored": true});
    step(&state, &mut tracker, 461).await;
    assert_eq!(statuses(&take(&worker)[0]), ["running"]);
    step(&state, &mut tracker, 470).await;
    assert!(take(&worker).is_empty());
    // Turning notifications off ends reports to that registration.
    state.store.revoke_push("owner").await.unwrap();
    step(&state, &mut tracker, 1_000).await;
    assert!(take(&worker).is_empty());
    assert!(state.live_activity.reported.lock().unwrap().is_empty());
}

// Two phones paired to this Mac each get their own routes; a computer reports to
// every registration, and no phone can resolve another's route.
#[tokio::test]
async fn every_phone_registration_gets_its_own_routes() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let worker = fake_worker().await;
    paired(&state, &worker).await;
    state
        .store
        .upsert_owner_device("phone-b", "phone-b", "{}", "now")
        .await
        .unwrap();
    state
        .store
        .register_push(
            "phone-b",
            "22222222-2222-4222-8222-222222222222",
            &worker.endpoint,
            &"s".repeat(43),
        )
        .await
        .unwrap();
    let mut tracker = Tracker::default();
    let _message = owned_turn(&state, "receipt", "turn").await;
    step(&state, &mut tracker, 100).await;
    let sent = take(&worker);
    assert_eq!(sent.len(), 2);
    assert_eq!(
        *worker.registrations.lock().unwrap(),
        [REGISTRATION, "22222222-2222-4222-8222-222222222222"]
    );
    let owner_route = sent[0]["rows"][0]["routeId"].as_str().unwrap();
    let other_route = sent[1]["rows"][0]["routeId"].as_str().unwrap();
    assert_ne!(owner_route, other_route);
    assert!(state
        .store
        .push_route("owner", owner_route)
        .await
        .unwrap()
        .is_some());
    assert!(state
        .store
        .push_route("owner", other_route)
        .await
        .unwrap()
        .is_none());
    assert!(state
        .store
        .push_route("phone-b", other_route)
        .await
        .unwrap()
        .is_some());
}

/// The running loop reports the end within seconds of the harness ending,
/// with no phone involved.
#[tokio::test]
async fn the_loop_reports_the_end_within_seconds_of_the_turn() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let task = spawn(state.clone());
    state.live_activity.wake.notify_one();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !state
            .live_activity
            .milestones
            .lock()
            .unwrap()
            .iter()
            .any(|(name, _)| *name == "observation"),
        "zero registrations must not load activity inputs"
    );
    let worker = fake_worker().await;
    paired(&state, &worker).await;
    let message = owned_turn(&state, "receipt", "turn").await;
    state.live_activity.wake.notify_one();
    let wait = |status: &'static str| {
        let requests = worker.requests.clone();
        async move {
            let started = std::time::Instant::now();
            loop {
                if requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|body| statuses(body).iter().any(|s| s == status))
                {
                    return started.elapsed();
                }
                assert!(
                    started.elapsed() < Duration::from_secs(60),
                    "no {status} report"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    wait("running").await;
    state.live_activity.milestones.lock().unwrap().clear();
    worker.received.lock().unwrap().clear();
    let mutation = std::time::Instant::now();
    let utc = chrono::Utc::now();
    finish(&state, &message, "completed").await;
    let mutated = std::time::Instant::now();
    let owned = wait("done").await;
    let milestones = state.live_activity.milestones.lock().unwrap().clone();
    let at = |stage: &str| {
        milestones
            .iter()
            .find(|(name, _)| *name == stage)
            .unwrap()
            .1
    };
    let (observed, event, aggregated) = (at("observation"), at("event"), at("aggregation"));
    let submitted = worker.received.lock().unwrap()[0];
    eprintln!("DAEMON_LATENCY utc={utc} mutation_ms=0 stored_ms={:.3} event_ms={:.3} read_ms={:.3} aggregation_ms={:.3} submission_ms={:.3}",
        mutated.duration_since(mutation).as_secs_f64() * 1000.0,
        event.duration_since(mutation).as_secs_f64() * 1000.0,
        observed.duration_since(mutation).as_secs_f64() * 1000.0,
        aggregated.duration_since(mutation).as_secs_f64() * 1000.0,
        submitted.duration_since(mutation).as_secs_f64() * 1000.0);
    assert!(
        owned < Duration::from_secs(60),
        "owned turn end took {owned:?}"
    );
    // Desktop work ends through the watcher's reading.
    take(&worker);
    state
        .live_activity
        .desktop_reading([(CHAT.to_string(), Desktop::Working)].into(), vec![]);
    wait("running").await;
    state.live_activity.milestones.lock().unwrap().clear();
    worker.received.lock().unwrap().clear();
    let started = std::time::Instant::now();
    state
        .live_activity
        .desktop_reading(HashMap::new(), vec![(CHAT.into(), Phase::Stopped)]);
    wait("stopped").await;
    eprintln!(
        "live activity end: owned {owned:?}, desktop {:?} after the watcher's reading",
        started.elapsed()
    );
    let milestones = state.live_activity.milestones.lock().unwrap().clone();
    let at = |stage: &str| {
        milestones
            .iter()
            .find(|(name, _)| *name == stage)
            .unwrap()
            .1
    };
    let (wake, observed, aggregated) = (at("wake"), at("observation"), at("aggregation"));
    let submitted = worker.received.lock().unwrap()[0];
    eprintln!("DESKTOP_LATENCY utc={} mutation_ms=0 wake_ms={:.3} read_ms={:.3} aggregation_ms={:.3} submission_ms={:.3}", chrono::Utc::now(),
        wake.duration_since(started).as_secs_f64() * 1000.0,
        observed.duration_since(started).as_secs_f64() * 1000.0,
        aggregated.duration_since(started).as_secs_f64() * 1000.0,
        submitted.duration_since(started).as_secs_f64() * 1000.0);
    task.abort();
}

/// A terminal report must make progress before an event burst ends, without
/// producing repeated reports. Uses the same real store and loop.
#[tokio::test]
async fn terminal_submission_is_not_starved_by_sustained_events() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let worker = fake_worker().await;
    paired(&state, &worker).await;
    let message = owned_turn(&state, "burst-receipt", "turn").await;
    let task = spawn(state.clone());
    let done = |requests: &Arc<Mutex<Vec<Value>>>| {
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|body| statuses(body).iter().any(|s| s == "done"))
    };
    let began = std::time::Instant::now();
    while !worker
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|body| statuses(body).iter().any(|s| s == "running"))
    {
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "Missing running report"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mutation = std::time::Instant::now();
    finish(&state, &message, "completed").await;
    let burst_state = state.clone();
    let burst_requests = worker.requests.clone();
    let burst = tokio::spawn(async move {
        // Sustain events until the terminal report lands. A starved report
        // lands only after the burst gives up at the hang guard.
        let until = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < until
            && !burst_requests
                .lock()
                .unwrap()
                .iter()
                .any(|body| statuses(body).iter().any(|s| s == "done"))
        {
            crate::publish_event(
                &burst_state,
                wonder_api::WonderEvent::HostStatus {
                    state: "ready".into(),
                },
            )
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::time::Instant::now()
    });
    while !done(&worker.requests) {
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "Missing done report"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let submitted = *worker.received.lock().unwrap().last().unwrap();
    let burst_ended = burst.await.unwrap();
    assert!(
        submitted < burst_ended,
        "Terminal submission waited for the burst to end"
    );
    let requests = worker.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|body| statuses(body).iter().any(|s| s == "done"))
            .count(),
        1
    );
    assert_private(requests.last().unwrap(), &state);
    eprintln!(
        "STREAM_BOUND utc={} terminal_ms={:.3} burst_ended_ms={:.3} end_submissions=1",
        chrono::Utc::now(),
        submitted.duration_since(mutation).as_secs_f64() * 1000.0,
        burst_ended.duration_since(mutation).as_secs_f64() * 1000.0
    );
    task.abort();
}

#[tokio::test]
async fn phones_forward_their_card_state_with_a_signed_action_over_their_pairing() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    let worker = fake_worker().await;
    let key = SigningKey::from_slice(&[23u8; 32]).unwrap();
    let point = key.verifying_key().to_sec1_point(false);
    state.pairing.lock().await.restore_device(
        "owner".into(),
        wonder_api::pairing_protocol::DevicePublicKeyJwk {
            kty: "EC".into(),
            crv: "P-256".into(),
            x: base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                point.x().unwrap(),
            ),
            y: base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                point.y().unwrap(),
            ),
        },
        false,
        None,
    );
    let path = "/api/v1/push/live-activity";
    let app = Router::new()
        .route(path, post(register))
        .layer(Extension(AuthenticatedDevice {
            device_id: "owner".into(),
            session_binding: "phone-session".into(),
        }))
        .with_state(state.clone());
    let issued = std::cell::Cell::new(now_ms());
    let signed = |kind: &str, token: &str| {
        issued.set(issued.get() + 1);
        let nonce = format!("nonce-{}", issued.get());
        let hash = action_body_sha256(&json!([kind, token]));
        let transcript = ActionTranscript {
            action: "push.liveActivity",
            target: path,
            body_sha256: &hash,
            action_nonce: &nonce,
            session_binding: "phone-session",
            device_id: "owner",
            host_installation_id: &state.host_installation_id,
            issued_at_ms: issued.get(),
            expected_state: "active",
        };
        let signature: Signature = key.sign(&transcript.to_bytes());
        json!({"kind":kind,"token":token,"actionNonce":nonce,"issuedAtMs":issued.get(),
            "signature":base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, signature.to_bytes())})
    };
    let call = |body: Value| {
        let app = app.clone();
        async move {
            let response = app
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
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    let forwarded = || std::mem::take(&mut *worker.states.lock().unwrap());
    // Without notifications for this Mac, the Worker could not push to the phone.
    assert_eq!(call(signed("update", TOKEN)).await.0, StatusCode::CONFLICT);
    paired(&state, &worker).await;
    let mut tampered = signed("update", TOKEN);
    tampered["token"] = START_TOKEN.into();
    assert_eq!(call(tampered).await.0, StatusCode::UNAUTHORIZED);
    for (kind, token) in [
        ("other", TOKEN),
        ("update", "not-hex"),
        ("ended", ""),
        ("dismissed", TOKEN),
        ("foreground", TOKEN),
    ] {
        assert_eq!(
            call(signed(kind, token)).await.0,
            StatusCode::BAD_REQUEST,
            "{kind}"
        );
    }
    assert!(forwarded().is_empty());

    // A token registers with the Worker, which answers with the combined state.
    *worker.state_reply.lock().unwrap() =
        json!({"registered": true, "state": {"phase":"running","running":3,"needsInput":0}});
    let (status, body) = call(signed("update", TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"registered":true,"state":{"phase":"running","running":3,"needsInput":0}})
    );
    *worker.state_reply.lock().unwrap() = json!({"registered": true});
    assert_eq!(call(signed("start", START_TOKEN)).await.0, StatusCode::OK);
    // The owner swiped the card away, then opened Cupola again.
    *worker.state_reply.lock().unwrap() = json!({"registered": false, "dismissed": true});
    let (_, body) = call(signed("dismissed", "")).await;
    assert_eq!(body, json!({"registered":false}));
    let (_, body) = call(signed("update", TOKEN)).await;
    assert_eq!(body, json!({"registered":false,"dismissed":true}));
    *worker.state_reply.lock().unwrap() = json!({"dismissed": false});
    assert_eq!(
        call(signed("foreground", "")).await.1,
        json!({"registered":false})
    );
    *worker.state_reply.lock().unwrap() = json!({"registered": false});
    assert_eq!(call(signed("ended", TOKEN)).await.0, StatusCode::OK);
    // Turning Live Activities off forgets the card and its rows.
    assert_eq!(call(signed("start", "")).await.0, StatusCode::OK);
    assert_eq!(
        forwarded(),
        [
            json!({"kind":"update","token":TOKEN}),
            json!({"kind":"start","token":START_TOKEN}),
            json!({"kind":"dismissed"}),
            json!({"kind":"update","token":TOKEN}),
            json!({"kind":"foreground"}),
            json!({"kind":"ended","token":TOKEN}),
            json!({"kind":"off"}),
        ]
    );
    // The Worker being unreachable is retried by the phone.
    *worker.status.lock().unwrap() = 503;
    assert_eq!(
        call(signed("update", TOKEN)).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

// Protocol contract: rows list what needs the owner first and keep a finished
// chat for a short Done moment. The Worker caps the card at five rows.
#[test]
fn rows_rank_cap_and_linger_for_the_report() {
    let mut tracker = Tracker::default();
    let names: Vec<String> = (0..25).map(|i| format!("chat-{i:02}")).collect();
    let owned: Vec<&str> = names.iter().map(String::as_str).collect();
    let observed = inputs(&owned, &[], &["chat-03"], &[]);
    let first = tracker.step(&observed, 100, |_, _| Phase::Done);
    assert_eq!(first.rows.len(), ROW_LIMIT);
    assert_eq!(first.more, 5);
    assert_eq!(first.rows[0].thread, "chat-03");
    assert_eq!(first.rows[0].status, Phase::NeedsApproval);
    assert!(first.rows[1..].iter().all(|r| r.status == Phase::Running));

    // One finishes: a Done row is reported for a short while, then drops out.
    let mut tracker = Tracker::default();
    let mut observed = inputs(&["a", "b", "c"], &[], &[], &[]);
    tracker.step(&observed, 100, |_, _| Phase::Done);
    observed.owned.remove("a");
    let next = tracker.step(&observed, 110, |_, _| Phase::Done);
    assert_eq!(
        next.rows.iter().map(|r| r.status).collect::<Vec<_>>(),
        [Phase::Running, Phase::Running, Phase::Done]
    );
    assert_eq!(next.phase, Phase::Running);
    let later = tracker.step(&observed, 110 + ROW_FINISHED_SECONDS, |_, _| Phase::Done);
    assert!(later.rows.iter().all(|r| !r.status.is_finished()));
    assert_eq!(later.rows.len(), 2);
}

// Long names are cut before they leave the Mac; a row stays small.
#[test]
fn report_rows_cut_long_names() {
    let row = ReportRow {
        route_id: uuid::Uuid::new_v4().to_string(),
        title: shorten(&"é".repeat(500), TITLE_CHARS),
        project: shorten(&format!("a\n\t{}", "p".repeat(500)), PROJECT_CHARS),
        status: Phase::Running,
        started_at: 1_800_000_000,
    };
    assert_eq!(row.title.chars().count(), 40);
    assert_eq!(row.project.chars().count(), 24);
    assert!(row.title.ends_with('…') && !row.project.contains('\n'));
    assert!(serde_json::to_string(&row).unwrap().len() < 512);
}

// The chat's last row is the one that ended, with how it ended.
#[test]
fn the_final_state_keeps_the_rows_that_just_ended() {
    let mut tracker = Tracker::default();
    tracker.step(&inputs(&["a", "b"], &[], &[], &[]), 100, |_, _| Phase::Done);
    let end = tracker.step(&inputs(&[], &[], &[], &[]), 105, |id, _| {
        if id == "a" {
            Phase::Failed
        } else {
            Phase::Done
        }
    });
    assert_eq!(end.phase, Phase::Failed);
    assert_eq!(end.rows.len(), 2);
    assert_eq!(end.rows[0].status, Phase::Failed);
    assert_eq!(end.rows[1].status, Phase::Done);
}

// Contract: a thread that left the inputs with no recorded outcome is
// Stopped, never Done: missing evidence is not a finished turn.
#[test]
fn an_ended_thread_without_an_outcome_is_not_done() {
    let mut outcomes = HashMap::new();
    outcomes.insert("known".to_string(), Phase::Failed);
    assert_eq!(ended_phase(&outcomes, "known"), Phase::Failed);
    assert_eq!(ended_phase(&outcomes, "unknown"), Phase::Stopped);
    assert_ne!(ended_phase(&outcomes, "unknown"), Phase::Done);
}
