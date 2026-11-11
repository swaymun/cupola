//! Delivery of a message that carries the other provider's model, against a
//! fake Codex runtime and a fake Claude bridge.
use crate::projects::tests::handoff_fixture;
use crate::provider_switch::{count_positions, is_handoff_text};
use crate::AppState;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;
use wonder_app_server::AppServerClient;
use wonder_store::{AgentFamily, HandoffDelivery, MessageInsert, StoredMessage};

pub(crate) const CLAUDE_THREAD: &str = "claude-aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
pub(crate) const CLAUDE_SESSION: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

pub(crate) struct Fx {
    pub(crate) chat: String,
    pub(crate) dir: tempfile::TempDir,
    pub(crate) state: AppState,
    _service: crate::ingestion::NotificationService,
}

pub(crate) fn model(id: &str, family: AgentFamily) -> crate::ModelOption {
    crate::ModelOption {
        agent_family: family,
        capabilities: crate::ModelCapabilities::for_family(family),
        id: id.into(),
        display_name: if family == AgentFamily::Claude {
            "Claude Sonnet".into()
        } else {
            "GPT Fake".into()
        },
        description: None,
        model_specialty: None,
        hidden: false,
        reasoning_efforts: vec![],
        default_reasoning_effort: None,
        service_tiers: vec![],
        default_service_tier: Some("default".into()),
        is_default: false,
        provider_default: None,
        native_ids: vec![],
    }
}

pub(crate) async fn fx() -> Fx {
    let (dir, mut state, first) = handoff_fixture().await;
    // The fixture's own message is long finished.
    state
        .store
        .update_message_delivery(&first.id, "completed", Some("thread"), Some("turn"))
        .await
        .unwrap();
    // The fake Codex runtime reports history from items.json and can hand out
    // a chosen thread id.
    let script = dir.path().join("runtime.py");
    let source = std::fs::read_to_string(&script).unwrap();
    let start = "elif method == 'thread/start': result = {'thread':{'id':'thread'}}";
    let items = "    elif method == 'thread/items/list':\n        if os.path.exists(root + '/long-history'):";
    let resume = "elif method == 'thread/resume': result = {'thread':child_thread()} if r.get('params',{}).get('threadId') == 'child-thread' else {'thread':{'status':{'type':'idle'}}}";
    let list = "\n    elif method == 'thread/list':\n        if os.path.exists(root + '/archive-fixture'):";
    assert!(
        source.contains(start)
            && source.contains(items)
            && source.contains(resume)
            && source.contains(list)
    );
    let folder = state.store.project("project").await.unwrap().unwrap().roots[0]
        .path
        .clone();
    std::fs::write(
        dir.path().join("idle-fixture.json"),
        json!({"thread":{"id":"thread","status":{"type":"idle"},"cwd":folder},
            "codex-main":{"id":"codex-main","status":{"type":"idle"},"cwd":folder},
            "fresh-codex":{"id":"fresh-codex","status":{"type":"idle"},"cwd":folder}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        &script,
        source
            .replace(
                resume,
                "elif method == 'thread/resume': result = {'thread':{'id':r.get('params',{}).get('threadId'),'status':{'type':'idle'}}}",
            )
            .replace(
                start,
                "elif method == 'thread/start': result = {'thread':{'id':open(root + '/next-thread').read().strip() if os.path.exists(root + '/next-thread') else 'thread'}}",
            )
            .replacen(
                list,
                "\n    elif method == 'thread/list' and os.path.exists(root + '/codex-threads.json'):\n        result = {'data':(json.load(open(root + '/codex-archived.json')) if os.path.exists(root + '/codex-archived.json') else []) if r.get('params',{}).get('archived',False) else json.load(open(root + '/codex-threads.json')),'nextCursor':None}\n    elif method == 'thread/list':\n        if os.path.exists(root + '/archive-fixture'):",
                1,
            )
            .replace(
                items,
                "    elif method == 'thread/items/list' and os.path.exists(root + '/codex-items.json'):\n        result = {'data':json.load(open(root + '/codex-items.json')).get(r['params']['threadId'], []),'nextCursor':None}\n    elif method == 'thread/items/list':\n        if os.path.exists(root + '/long-history'):",
            ),
    )
    .unwrap();
    state.projects.shutdown().await;
    let bridge = dir.path().join("claude-bridge.py");
    std::fs::write(
        &bridge,
        format!(
            r#"import json, sys, os, subprocess
root = os.path.dirname(__file__)
n = 0
# A benchmark can answer the catalog from a real runtime reader: the file holds
# {{"argv": [...], "env": {{...}}}} and every project/sessions/list goes there.
native = None
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); p = r.get('params', {{}}); result = {{}}
    with open(root + '/claude-requests', 'a') as log: log.write(json.dumps(r) + '\n')
    if m == 'initialize': result = {{'wonderBridge': {{'protocolVersion': 1, 'family': 'claude'}}, 'capabilities': {{'experimentalApi': True}}}}
    elif m == 'thread/loaded/list' and os.path.exists(root + '/claude-hold-roster'): continue
    elif m == 'project/sessions/list' and os.path.exists(root + '/claude-native-list'):
        if native is None:
            spec = json.load(open(root + '/claude-native-list'))
            native = subprocess.Popen(spec['argv'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=dict(os.environ, **spec['env']))
        native.stdin.write(json.dumps(r) + '\n'); native.stdin.flush()
        answer = json.loads(native.stdout.readline())
        if 'error' in answer:
            print(json.dumps({{'id': r['id'], 'error': answer['error']}}), flush=True)
            continue
        result = answer['result']
    elif m and os.path.exists(root + '/claude-reply-' + m.replace('/', '-') + '.json'):
        result = json.load(open(root + '/claude-reply-' + m.replace('/', '-') + '.json'))
    elif m == 'thread/start':
        n += 1
        tid = open(root + '/next-claude').read().strip() if os.path.exists(root + '/next-claude') else '{CLAUDE_THREAD}'
        result = {{'thread': {{'id': tid, 'sessionId': tid[len('claude-'):]}}}}
    elif m == 'turn/start':
        if os.path.exists(root + '/claude-boom'):
            print(json.dumps({{'id': r['id'], 'error': {{'code': -32000, 'message': 'Claude stopped unexpectedly'}}}}), flush=True)
            continue
        if os.path.exists(root + '/claude-busy'):
            print(json.dumps({{'id': r['id'], 'error': {{'code': -32000, 'message': 'Native session still active.', 'data': {{'turnStart': {{'accepted': False, 'action': 'wait'}}}}}}}}), flush=True)
            continue
        if os.path.exists(root + '/claude-running'):
            print(json.dumps({{'id': r['id'], 'error': {{'code': -32000, 'message': 'Native reply still active.', 'data': {{'turnStart': {{'accepted': False, 'action': 'wait'}}}}}}}}), flush=True)
            continue
        result = {{'turn': {{'id': 'claude-turn-' + str(os.path.getsize(root + '/claude-requests'))}}}}
    elif m == 'turn/steer':
        if os.path.exists(root + '/claude-steer-untyped'):
            print(json.dumps({{'id': r['id'], 'error': {{'code': -32000, 'message': 'This reply already finished. Diagnostic only.', 'data': {{'turnStart': {{'accepted': False, 'action': 'wait'}}}}}}}}), flush=True)
            continue
        if os.path.exists(root + '/claude-steer-finished'):
            print(json.dumps({{'id': r['id'], 'error': {{'code': -32000, 'message': 'Native reply finished.', 'data': {{'turnSteer': {{'accepted': False, 'action': 'wait'}}}}}}}}), flush=True)
            continue
        result = {{'turnId': 'claude-guide-turn'}}
    elif m == 'thread/items/list':
        data = json.load(open(root + '/claude-items.json')).get(p.get('threadId'), []) if os.path.exists(root + '/claude-items.json') else []
        result = {{'data': data, 'nextCursor': None}}
    elif m == 'thread/read': result = {{'thread': {{'id': p.get('threadId'), 'sessionId': p.get('threadId', '')[len('claude-'):], 'status': {{'type': 'active' if os.path.exists(root + '/claude-active') else 'idle'}}}}}}
    elif m == 'thread/turns/list':
        data = json.load(open(root + '/claude-turns.json')) if os.path.exists(root + '/claude-turns.json') else []
        result = {{'data': data, 'nextCursor': None}}
    elif m == 'thread/rewind':
        if os.path.exists(root + '/claude-rewind-refused'):
            print(json.dumps({{'id': r['id'], 'error': {{'code': -32000, 'message': open(root + '/claude-rewind-refused').read()}}}}), flush=True)
            continue
        result = json.load(open(root + '/claude-rewind.json')) if os.path.exists(root + '/claude-rewind.json') else {{'removedTurnIds': [p.get('turnId')], 'filesRestored': False}}
    print(json.dumps({{'id': r['id'], 'result': result}}), flush=True)
"#
        ),
    )
    .unwrap();
    let config = wonder_app_server::BridgeLaunchConfig {
        node_bin: "/usr/bin/python3".into(),
        entrypoint: bridge,
        state_dir: dir.path().join("claude-state"),
        npm_cli: None,
        claude_config_dir: None,
        wonder_version: "test".into(),
        family: "claude",
    };
    let client = Arc::new(Mutex::new(AppServerClient::unavailable(
        "test".into(),
        crate::ingestion::notification_sink(state.store.clone()),
    )));
    client
        .lock()
        .await
        .restart_bridge(config.clone())
        .await
        .unwrap();
    state.claude = Some(Arc::new(crate::claude::Runtime { client, config }));
    // A conversation with a real id, bound to its own Codex thread.
    let chat = uuid::Uuid::new_v4().to_string();
    let project = state.store.project("project").await.unwrap().unwrap();
    state
        .store
        .create_project_conversation(wonder_store::ProjectConversationInsert {
            conversation_id: &chat,
            project_id: "project",
            family: AgentFamily::Codex,
            provider_store: &state.projects.codex_store,
            native_session_id: Some("codex-main"),
            cwd: &project.roots[0].path,
            roots_revision: project.roots_revision,
            title: "Main",
            model: Some("fake"),
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
    state
        .store
        .bind_project_runtime(
            &chat,
            AgentFamily::Codex,
            &state.projects.codex_store,
            "codex-main",
            None,
            "now",
        )
        .await
        .unwrap();
    let service = crate::ingestion::spawn(state.clone()).await;
    for _ in 0..6000 {
        if state.ingestion.project_readiness(&state.store).await.ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Fx {
        chat,
        dir,
        state,
        _service: service,
    }
}

fn text_items(pairs: &[(&str, &str)]) -> Value {
    Value::Array(
        pairs
            .iter()
            .enumerate()
            .map(|(i, (role, text))| {
                let item = if *role == "user" {
                    json!({"type":"userMessage","id":format!("u{i}"),"content":[{"type":"text","text":text}]})
                } else {
                    json!({"type":"agentMessage","id":format!("a{i}"),"text":text})
                };
                json!({"turnId":"turn","item":item})
            })
            .collect(),
    )
}

impl Fx {
    pub(crate) fn write(&self, name: &str, value: &Value) {
        std::fs::write(self.dir.path().join(name), value.to_string()).unwrap();
    }

    pub(crate) fn claude_requests(&self, method: &str) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("claude-requests"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|request| request["method"] == method)
            .collect()
    }

    pub(crate) fn codex_requests(&self, method: &str) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("requests-jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|request| request["method"] == method)
            .collect()
    }

    pub(crate) async fn thread(&self) -> wonder_store::StoredProjectConversation {
        self.state
            .store
            .project_conversation(&self.chat)
            .await
            .unwrap()
            .unwrap()
    }

    /// Accepts a message the way the send route does, optionally with a model.
    pub(crate) async fn queue(
        &self,
        text: &str,
        with: Option<(AgentFamily, &str)>,
    ) -> StoredMessage {
        let client = uuid::Uuid::new_v4().to_string();
        if let Some((family, model)) = with {
            self.state
                .store
                .stage_project_message_target(
                    "owner",
                    &client,
                    family,
                    model,
                    None,
                    None,
                    "2026-01-01T00:00:00Z",
                )
                .await
                .unwrap();
        }
        let MessageInsert::Inserted(message) = self
            .state
            .store
            .insert_dispatch_message(
                "owner",
                &client,
                text,
                "hash",
                &self.chat,
                &[],
                "2026-01-01T00:00:00Z",
                true,
            )
            .await
            .unwrap()
        else {
            panic!("new message")
        };
        message
    }

    /// Starting a runtime refreshes the catalog from the fake provider, so the
    /// two models are offered again before each delivery.
    pub(crate) async fn offer_models(&self) {
        let _ = crate::projects::codex_rpc(&self.state).await;
        let mut catalog = self.state.runtime_catalog.write().await;
        catalog
            .models
            .retain(|m| m.id != "fake" && m.id != "claude:sonnet");
        catalog.models.push(model("fake", AgentFamily::Codex));
        catalog
            .models
            .push(model("claude:sonnet", AgentFamily::Claude));
    }

    /// Releases the message for delivery, as the dispatcher does.
    pub(crate) async fn release(
        &self,
        message: &StoredMessage,
    ) -> Result<(String, String, String), String> {
        self.offer_models().await;
        assert!(self
            .state
            .store
            .claim_message_for_dispatch(&message.id)
            .await
            .unwrap());
        let message = self
            .state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        let mut submitting = false;
        crate::projects::dispatch_inner(&self.state, &message, &mut submitting).await
    }

    pub(crate) async fn finish(&self, message: &StoredMessage) {
        self.state
            .store
            .update_message_delivery(&message.id, "completed", None, None)
            .await
            .unwrap();
    }
}

fn turn_input(request: &Value) -> Vec<Value> {
    request["params"]["input"].as_array().unwrap().clone()
}

fn text_of(part: &Value) -> &str {
    part["text"].as_str().unwrap()
}

// Contract: choosing the other provider's model for a message moves the thread
// when that message is released; the new session receives the earlier
// conversation once, and the owner's text is untouched.
#[tokio::test]
async fn a_message_with_claudes_model_starts_a_claude_session_with_the_history_once() {
    let fx = fx().await;
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", "fix the parser"), ("agent", "parser fixed")])}),
    );
    let mut events = fx.state.events.subscribe();
    let first = fx
        .queue(
            "now continue in Claude",
            Some((AgentFamily::Claude, "claude:sonnet")),
        )
        .await;
    fx.release(&first).await.unwrap();

    let thread = fx.thread().await;
    assert_eq!(thread.family, AgentFamily::Claude);
    assert_eq!(thread.model.as_deref(), Some("claude:sonnet"));
    // Moving the thread is the owner's choice of Claude's model in this
    // Project: its next new Claude chat starts there.
    let project = fx
        .state
        .store
        .project(&thread.project_id)
        .await
        .unwrap()
        .unwrap();
    assert!(project
        .last_models
        .contains(&wonder_store::ProjectLastModel {
            family: AgentFamily::Claude,
            model: "claude:sonnet".into(),
            effort: thread.effort.clone(),
        }));
    assert_eq!(thread.native_session_id.as_deref(), Some(CLAUDE_SESSION));
    assert_eq!(fx.claude_requests("thread/start").len(), 1);
    let starts = fx.claude_requests("turn/start");
    assert_eq!(starts.len(), 1);
    let input = turn_input(&starts[0]);
    assert_eq!(input.len(), 2, "briefing part then the owner's message");
    assert!(is_handoff_text(text_of(&input[0])));
    assert!(text_of(&input[0]).contains("fix the parser"));
    assert!(text_of(&input[0]).contains("parser fixed"));
    assert!(text_of(&input[0]).contains("wonder_thread_read"));
    assert_eq!(text_of(&input[1]), "now continue in Claude");
    // New Claude sessions get the thread tools to read what was left out.
    assert!(starts[0]["params"]["dynamicTools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "wonder_thread_read"));
    // Delivered and recorded; the old Codex session is remembered.
    assert!(fx
        .state
        .store
        .pending_handoff(&fx.chat)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        fx.state
            .store
            .handoff_delivery(&fx.chat, CLAUDE_THREAD)
            .await
            .unwrap(),
        Some(HandoffDelivery::Injected)
    );
    let history = fx
        .state
        .store
        .project_runtime_history(&fx.chat)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].family, AgentFamily::Codex);
    // The timeline got a switch row (a thread item, not a message), updated with counts.
    let mut rows = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let wonder_api::WonderEvent::Activity {
            category,
            detail: Some(detail),
            ..
        } = event.event
        {
            if category == "thread_item_upsert" {
                let detail: Value = serde_json::from_str(&detail).unwrap();
                if detail["item"]["type"] == "providerSwitch" {
                    rows.push(detail["item"]["text"].as_str().unwrap().to_owned());
                }
            }
        }
    }
    assert!(rows.len() >= 2, "{rows:?}");
    assert_eq!(
        rows.last().unwrap(),
        "Switched to Claude Sonnet · Codex history was handed over (2 messages)"
    );

    // The next message is an ordinary turn: the briefing is not sent again.
    fx.finish(&first).await;
    let second = fx.queue("and now this", None).await;
    fx.release(&second).await.unwrap();
    let starts = fx.claude_requests("turn/start");
    assert_eq!(starts.len(), 2);
    assert_eq!(turn_input(&starts[1]).len(), 1);
    assert_eq!(fx.claude_requests("thread/start").len(), 1);
}

// Contract: the switch line is part of stored history, so a client that
// reloads the conversation sees it once, with the families it switched between.
#[tokio::test]
async fn the_switch_line_survives_a_history_reload() {
    let fx = fx().await;
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", "fix the parser"), ("agent", "parser fixed")])}),
    );
    let first = fx
        .queue("to claude", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    fx.release(&first).await.unwrap();

    let response = crate::history::conversation_snapshot(
        axum::extract::State(fx.state.clone()),
        axum::extract::Path(fx.chat.clone()),
        axum::extract::Query(crate::history::HistoryQuery::default()),
        axum::http::HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let snapshot: Value = serde_json::from_slice(&body).unwrap();
    let switches: Vec<&Value> = snapshot["thread"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|turn| turn["items"].as_array().unwrap())
        .filter(|item| item["type"] == "providerSwitch")
        .collect();
    assert_eq!(switches.len(), 1, "{switches:?}");
    let payload = &switches[0]["payload"];
    assert_eq!(payload["fromFamily"], "codex");
    assert_eq!(payload["toFamily"], "claude");
    assert_eq!(
        switches[0]["text"],
        "Switched to Claude Sonnet · Codex history was handed over (2 messages)"
    );
}

async fn timeline(fx: &Fx) -> Vec<String> {
    let response = crate::history::conversation_snapshot(
        axum::extract::State(fx.state.clone()),
        axum::extract::Path(fx.chat.clone()),
        axum::extract::Query(crate::history::HistoryQuery::default()),
        axum::http::HeaderMap::new(),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let snapshot = json_of(response).await;
    snapshot["thread"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|turn| turn["items"].as_array().unwrap())
        .map(|item| match item["type"].as_str() {
            Some("providerSwitch") => "switch".to_owned(),
            _ => item["text"].as_str().unwrap_or_default().to_owned(),
        })
        .collect()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The owner's history refresh. History reads share two slots across the
/// host (and this test process), so a busy answer is asked again.
async fn refresh_history(fx: &Fx) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let response = crate::history::refresh_history(
                axum::extract::State(fx.state.clone()),
                axum::extract::Path(fx.chat.clone()),
            )
            .await;
            if response.status() == axum::http::StatusCode::ACCEPTED {
                break;
            }
            assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the refresh starts");
}

/// Waits until no history read holds the chat.
async fn history_settled(fx: &Fx) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while fx
            .state
            .store
            .history_refresh_status(&fx.chat, now_ms())
            .await
            .unwrap()["state"]
            == "refreshing"
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("history reads finish");
}

/// History reads of the chat wait before reading each page while held.
fn hold_history_pages(fx: &Fx, on: bool) {
    let mut guard = crate::history::PAGE_HOLD.lock().unwrap();
    let (held, reached) = guard.get_or_insert_with(Default::default);
    if on {
        held.insert(fx.chat.clone());
    } else {
        held.remove(&fx.chat);
        reached.remove(&fx.chat);
    }
}

async fn history_read_paused(fx: &Fx) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while !crate::history::PAGE_HOLD
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(_, reached)| reached.contains(&fx.chat))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a history read pauses");
}

/// The Mac turns appear once each, in order, before the switch row.
async fn mac_turns_once_before_the_switch(fx: &Fx) {
    assert!(fx
        .state
        .store
        .history_imports_owed(&fx.chat)
        .await
        .unwrap()
        .is_empty());
    let rows = timeline(fx).await;
    let at = |text: &str| {
        let found: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| *row == text)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(found.len(), 1, "{text} once in {rows:?}");
        found[0]
    };
    assert!(at("made on the mac") < at("mac reply"), "{rows:?}");
    assert!(at("mac reply") < at("switch"), "{rows:?}");
}

// Contract: when the old session's history cannot be saved during the switch
// (another refresh holds the thread), it stays owed across the switch until
// an import can read it, and arrives once, before the switch row.
#[tokio::test]
async fn history_a_switch_could_not_save_is_imported_later_before_the_switch_row() {
    let fx = fx().await;
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", "made on the mac"), ("agent", "mac reply")])}),
    );
    // A refresh already holds the thread, so the switch's own save gives up;
    // the import that follows pauses before reading.
    assert!(fx
        .state
        .store
        .claim_history_refresh(&fx.chat, "elsewhere", now_ms())
        .await
        .unwrap());
    hold_history_pages(&fx, true);
    let first = fx
        .queue("to claude", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    fx.release(&first).await.unwrap();
    fx.finish(&first).await;
    history_read_paused(&fx).await;
    let owed = fx.state.store.history_imports_owed(&fx.chat).await.unwrap();
    assert_eq!(owed.len(), 1, "the left session's history is still owed");
    assert_eq!(owed[0].runtime_thread_id.as_deref(), Some("codex-main"));
    assert!(!timeline(&fx).await.contains(&"mac reply".to_owned()));
    hold_history_pages(&fx, false);
    history_settled(&fx).await;

    // The owner's refreshes import anything still owed and add nothing twice.
    refresh_history(&fx).await;
    history_settled(&fx).await;
    refresh_history(&fx).await;
    history_settled(&fx).await;
    mac_turns_once_before_the_switch(&fx).await;
}

// Contract: a history read that started before a switch and reads after it
// cannot place the left session's items after the switch row; they arrive
// once, before it.
#[tokio::test]
async fn a_history_read_paused_across_a_switch_saves_nothing_after_the_switch_row() {
    let fx = fx().await;
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", "made on the mac"), ("agent", "mac reply")])}),
    );
    // The owner's refresh of the Codex session started before the switch
    // and pauses before reading its page.
    hold_history_pages(&fx, true);
    refresh_history(&fx).await;
    history_read_paused(&fx).await;
    let first = fx
        .queue("to claude", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    fx.release(&first).await.unwrap();
    fx.finish(&first).await;
    hold_history_pages(&fx, false);

    tokio::time::timeout(Duration::from_secs(60), async {
        while !timeline(&fx).await.contains(&"mac reply".to_owned()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the Mac turns reach the timeline");
    history_settled(&fx).await;
    // Whatever is still owed is imported by the next refresh.
    refresh_history(&fx).await;
    history_settled(&fx).await;
    mac_turns_once_before_the_switch(&fx).await;
}

#[tokio::test]
async fn going_back_resumes_the_old_session_and_sends_only_what_happened_since() {
    let fx = fx().await;
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", "fix the parser"), ("agent", "parser fixed")])}),
    );
    let first = fx
        .queue("to claude", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    fx.release(&first).await.unwrap();
    fx.finish(&first).await;
    fx.write(
        "claude-items.json",
        &json!({CLAUDE_THREAD: text_items(&[("user", "review the diff"), ("agent", "diff looks fine")])}),
    );
    let back = fx
        .queue("back to codex", Some((AgentFamily::Codex, "fake")))
        .await;
    fx.release(&back).await.unwrap();

    let thread = fx.thread().await;
    assert_eq!(thread.family, AgentFamily::Codex);
    assert_eq!(thread.native_session_id.as_deref(), Some("codex-main"));
    // The old Codex session was resumed, not started again.
    assert_eq!(fx.codex_requests("thread/start").len(), 0);
    assert!(!fx.codex_requests("thread/resume").is_empty());
    let starts = fx.codex_requests("turn/start");
    let input = turn_input(starts.last().unwrap());
    assert!(is_handoff_text(text_of(&input[0])));
    let briefing = text_of(&input[0]);
    assert!(briefing.contains("delta_since_target_last_seen"));
    assert!(briefing.contains("review the diff") && briefing.contains("diff looks fine"));
    // What Codex already had is not repeated.
    assert!(!briefing.contains("fix the parser"));
    assert_eq!(text_of(&input[1]), "back to codex");
    let history = fx
        .state
        .store
        .project_runtime_history(&fx.chat)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].family, AgentFamily::Claude);
    assert!(fx
        .state
        .store
        .pending_handoff(&fx.chat)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn going_back_to_a_long_session_carries_a_shortened_briefing_instead_of_refusing() {
    let fx = fx().await;
    // Codex's own session holds far more text than the handoff could sit beside;
    // Codex compacts that itself, so it must not stop the switch back.
    let long = "l".repeat(70_000);
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", &long), ("agent", &long)])}),
    );
    let first = fx
        .queue("to claude", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    fx.release(&first).await.unwrap();
    fx.finish(&first).await;
    let answer = format!("diff looks fine {}", "d".repeat(20_000));
    fx.write(
        "claude-items.json",
        &json!({CLAUDE_THREAD: text_items(&[("user", "review the diff"), ("agent", &answer)])}),
    );
    let back = fx
        .queue("back to codex", Some((AgentFamily::Codex, "fake")))
        .await;
    fx.release(&back).await.unwrap();

    assert_eq!(fx.thread().await.family, AgentFamily::Codex);
    let starts = fx.codex_requests("turn/start");
    let input = turn_input(starts.last().unwrap());
    let briefing = text_of(&input[0]);
    assert!(is_handoff_text(briefing));
    assert!(briefing.contains("delta_since_target_last_seen"));
    // The short request is carried whole, the long answer as a shortened excerpt.
    assert!(briefing.contains("review the diff"));
    assert!(briefing.contains("diff looks fine"));
    assert!(briefing.contains("shortened; read position"));
    assert!(briefing.len() < answer.len());
    assert_eq!(text_of(&input[1]), "back to codex");
}

#[tokio::test]
async fn a_session_that_cannot_be_resumed_is_replaced_with_a_full_handoff() {
    let fx = fx().await;
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", "fix the parser"), ("agent", "parser fixed")])}),
    );
    let first = fx
        .queue("to claude", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    fx.release(&first).await.unwrap();
    fx.finish(&first).await;
    // The earlier Codex session was never handed delivery state that is ambiguous,
    // but the provider no longer knows it: make its thread unreadable.
    fx.state
        .store
        .set_handoff_delivery(
            &fx.chat,
            "codex-main",
            Some(HandoffDelivery::Pending),
            "now",
        )
        .await
        .unwrap();
    std::fs::write(fx.dir.path().join("next-thread"), "fresh-codex").unwrap();
    let back = fx
        .queue("back to codex", Some((AgentFamily::Codex, "fake")))
        .await;
    fx.release(&back).await.unwrap();

    assert_eq!(fx.codex_requests("thread/start").len(), 1);
    let thread = fx.thread().await;
    assert_eq!(thread.native_session_id.as_deref(), Some("fresh-codex"));
    let starts = fx.codex_requests("turn/start");
    let briefing = text_of(&turn_input(starts.last().unwrap())[0]).to_owned();
    assert!(briefing.contains("full_thread_summary"));
    assert!(briefing.contains("fix the parser"));
}

// Contract: a send refused before it ran leaves the context owed, and the retry
// delivers it exactly once.
#[tokio::test]
async fn a_refused_send_is_retried_with_the_same_context_and_delivered_once() {
    let fx = fx().await;
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", "fix the parser"), ("agent", "parser fixed")])}),
    );
    std::fs::write(fx.dir.path().join("claude-busy"), "").unwrap();
    let message = fx
        .queue("to claude", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    assert!(fx.release(&message).await.is_err());
    // Nothing was delivered: the context is still owed and the delivery forgotten.
    assert!(fx
        .state
        .store
        .pending_handoff(&fx.chat)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        fx.state
            .store
            .handoff_delivery(&fx.chat, CLAUDE_THREAD)
            .await
            .unwrap(),
        None
    );
    std::fs::remove_file(fx.dir.path().join("claude-busy")).unwrap();
    fx.state
        .store
        .update_message_delivery(&message.id, "accepted_by_wonder", None, None)
        .await
        .unwrap();
    fx.release(&message).await.unwrap();
    let starts = fx.claude_requests("turn/start");
    assert_eq!(starts.len(), 2);
    let briefing = |request: &Value| text_of(&turn_input(request)[0]).to_owned();
    assert_eq!(briefing(&starts[0]), briefing(&starts[1]));
    // The thread did not get a second session for the retry.
    assert_eq!(fx.claude_requests("thread/start").len(), 1);
    assert!(fx
        .state
        .store
        .pending_handoff(&fx.chat)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        fx.state
            .store
            .handoff_delivery(&fx.chat, CLAUDE_THREAD)
            .await
            .unwrap(),
        Some(HandoffDelivery::Injected)
    );
}

// Contract: a delivery whose outcome is unknown is never repeated into the
// same session; the session is replaced and the context rebuilt.
#[tokio::test]
async fn an_ambiguous_delivery_replaces_the_session_instead_of_repeating_the_context() {
    let fx = fx().await;
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", "fix the parser"), ("agent", "parser fixed")])}),
    );
    // The bridge fails after taking the turn: the outcome is unknown.
    std::fs::write(fx.dir.path().join("claude-boom"), "").unwrap();
    let first = fx
        .queue("to claude", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    assert!(fx.release(&first).await.is_err());
    assert_eq!(
        fx.state
            .store
            .handoff_delivery(&fx.chat, CLAUDE_THREAD)
            .await
            .unwrap(),
        Some(HandoffDelivery::Pending)
    );
    assert!(fx
        .state
        .store
        .pending_handoff(&fx.chat)
        .await
        .unwrap()
        .is_some());
    // Recovery settles the unknown send; the owner sends again.
    fx.state
        .store
        .update_message_delivery(&first.id, "failed", None, None)
        .await
        .unwrap();
    std::fs::remove_file(fx.dir.path().join("claude-boom")).unwrap();
    std::fs::write(
        fx.dir.path().join("next-claude"),
        "claude-99999999-8888-4777-8666-555555555555",
    )
    .unwrap();
    let again = fx.queue("again", None).await;
    fx.release(&again).await.unwrap();
    // A second Claude session took the turn, with the context from the start.
    assert_eq!(fx.claude_requests("thread/start").len(), 2);
    let thread = fx.thread().await;
    assert_eq!(
        thread.native_session_id.as_deref(),
        Some("99999999-8888-4777-8666-555555555555")
    );
    let starts = fx.claude_requests("turn/start");
    let last = turn_input(starts.last().unwrap());
    assert!(is_handoff_text(text_of(&last[0])));
    assert!(text_of(&last[0]).contains("fix the parser"));
    assert_eq!(text_of(&last[1]), "again");
    assert!(fx
        .state
        .store
        .pending_handoff(&fx.chat)
        .await
        .unwrap()
        .is_none());
    // The half-delivered session stays readable as part of the thread.
    assert_eq!(
        fx.state
            .store
            .project_runtime_history(&fx.chat)
            .await
            .unwrap()
            .len(),
        2
    );
}

// Contract: a plain model change inside one provider needs no new session, and
// the thread's own settings are changed by nothing but delivery.
#[tokio::test]
async fn a_same_provider_model_keeps_the_session_and_the_thread_is_unmoved_until_release() {
    let fx = fx().await;
    let message = fx
        .queue("hello", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    // Accepting a message with the other provider's model changes nothing yet.
    assert_eq!(fx.thread().await.family, AgentFamily::Codex);
    fx.finish(&message).await;
    let same = fx.queue("again", Some((AgentFamily::Codex, "fake"))).await;
    fx.release(&same).await.unwrap();
    assert_eq!(fx.thread().await.family, AgentFamily::Codex);
    assert!(fx
        .state
        .store
        .project_runtime_history(&fx.chat)
        .await
        .unwrap()
        .is_empty());
    assert!(fx
        .state
        .store
        .pending_handoff(&fx.chat)
        .await
        .unwrap()
        .is_none());
}

// Contract: the history tool and the settings follower both read the active
// session; an old provider's turns never flip settings back.
#[tokio::test]
async fn reading_and_desktop_settings_follow_the_active_session_after_a_switch() {
    let fx = fx().await;
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", "fix the parser"), ("agent", "parser fixed")])}),
    );
    let first = fx
        .queue("to claude", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    fx.release(&first).await.unwrap();
    fx.finish(&first).await;
    fx.write(
        "claude-items.json",
        &json!({CLAUDE_THREAD: text_items(&[("user", "to claude"), ("agent", "hello from claude")])}),
    );
    // Positions run through the Codex session, then the Claude one.
    let segments = crate::thread_tools::segments_for_test(&fx.state, &fx.chat).await;
    assert_eq!(
        segments,
        vec![
            (AgentFamily::Codex, "codex-main".to_owned(), false),
            (AgentFamily::Claude, CLAUDE_THREAD.to_owned(), true)
        ]
    );
    assert_eq!(count_positions(&fx.state, &fx.chat).await, Some(4));
    // A desktop turn on the old Codex session does not change the Claude thread.
    let stored = fx.thread().await;
    let desktop = crate::native_settings::NativeSettings {
        turn: "codex-desktop-turn".into(),
        model: "fake".into(),
        effort: None,
        service_tier: None,
    };
    let changed = crate::native_settings::apply(&fx.state, &stored, &desktop)
        .await
        .unwrap();
    assert!(!changed);
    let after = fx.thread().await;
    assert_eq!(after.model.as_deref(), Some("claude:sonnet"));
    assert_eq!(after.family, AgentFamily::Claude);
    // And reading settings goes to the active (Claude) binding only.
    let binding = fx
        .state
        .store
        .runtime_binding(&fx.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding.family, AgentFamily::Claude);
}

#[tokio::test]
async fn a_message_too_large_for_the_handoff_is_refused_without_changing_it() {
    let fx = fx().await;
    fx.write(
        "codex-items.json",
        &json!({"codex-main": text_items(&[("user", "fix the parser"), ("agent", "parser fixed")])}),
    );
    let huge = "p".repeat(60_000);
    let message = fx
        .queue(&huge, Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    // 60 KB is more than a Claude handoff leaves room for beside the history.
    let result = fx.release(&message).await;
    if let Err(error) = result {
        assert!(error.contains("not enough room"), "{error}");
        assert!(fx.claude_requests("turn/start").is_empty());
    } else {
        // It fits: the text must have reached Claude whole.
        let starts = fx.claude_requests("turn/start");
        assert_eq!(text_of(&turn_input(&starts[0])[1]), huge);
    }
}

#[tokio::test]
async fn native_clear_acknowledges_only_its_owned_claude_runtime_and_fences_late_output() {
    let fx = fx().await;
    let first = fx
        .queue("/clear", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    let (_, turn, _) = fx.release(&first).await.unwrap();
    fx.state
        .store
        .update_message_delivery(&first.id, "completed", Some(CLAUDE_THREAD), Some(&turn))
        .await
        .unwrap();
    let queued = fx.queue("After clear", None).await;
    let next = "11111111-2222-4333-8444-555555555555";
    let rpc = crate::claude::for_thread(&fx.state, CLAUDE_THREAD)
        .await
        .unwrap();
    let params = json!({"threadId":CLAUDE_THREAD,"oldSessionId":CLAUDE_SESSION,"sessionId":next,"retainedTurnIds":[]});
    let codex = crate::projects::codex_rpc(&fx.state).await.unwrap();
    crate::publish_app_server_notification(&fx.state, json!({"id":"foreign-reset","method":"thread/session/reset","params":params,"_wonderRuntimeId":codex.health().id()})).await;
    assert_eq!(
        fx.thread().await.native_session_id.as_deref(),
        Some(CLAUDE_SESSION)
    );
    crate::publish_app_server_notification(&fx.state, json!({"id":"owned-reset","method":"thread/session/reset","params":params,"_wonderRuntimeId":rpc.health().id()})).await;
    let after = fx
        .state
        .store
        .committed_conversation_snapshot(&fx.state.host_epoch, &fx.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.history_revision, 1);
    assert_eq!(fx.thread().await.native_session_id.as_deref(), Some(next));
    assert_eq!(
        after
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec![queued.id.as_str()]
    );
    assert_eq!(after.codex_thread_id.as_deref(), Some(CLAUDE_THREAD));
    let sequence = after.last_sequence;
    crate::publish_app_server_notification(&fx.state, json!({"method":"item/agentMessage/delta","params":{"threadId":CLAUDE_THREAD,"turnId":turn,"itemId":"late","delta":"old reply must stay hidden"},"_wonderRuntimeId":rpc.health().id()})).await;
    assert_eq!(
        fx.state
            .store
            .committed_sequence(&fx.state.host_epoch)
            .await
            .unwrap(),
        sequence,
        "hidden old output is never broadcast live"
    );
    crate::publish_app_server_notification(&fx.state, json!({"id":"lost-ack-retry","method":"thread/session/reset","params":params,"_wonderRuntimeId":rpc.health().id()})).await;
    assert_eq!(
        fx.state
            .store
            .committed_sequence(&fx.state.host_epoch)
            .await
            .unwrap(),
        sequence
    );
}

// Contract: an OpenCode thread receives Cupola's thread tools when its ACP
// session is created and with every turn; the bridge turns them into the
// session's own MCP server (services/acp-runtime/test/tools.test.mjs).
/// What the fake OpenCode bridge received for one new OpenCode thread.
async fn opencode_requests(fx: &mut Fx) -> Vec<Value> {
    // The fake Claude bridge, speaking for OpenCode under its own log.
    let source = std::fs::read_to_string(fx.dir.path().join("claude-bridge.py")).unwrap();
    let bridge = fx.dir.path().join("opencode-bridge.py");
    std::fs::write(
        &bridge,
        source
            .replace("/claude-requests", "/opencode-requests")
            .replace("'family': 'claude'", "'family': 'opencode'")
            .replace("/next-claude", "/next-opencode")
            .replace(
                CLAUDE_THREAD,
                "opencode-aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
            )
            .replace("len('claude-')", "len('opencode-')"),
    )
    .unwrap();
    let config = wonder_app_server::BridgeLaunchConfig {
        node_bin: "/usr/bin/python3".into(),
        entrypoint: bridge,
        state_dir: fx.dir.path().join("opencode-state"),
        npm_cli: None,
        claude_config_dir: None,
        wonder_version: "test".into(),
        family: "opencode",
    };
    let client = Arc::new(Mutex::new(AppServerClient::unavailable(
        "test".into(),
        crate::ingestion::notification_sink(fx.state.store.clone()),
    )));
    client
        .lock()
        .await
        .restart_bridge(config.clone())
        .await
        .unwrap();
    fx.state.opencode = Some(Arc::new(crate::claude::Runtime { client, config }));
    let message = fx
        .queue(
            "continue in OpenCode",
            Some((AgentFamily::OpenCode, "opencode:opencode/fake")),
        )
        .await;
    fx.offer_models().await;
    fx.state
        .runtime_catalog
        .write()
        .await
        .models
        .push(model("opencode:opencode/fake", AgentFamily::OpenCode));
    assert!(fx
        .state
        .store
        .claim_message_for_dispatch(&message.id)
        .await
        .unwrap());
    let message = fx
        .state
        .store
        .message_by_id(&message.id)
        .await
        .unwrap()
        .unwrap();
    let mut submitting = false;
    crate::projects::dispatch_inner(&fx.state, &message, &mut submitting)
        .await
        .unwrap();
    std::fs::read_to_string(fx.dir.path().join("opencode-requests"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn a_new_opencode_session_gets_the_thread_tools_at_start_and_each_turn() {
    let mut fx = fx().await;
    let requests = opencode_requests(&mut fx).await;
    let names = |method: &str| -> Vec<String> {
        let request = requests.iter().find(|r| r["method"] == method).unwrap();
        request["params"]["dynamicTools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let expected: Vec<String> = crate::thread_tools::specs()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names("thread/start"), expected);
    assert_eq!(names("turn/start"), expected);
    // ACP has no system prompt: the new session's first prompt opens with
    // Cupola's preamble, naming the tools, as a briefing Cupola never shows.
    let turn = requests
        .iter()
        .find(|r| r["method"] == "turn/start")
        .unwrap();
    assert_eq!(
        turn["params"]["input"][0],
        json!({"type": "text", "text": crate::preamble::first_prompt_block(true)})
    );
    assert!(is_handoff_text(text_of(&turn["params"]["input"][0])));
    assert!(turn["params"].get("wonderPreamble").is_none());
}

// Contract: with the owner's thread tools off for OpenCode, its session
// starts and each turn carries an empty tool list (the ACP bridge then
// offers no MCP server), its preamble names no tools, and an instruction is
// never sent to OpenCode, whose ACP has no way to take one.
#[tokio::test]
async fn opencode_gets_no_thread_tools_while_off_and_never_an_instruction() {
    let mut fx = fx().await;
    fx.state
        .store
        .set_agent_thread_tools(AgentFamily::OpenCode, false, "now")
        .await
        .unwrap();
    // Stored directly: the settings route refuses it for OpenCode.
    fx.state
        .store
        .set_agent_instruction(AgentFamily::OpenCode, true, "Be brief.", "now")
        .await
        .unwrap();
    let requests = opencode_requests(&mut fx).await;
    for method in ["thread/start", "turn/start"] {
        let request = requests.iter().find(|r| r["method"] == method).unwrap();
        assert_eq!(request["params"]["dynamicTools"], json!([]), "{method}");
        assert!(!request.to_string().contains("Be brief."), "{method}");
    }
    let turn = requests
        .iter()
        .find(|r| r["method"] == "turn/start")
        .unwrap();
    assert_eq!(
        turn["params"]["input"][0]["text"],
        crate::preamble::first_prompt_block(false)
    );
}

async fn listed(fx: &Fx) -> Vec<Value> {
    let response = crate::projects::threads(
        axum::extract::State(fx.state.clone()),
        axum::Extension(crate::OwnerAuthority),
        axum::extract::Path("project".into()),
        axum::extract::Query(serde_json::from_value(json!({"limit": 30})).unwrap()),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let page: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 4_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    page["threads"].as_array().unwrap().clone()
}

/// A Codex thread id the phone can name in a reference.
const LEFT: &str = "0d1e2f30-4a5b-4c6d-8e7f-901a2b3c4d5e";

/// A thread titled "Main" on Codex session `LEFT` that moved to Claude, which
/// lists the session under its first prompt: the briefing. Returns the Claude
/// session. `codex` is the rest of Codex's thread list.
async fn codex_thread_moved_to_claude(fx: &mut Fx, codex: Value) -> String {
    let folder = fx.thread().await.cwd;
    let chat = uuid::Uuid::new_v4().to_string();
    let project = fx.state.store.project("project").await.unwrap().unwrap();
    fx.state
        .store
        .create_project_conversation(wonder_store::ProjectConversationInsert {
            conversation_id: &chat,
            project_id: "project",
            family: AgentFamily::Codex,
            provider_store: &fx.state.projects.codex_store,
            native_session_id: Some(LEFT),
            cwd: &folder,
            roots_revision: project.roots_revision,
            title: "Main",
            model: Some("fake"),
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
    fx.state
        .store
        .bind_project_runtime(
            &chat,
            AgentFamily::Codex,
            &fx.state.projects.codex_store,
            LEFT,
            None,
            "now",
        )
        .await
        .unwrap();
    fx.chat = chat;
    let mut threads = vec![
        json!({"id": LEFT, "name": null, "preview": "fix the parser", "cwd": folder, "updatedAt": 100, "status": {"type": "idle"}}),
    ];
    threads.extend(codex.as_array().cloned().unwrap_or_default());
    fx.write("codex-threads.json", &Value::Array(threads));
    // Codex can read `LEFT` back when the thread returns to it.
    let idle = fx.dir.path().join("idle-fixture.json");
    let mut known: Value = serde_json::from_str(&std::fs::read_to_string(&idle).unwrap()).unwrap();
    known[LEFT] = json!({"id": LEFT, "status": {"type": "idle"}, "cwd": folder});
    fx.write("idle-fixture.json", &known);
    fx.write(
        "codex-items.json",
        &json!({LEFT: text_items(&[("user", "fix the parser"), ("agent", "parser fixed")])}),
    );
    let first = fx
        .queue("to claude", Some((AgentFamily::Claude, "claude:sonnet")))
        .await;
    fx.release(&first).await.unwrap();
    fx.finish(&first).await;
    let session = fx.thread().await.native_session_id.unwrap();
    // Claude names a session by its first prompt (whitespace folded, 200
    // characters): here, the briefing.
    let starts = fx.claude_requests("turn/start");
    let briefing = text_of(&turn_input(starts.last().unwrap())[0]).to_owned();
    assert!(is_handoff_text(&briefing));
    let claude_title: String = briefing
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect();
    fx.write(
        "claude-reply-project-sessions-list.json",
        &json!({"data": [{"sessionId": session, "title": claude_title, "cwd": folder, "updatedAt": 200}], "nextCursor": null, "partial": false}),
    );
    session
}

/// A thread that moved from Codex to Claude is one row under its own title:
/// the Codex session it left is not a second thread, the briefing the Claude
/// session started with is not its name, and a rename shows in place and
/// stays when the thread moves back to its first Codex session.
#[tokio::test]
async fn a_switched_thread_is_one_row_under_its_own_title_and_renames_in_place() {
    let mut fx = fx().await;
    let folder = fx.thread().await.cwd;
    let reviewer = "You are reviewing the Wonder change on branch:agent/a4-handoff-titles at head 5e9e6134, written by another\nmodel.";
    let session = codex_thread_moved_to_claude(
        &mut fx,
        json!([{"id": "codex-review", "name": null, "preview": reviewer, "cwd": folder, "updatedAt": 90, "status": {"type": "idle"}}]),
    )
    .await;

    let rows = listed(&fx).await;
    let ours: Vec<_> = rows
        .iter()
        .filter(|r| r["conversationId"] == fx.chat.as_str())
        .collect();
    assert_eq!(ours.len(), 1, "{rows:#?}");
    assert_eq!(ours[0]["title"], "Main");
    assert!(
        !rows
            .iter()
            .any(|r| r["reference"] == format!("codex:{LEFT}")),
        "the session the thread left is not another row: {rows:#?}"
    );
    assert!(!rows.iter().any(|r| r["title"]
        .as_str()
        .unwrap_or_default()
        .contains("wonder-handoff")));
    let review = rows
        .iter()
        .find(|r| r["reference"] == "codex:codex-review")
        .unwrap();
    assert_eq!(review["title"], "Review a4-handoff-titles");

    // Renaming opens the left Codex session's row as this same thread.
    let before = fx
        .state
        .store
        .project_conversations("project")
        .await
        .unwrap()
        .len();
    let receipt = crate::projects::attach(
        axum::extract::State(fx.state.clone()),
        axum::Extension(crate::OwnerAuthority),
        axum::extract::Path("project".into()),
        axum::Json(serde_json::from_value(json!({"reference": format!("codex:{LEFT}")})).unwrap()),
    )
    .await;
    assert_eq!(receipt.status(), axum::http::StatusCode::OK);
    let receipt: Value = serde_json::from_slice(
        &axum::body::to_bytes(receipt.into_body(), 1_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["conversationId"], fx.chat.as_str());
    assert_eq!(
        fx.state
            .store
            .project_conversations("project")
            .await
            .unwrap()
            .len(),
        before,
        "no second thread"
    );
    let renamed = crate::projects::update_conversation(
        axum::extract::State(fx.state.clone()),
        axum::Extension(crate::OwnerAuthority),
        axum::extract::Path(fx.chat.clone()),
        axum::Json(serde_json::from_value(json!({"title": "Parser work"})).unwrap()),
    )
    .await;
    assert_eq!(renamed.status(), axum::http::StatusCode::OK);
    let rows = listed(&fx).await;
    let ours: Vec<_> = rows
        .iter()
        .filter(|r| r["conversationId"] == fx.chat.as_str())
        .collect();
    assert_eq!(ours.len(), 1, "{rows:#?}");
    assert_eq!(ours[0]["title"], "Parser work");
    assert_eq!(
        fx.thread().await.family,
        AgentFamily::Claude,
        "a rename moves nothing"
    );

    // Back on the first Codex session, whose first message ("fix the parser")
    // is not the thread's title, and the Claude session is now history.
    fx.write(
        "claude-items.json",
        &json!({CLAUDE_THREAD: text_items(&[("user", "to claude"), ("agent", "done")])}),
    );
    let back = fx
        .queue("back to codex", Some((AgentFamily::Codex, "fake")))
        .await;
    fx.release(&back).await.unwrap();
    fx.finish(&back).await;
    assert_eq!(fx.thread().await.native_session_id.as_deref(), Some(LEFT));
    let rows = listed(&fx).await;
    let ours: Vec<_> = rows
        .iter()
        .filter(|r| r["conversationId"] == fx.chat.as_str())
        .collect();
    assert_eq!(ours.len(), 1, "{rows:#?}");
    assert_eq!(ours[0]["reference"], format!("codex:{LEFT}"));
    assert_eq!(ours[0]["title"], "Parser work");
    assert!(
        !rows
            .iter()
            .any(|r| r["reference"] == format!("claude:{session}")),
        "the Claude session the thread left is not another row: {rows:#?}"
    );
}

/// Archiving on the desktop the Codex session a thread switched away from
/// archives only its history: the archived list shows no row for it, so it
/// cannot be restored or opened as the thread.
#[tokio::test]
async fn an_archived_session_a_thread_left_is_not_an_archived_row() {
    let mut fx = fx().await;
    let folder = fx.thread().await.cwd;
    codex_thread_moved_to_claude(&mut fx, json!([])).await;
    fx.write(
        "codex-archived.json",
        &json!([
            {"id": LEFT, "name": null, "preview": "fix the parser", "cwd": folder, "updatedAt": 100, "status": {"type": "notLoaded"}},
            {"id": "codex-old", "name": null, "preview": "an old idea", "cwd": folder, "updatedAt": 50, "status": {"type": "notLoaded"}},
        ]),
    );
    let response = crate::projects::archived_threads(
        axum::extract::State(fx.state.clone()),
        axum::Extension(crate::OwnerAuthority),
        axum::extract::Path("project".into()),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let page: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 4_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    let rows = page["threads"].as_array().unwrap();
    assert!(
        rows.iter().any(|r| r["reference"] == "codex:codex-old"),
        "other archived threads still list: {rows:#?}"
    );
    assert!(
        !rows
            .iter()
            .any(|r| r["reference"] == format!("codex:{LEFT}")
                || r["conversationId"] == fx.chat.as_str()),
        "the left session is not an archived row: {rows:#?}"
    );
}

async fn json_of(response: axum::response::Response) -> Value {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4_000_000)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    body
}

async fn archived(fx: &Fx) -> Vec<Value> {
    let page = json_of(
        crate::projects::archived_threads(
            axum::extract::State(fx.state.clone()),
            axum::Extension(crate::OwnerAuthority),
            axum::extract::Path("project".into()),
        )
        .await,
    )
    .await;
    // A provider that could not be read leaves its rows on their stored
    // titles; say so here instead of as a wrong title further on.
    let unread: Vec<_> = page["partial"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|failure| failure["family"] == "codex")
        .collect();
    assert!(unread.is_empty(), "Codex archive unread: {unread:?}");
    page["threads"].as_array().unwrap().clone()
}

/// A scripted run's whole brief, and the start of it a provider may list.
fn brief(task: &str) -> String {
    format!("GOAL: Implement plan unit A4 from plans/ux-sweep-2026-10-21.md exactly as its row describes.\n\nSCOPE: branch agent/{task}; start with: crates/wonderd/src/projects.rs")
}

/// One title rule on every surface (listed, pinned, archived, an attach
/// receipt, the header and search's stored title). Titles stored before the
/// rule stay exactly as they were, renames included. A title Cupola took is
/// written once: no later preview replaces it, not one cut short inside the
/// task's name (Claude lists 200 characters) nor a session's Cupola
/// briefing. A thread taken before it had a name gets one once. A rename
/// always wins.
#[tokio::test]
async fn every_surface_shows_one_title_and_keeps_renames_and_tasks() {
    let fx = fx().await;
    let thread = fx.thread().await;
    let surfaces = ["listed", "pinned", "archived", "attached"];
    let cut = |task: &str| brief(task).split("agent/").next().unwrap().to_owned();
    let review = |_: &str| {
        "You are reviewing the Wonder change at head 5e9e6134, written by another model, and"
            .to_owned()
    };
    let handoff = |_: &str| {
        "<wonder-handoff conversation=\"c1\">\nuser: fix the parser\n</wonder-handoff>\nNow add tests".to_owned()
    };
    let full = |task: &str| brief(task);
    // Cut short inside the task's name: names a different, partial task.
    let partial = |task: &str| format!("{}agent/{}", cut(task), &task[..4]);
    let typed = "GOAL: keep agent/a1-typed as I wrote it";
    type Preview<'a> = &'a dyn Fn(&str) -> String;
    // (shape, stored title, taken from a first message?, provider preview)
    let shapes: [(&str, &str, bool, Preview); 9] = [
        // Stored before the rule: as it was, whatever the provider lists.
        ("legacy-goal", "GOAL", false, &full),
        (
            "legacy-handoff",
            "<wonder-handoff conversation=\"c1\">",
            false,
            &full,
        ),
        ("legacy-rename", "My chosen name", false, &full),
        // Taken by Cupola from the whole first message.
        ("task", "{task}", true, &cut),
        ("cut-task", "{task}", true, &partial),
        ("review", "Review {task}", true, &review),
        ("briefed", "Fix the parser", true, &handoff),
        ("renamed", "GOAL", true, &full),
        // Taken before the provider listed anything: named once, by its task.
        ("untitled", "Untitled thread", true, &full),
    ];
    let mut codex_listed = Vec::new();
    let mut codex_archived = Vec::new();
    let mut expected = Vec::new();
    let mut n = 0;
    for surface in surfaces {
        for (shape, stored, taken, preview) in &shapes {
            n += 1;
            let native = format!("00000000-0000-4000-8000-{n:012}");
            let task = format!("{surface}-{shape}");
            let stored = stored.replace("{task}", &task);
            fx.state
                .store
                .create_project_conversation(wonder_store::ProjectConversationInsert {
                    conversation_id: &task,
                    project_id: "project",
                    family: AgentFamily::Codex,
                    provider_store: &fx.state.projects.codex_store,
                    native_session_id: Some(&native),
                    cwd: &thread.cwd,
                    roots_revision: thread.roots_revision,
                    title: &stored,
                    model: Some("fake"),
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
            if *taken {
                fx.state
                    .store
                    .mark_project_conversation_title_taken(&task)
                    .await
                    .unwrap();
            }
            let row = json!({"id": native, "name": null, "preview": preview(&task), "cwd": thread.cwd, "updatedAt": 100, "status": {"type": "idle"}});
            if surface == "archived" {
                codex_archived.push(row);
                assert!(fx
                    .state
                    .store
                    .set_project_conversation_wonder_archived(&task, true, "now")
                    .await
                    .unwrap());
            } else {
                codex_listed.push(row);
            }
            if surface == "pinned" {
                fx.state
                    .store
                    .update_project_conversation(
                        &task,
                        wonder_store::ProjectConversationPatch {
                            pinned: Some(true),
                            ..Default::default()
                        },
                        "now",
                    )
                    .await
                    .unwrap();
            }
            let title = if *shape == "renamed" {
                json_of(
                    crate::projects::update_conversation(
                        axum::extract::State(fx.state.clone()),
                        axum::Extension(crate::OwnerAuthority),
                        axum::extract::Path(task.clone()),
                        axum::Json(serde_json::from_value(json!({"title": typed})).unwrap()),
                    )
                    .await,
                )
                .await;
                typed.to_owned()
            } else if *shape == "untitled" {
                task.clone()
            } else {
                stored.clone()
            };
            expected.push((surface, task, native, title, *taken));
        }
    }
    fx.write("codex-threads.json", &Value::Array(codex_listed));
    fx.write("codex-archived.json", &Value::Array(codex_archived));
    // Listing and attaching both check a thread with Codex. The fake runtime
    // answers a thread it does not know with a stream of 600 message deltas,
    // which the host handles one by one: with every listed thread unknown, the
    // archived listing waited behind that stream until its 30 s request limit
    // (only under load) and then showed no provider names.
    let idle = fx.dir.path().join("idle-fixture.json");
    let mut known: Value = serde_json::from_str(&std::fs::read_to_string(&idle).unwrap()).unwrap();
    for (_, _, native, ..) in &expected {
        known[native] = json!({"id": native, "status": {"type": "idle"}, "cwd": thread.cwd});
    }
    fx.write("idle-fixture.json", &known);

    // Twice: a title the first read settled stays on the next.
    for round in 0..2 {
        let rows = listed(&fx).await;
        let archived = archived(&fx).await;
        let mut failures = Vec::new();
        for (surface, task, native, title, taken) in &expected {
            let find = |rows: &[Value]| {
                rows.iter()
                    .find(|r| r["conversationId"] == task.as_str())
                    .map(|r| r["title"].clone())
            };
            let shown = match *surface {
                "listed" | "pinned" => find(&rows),
                "archived" => find(&archived),
                _ => {
                    let receipt = json_of(
                        crate::projects::attach(
                            axum::extract::State(fx.state.clone()),
                            axum::Extension(crate::OwnerAuthority),
                            axum::extract::Path("project".into()),
                            axum::Json(
                                serde_json::from_value(
                                    json!({"reference": format!("codex:{native}")}),
                                )
                                .unwrap(),
                            ),
                        )
                        .await,
                    )
                    .await;
                    assert_eq!(receipt["conversationId"], task.as_str());
                    Some(receipt["title"].clone())
                }
            };
            if shown.as_ref().and_then(Value::as_str) != Some(title.as_str()) {
                failures.push(format!(
                    "{task}: {surface} shows {shown:?}, wanted {title:?}"
                ));
            }
            let stored = fx
                .state
                .store
                .project_conversation(task)
                .await
                .unwrap()
                .unwrap();
            let header = crate::projects::conversation_detail(&fx.state, &stored)
                .await
                .unwrap();
            let chosen = !taken || task.ends_with("-renamed");
            if serde_json::to_value(header).unwrap()["title"] != title.as_str()
                || stored.title != *title
                || stored.title_chosen != chosen
            {
                failures.push(format!(
                    "{task}: header/stored {:?} (chosen {})",
                    stored.title, stored.title_chosen
                ));
            }
        }
        assert!(failures.is_empty(), "round {round}: {failures:#?}");
    }
}

/// A new thread takes its title once from its whole first message: a
/// scripted run is named by the task its brief names, stored as taken so
/// the list rule (never a rename) applies to it later.
#[tokio::test]
async fn a_new_thread_from_a_brief_is_named_by_its_task() {
    let fx = fx().await;
    fx.offer_models().await;
    let created = json_of(
        crate::projects::create_thread(
            axum::extract::State(fx.state.clone()),
            axum::Extension(crate::OwnerAuthority),
            None,
            axum::extract::Path("project".into()),
            axum::Json(
                serde_json::from_value(json!({"deviceId": "owner", "clientMessageId": uuid::Uuid::new_v4().to_string(),
                    "family": "codex", "model": "fake", "body": brief("a4-handoff-titles"), "prepareOnly": true}))
                .unwrap(),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(created["conversation"]["title"], "a4-handoff-titles");
    let stored = fx
        .state
        .store
        .project_conversation(created["conversation"]["conversationId"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (stored.title.as_str(), stored.title_chosen),
        ("a4-handoff-titles", false)
    );
}

/// A Codex session a thread switched away from, archived on the desktop and
/// opening with another task's brief, never renames the thread, even while
/// the thread itself is archived in Cupola.
#[tokio::test]
async fn an_archived_session_a_thread_left_never_renames_it() {
    let mut fx = fx().await;
    let folder = fx.thread().await.cwd;
    codex_thread_moved_to_claude(&mut fx, json!([])).await;
    fx.state
        .store
        .mark_project_conversation_title_taken(&fx.chat)
        .await
        .unwrap();
    assert!(fx
        .state
        .store
        .set_project_conversation_wonder_archived(&fx.chat, true, "now")
        .await
        .unwrap());
    fx.write(
        "codex-archived.json",
        &json!([{"id": LEFT, "name": null, "preview": brief("someone-else"), "cwd": folder, "updatedAt": 100, "status": {"type": "notLoaded"}}]),
    );
    let rows = archived(&fx).await;
    let ours: Vec<_> = rows
        .iter()
        .filter(|r| r["conversationId"] == fx.chat.as_str())
        .collect();
    assert_eq!(ours.len(), 1, "{rows:#?}");
    assert_eq!(ours[0]["title"], "Main");
    assert!(!rows
        .iter()
        .any(|r| r["reference"] == format!("codex:{LEFT}")));
    assert_eq!(fx.thread().await.title, "Main");
}
