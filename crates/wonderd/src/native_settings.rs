//! Keeps a thread's model, effort and speed in step with the provider's own
//! record. A turn made in the Claude or Codex app on the Mac leaves its
//! settings in the provider's session file; the next time the phone reads the
//! thread, those become the thread's settings, so the composer shows what was
//! last used. A change made on the phone afterwards stands until the desktop
//! runs another turn. A value the provider does not report is never guessed.

use crate::{agent_defaults, AgentFamily, AppState, RuntimeCatalog};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};
use wonder_store::{ProjectConversationPatch, StoredProjectConversation};

/// The newest turn made outside Wonder, with what it ran with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeSettings {
    /// Identifies the turn, so each one is taken once.
    pub turn: String,
    /// The provider's own model id, not yet mapped to the catalog.
    pub model: String,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
}

/// Reading the session file on every poll would cost more than it tells.
const CHECK_INTERVAL: Duration = Duration::from_secs(3);
const ROLLOUT_TAIL_BYTES: u64 = 4 * 1024 * 1024;

static CHECKED: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn due(conversation: &str) -> bool {
    let mut checked = CHECKED.lock().unwrap_or_else(|e| e.into_inner());
    if checked.len() > 512 {
        checked.retain(|_, at| at.elapsed() < CHECK_INTERVAL);
    }
    match checked.get(conversation) {
        Some(at) if at.elapsed() < CHECK_INTERVAL => false,
        _ => {
            checked.insert(conversation.to_owned(), Instant::now());
            true
        }
    }
}

fn bare(id: &str) -> &str {
    id.split('[').next().unwrap_or(id)
}

/// The catalog model a provider's own id stands for. Codex ids are the
/// catalog ids. Claude lists the ids its transcripts use beside each alias.
/// Context-window suffixes such as `[1m]` do not change the model.
pub(crate) fn catalog_model<'a>(
    catalog: &'a RuntimeCatalog,
    family: AgentFamily,
    native: &str,
) -> Option<&'a str> {
    let native = bare(native);
    let mut candidates = catalog
        .models
        .iter()
        .filter(|m| m.agent_family == family && !m.hidden);
    candidates
        .find(|m| bare(&m.id) == native || m.native_ids.iter().any(|id| bare(id) == native))
        .map(|m| m.id.as_str())
}

enum Outcome {
    /// This turn was already taken, or Wonder cannot tell what it used.
    Nothing,
    /// The provider names a model the catalog does not offer.
    Unmapped,
    Take(agent_defaults::Resolved),
}

fn plan(
    catalog: &RuntimeCatalog,
    stored: &StoredProjectConversation,
    native: &NativeSettings,
) -> Outcome {
    if stored.native_settings_turn.as_deref() == Some(native.turn.as_str()) {
        return Outcome::Nothing;
    }
    let Some(model) = catalog_model(catalog, stored.family, &native.model) else {
        return Outcome::Unmapped;
    };
    let Some(option) = catalog
        .models
        .iter()
        .find(|m| m.id == model && m.agent_family == stored.family)
    else {
        return Outcome::Unmapped;
    };
    // Only what the provider reported replaces the stored choice, and only
    // while the model offers it; the rest resolves as any thread does.
    let reported_effort = native
        .effort
        .as_deref()
        .filter(|id| option.reasoning_efforts.iter().any(|e| e.id == *id));
    let reported_tier = native
        .service_tier
        .as_deref()
        .filter(|tier| agent_defaults::offers_tier(option, tier));
    Outcome::Take(agent_defaults::resolve(
        catalog,
        stored.family,
        Some(model),
        reported_effort.or(stored.effort.as_deref()),
        reported_tier.or(stored.service_tier.as_deref()),
    ))
}

/// Takes `native` into the thread when it is a new turn. Returns whether the
/// thread's settings changed.
pub(crate) async fn apply(
    state: &AppState,
    stored: &StoredProjectConversation,
    native: &NativeSettings,
) -> Result<bool, String> {
    let outcome = {
        let catalog = state.runtime_catalog.read().await;
        plan(&catalog, stored, native)
    };
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let id = stored.conversation_id.as_str();
    let resolved = match outcome {
        Outcome::Nothing => return Ok(false),
        Outcome::Unmapped => {
            let _ = state.logger.record(
                "warn",
                "native_settings_unmapped",
                json!({"conversationId": id, "family": stored.family, "model": native.model}),
            );
            return Ok(false);
        }
        Outcome::Take(resolved) => resolved,
    };
    let changed = resolved.model != stored.model
        || resolved.effort != stored.effort
        || resolved.service_tier != stored.service_tier;
    let patch = ProjectConversationPatch {
        model: resolved.model.as_deref().filter(|_| changed),
        effort: changed.then_some(resolved.effort.as_deref()),
        service_tier: changed.then_some(resolved.service_tier.as_deref()),
        native_settings_turn: Some(&native.turn),
        ..Default::default()
    };
    // A phone change racing this read keeps its values and is not overwritten.
    match state
        .store
        .update_project_conversation_if_settings_match(
            id,
            patch,
            stored.model.as_deref(),
            stored.effort.as_deref(),
            stored.service_tier.as_deref(),
            &now,
        )
        .await
    {
        Ok(_) => Ok(changed),
        Err(sqlx::Error::Protocol(message)) if message == "project_settings_changed" => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

/// Reads the provider's record for this thread and takes a newer desktop turn's
/// settings. Failures leave the stored settings as they are.
pub(crate) async fn sync(state: &AppState, conversation: &str) {
    if !due(conversation) {
        return;
    }
    let Ok(Some(stored)) = state.store.project_conversation(conversation).await else {
        return;
    };
    let Some(native) = read(state, &stored).await else {
        return;
    };
    if let Err(error) = apply(state, &stored, &native).await {
        let _ = state.logger.record(
            "warn",
            "native_settings_failed",
            json!({"conversationId": conversation, "error": error}),
        );
    }
}

async fn read(state: &AppState, stored: &StoredProjectConversation) -> Option<NativeSettings> {
    let native_session = stored.native_session_id.as_deref()?;
    let binding = state
        .store
        .runtime_binding(&stored.conversation_id)
        .await
        .ok()??;
    let rpc =
        crate::providers::live_rpc_for_store(state, stored.family, &stored.provider_store).await?;
    match stored.family {
        // OpenCode's session settings live in OpenCode; Wonder's are current.
        AgentFamily::OpenCode | AgentFamily::Cursor => None,
        AgentFamily::Claude => {
            let reply = rpc
                .request(
                    "thread/nativeSettings",
                    json!({"threadId": binding.thread_id}),
                )
                .await
                .ok()?
                .result?;
            claude_settings(reply.get("settings")?)
        }
        AgentFamily::Codex => {
            let path = match crate::desktop_activity::codex_rollout(native_session) {
                Some(path) => path,
                None => {
                    let read = rpc
                        .request("thread/read", json!({"threadId": native_session}))
                        .await
                        .ok()?
                        .result?;
                    let path = read
                        .pointer("/thread/path")
                        .or_else(|| read.get("path"))?
                        .as_str()?
                        .to_owned();
                    crate::desktop_activity::remember_codex_rollout(native_session, &path);
                    path.into()
                }
            };
            let own: HashSet<String> = state
                .store
                .wonder_turn_ids(&stored.conversation_id)
                .await
                .ok()?
                .into_iter()
                .collect();
            tokio::task::spawn_blocking(move || {
                codex_rollout_settings(&read_tail(&path, ROLLOUT_TAIL_BYTES)?, &own)
            })
            .await
            .ok()?
        }
    }
}

/// The bridge's reply for a Claude session. A turn Wonder's own session wrote
/// ran with the thread's settings already.
fn claude_settings(value: &Value) -> Option<NativeSettings> {
    if value.get("native").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(NativeSettings {
        turn: text("turnId")?,
        model: text("model")?,
        effort: text("effort"),
        service_tier: text("speed"),
    })
}

fn read_tail(path: &std::path::Path, limit: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let name = path.file_name()?.to_str()?;
    if !path.is_absolute() || !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(limit);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // A tail read begins mid-line; drop the partial first line.
    Some(if start > 0 {
        text.split_once('\n')?.1.to_owned()
    } else {
        text
    })
}

/// The newest turn context in a Codex session log, unless Wonder started that
/// turn. The speed comes from the newest settings event, which is how Codex
/// records it.
pub(crate) fn codex_rollout_settings(
    log: &str,
    wonder_turns: &HashSet<String>,
) -> Option<NativeSettings> {
    let mut context = None;
    let mut tier = None;
    for line in log.lines().rev() {
        if context.is_some() && tier.is_some() {
            break;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(payload) = row.get("payload") else {
            continue;
        };
        match row.get("type").and_then(Value::as_str) {
            Some("turn_context") if context.is_none() => context = Some(payload.clone()),
            Some("event_msg")
                if tier.is_none()
                    && payload.get("type").and_then(Value::as_str)
                        == Some("thread_settings_applied") =>
            {
                tier = payload
                    .pointer("/thread_settings/service_tier")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
    }
    let context = context?;
    let turn = context.get("turn_id")?.as_str()?;
    if wonder_turns.contains(turn) {
        return None;
    }
    let effort = context
        .get("effort")
        .or_else(|| context.pointer("/collaboration_mode/settings/reasoning_effort"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(NativeSettings {
        turn: turn.to_owned(),
        model: context.get("model")?.as_str()?.to_owned(),
        effort,
        service_tier: tier,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent_defaults::tests::model;

    fn catalog() -> RuntimeCatalog {
        let mut sonnet = model(
            "claude:sonnet",
            AgentFamily::Claude,
            &["low", "medium", "high"],
            None,
        );
        sonnet.native_ids = vec!["claude-sonnet-5-0".into()];
        let mut opus = model(
            "claude:opus",
            AgentFamily::Claude,
            &["medium", "xhigh"],
            None,
        );
        opus.native_ids = vec!["claude-opus-5-5".into()];
        let mut c = RuntimeCatalog {
            models: vec![
                sonnet,
                opus,
                model(
                    "gpt-a",
                    AgentFamily::Codex,
                    &["low", "medium", "xhigh"],
                    None,
                ),
            ],
            ..Default::default()
        };
        c.resolve_defaults();
        c
    }

    fn stored(family: AgentFamily, model: &str, effort: &str) -> StoredProjectConversation {
        StoredProjectConversation {
            conversation_id: "c".into(),
            project_id: "p".into(),
            family,
            provider_store: "s".into(),
            native_session_id: Some("n".into()),
            cwd: "/w".into(),
            roots_revision: 1,
            title: "t".into(),
            title_chosen: false,
            model: Some(model.into()),
            effort: Some(effort.into()),
            service_tier: None,
            access_mode: "workspace".into(),
            claude_approval: "auto".into(),
            plan_mode: false,
            native_settings_turn: None,
            wonder_archived: false,
            is_pinned: false,
            has_unread: false,
            creation_request_id: None,
            created_at: String::new(),
            updated_at: String::new(),
            last_activity_at: String::new(),
        }
    }

    fn native(turn: &str, model: &str, effort: Option<&str>) -> NativeSettings {
        NativeSettings {
            turn: turn.into(),
            model: model.into(),
            effort: effort.map(str::to_owned),
            service_tier: None,
        }
    }

    fn taken(outcome: Outcome) -> agent_defaults::Resolved {
        match outcome {
            Outcome::Take(resolved) => resolved,
            _ => panic!("expected the settings to be taken"),
        }
    }

    #[test]
    fn a_newer_native_turn_sets_model_and_effort() {
        let c = catalog();
        let resolved = taken(plan(
            &c,
            &stored(AgentFamily::Claude, "claude:sonnet", "high"),
            &native("t1", "claude-opus-5-5", Some("xhigh")),
        ));
        assert_eq!(resolved.model.as_deref(), Some("claude:opus"));
        assert_eq!(resolved.effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn an_unreported_or_unoffered_effort_keeps_the_stored_one() {
        let c = catalog();
        // Stored "medium" is offered by opus and the provider reported nothing.
        let kept = taken(plan(
            &c,
            &stored(AgentFamily::Claude, "claude:sonnet", "medium"),
            &native("t1", "claude-opus-5-5", None),
        ));
        assert_eq!(kept.effort.as_deref(), Some("medium"));
        // Reported "max" is not offered, so the stored choice stands.
        let ignored = taken(plan(
            &c,
            &stored(AgentFamily::Claude, "claude:sonnet", "medium"),
            &native("t1", "claude-opus-5-5", Some("max")),
        ));
        assert_eq!(ignored.effort.as_deref(), Some("medium"));
    }

    #[test]
    fn a_turn_already_taken_changes_nothing() {
        let c = catalog();
        let mut thread = stored(AgentFamily::Claude, "claude:sonnet", "high");
        thread.native_settings_turn = Some("t1".into());
        assert!(matches!(
            plan(&c, &thread, &native("t1", "claude-opus-5-5", Some("xhigh"))),
            Outcome::Nothing
        ));
    }

    #[test]
    fn an_unmapped_native_model_is_left_alone() {
        let c = catalog();
        assert!(matches!(
            plan(
                &c,
                &stored(AgentFamily::Claude, "claude:sonnet", "high"),
                &native("t1", "claude-future-9", Some("high")),
            ),
            Outcome::Unmapped
        ));
        // A Codex id never maps into the Claude family.
        assert!(matches!(
            plan(
                &c,
                &stored(AgentFamily::Claude, "claude:sonnet", "high"),
                &native("t1", "gpt-a", None),
            ),
            Outcome::Unmapped
        ));
    }

    #[test]
    fn codex_ids_map_directly_and_context_suffixes_are_ignored() {
        let c = catalog();
        assert_eq!(
            catalog_model(&c, AgentFamily::Codex, "gpt-a"),
            Some("gpt-a")
        );
        assert_eq!(
            catalog_model(&c, AgentFamily::Claude, "claude-opus-5-5[1m]"),
            Some("claude:opus")
        );
    }

    #[test]
    fn rollout_settings_come_from_the_newest_desktop_turn() {
        let log = [
            r#"{"type":"turn_context","payload":{"turn_id":"t1","model":"gpt-a","effort":"low"}}"#,
            r#"{"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"model":"gpt-a","service_tier":"priority","reasoning_effort":"low"}}}"#,
            r#"{"type":"turn_context","payload":{"turn_id":"t2","model":"gpt-b","collaboration_mode":{"settings":{"reasoning_effort":"xhigh"}}}}"#,
            "not json",
        ]
        .join("\n");
        let settings = codex_rollout_settings(&log, &HashSet::new()).unwrap();
        assert_eq!(
            settings,
            NativeSettings {
                turn: "t2".into(),
                model: "gpt-b".into(),
                effort: Some("xhigh".into()),
                service_tier: Some("priority".into()),
            }
        );
        // The newest turn is one Wonder started: nothing to take.
        let own = HashSet::from(["t2".to_owned()]);
        assert_eq!(codex_rollout_settings(&log, &own), None);
    }

    #[test]
    fn claude_settings_ignore_turns_wonder_wrote() {
        let desktop = json!({"turnId":"a1","native":true,"model":"claude-opus-5-5","effort":"xhigh","speed":"standard"});
        assert_eq!(
            claude_settings(&desktop).unwrap().effort.as_deref(),
            Some("xhigh")
        );
        assert!(claude_settings(&json!({"turnId":"a2","native":false,"model":"m"})).is_none());
    }

    // Contract: a desktop turn replaces the stored settings once, a later
    // phone change stands until the desktop runs another turn, and a model the
    // catalog cannot map changes nothing.
    #[tokio::test]
    async fn desktop_turns_update_the_thread_and_a_later_phone_change_wins() {
        let (_dir, state, _message) = crate::projects::tests::handoff_fixture().await;
        let mut gpt_b = model("gpt-b", AgentFamily::Codex, &["low", "xhigh"], None);
        gpt_b.hidden = false;
        let mut c = RuntimeCatalog {
            models: vec![
                model("gpt-a", AgentFamily::Codex, &["low", "medium"], None),
                gpt_b,
            ],
            ..Default::default()
        };
        c.resolve_defaults();
        *state.runtime_catalog.write().await = c;
        let id = "project-chat";
        let set = |model: &'static str, effort: &'static str| {
            let store = state.store.clone();
            async move {
                store
                    .update_project_conversation(
                        id,
                        ProjectConversationPatch {
                            model: Some(model),
                            effort: Some(Some(effort)),
                            ..Default::default()
                        },
                        "now",
                    )
                    .await
                    .unwrap();
            }
        };
        let read = || async { state.store.project_conversation(id).await.unwrap().unwrap() };
        set("gpt-a", "low").await;

        let desktop = native("t1", "gpt-b", Some("xhigh"));
        assert!(apply(&state, &read().await, &desktop).await.unwrap());
        let after = read().await;
        assert_eq!(
            (after.model.as_deref(), after.effort.as_deref()),
            (Some("gpt-b"), Some("xhigh"))
        );
        assert_eq!(after.native_settings_turn.as_deref(), Some("t1"));

        // The owner picks another model on the phone; the same desktop turn
        // read again does not take it back.
        set("gpt-a", "medium").await;
        assert!(!apply(&state, &read().await, &desktop).await.unwrap());
        assert_eq!(read().await.model.as_deref(), Some("gpt-a"));

        // The desktop is used again.
        let later = native("t2", "gpt-b", Some("low"));
        assert!(apply(&state, &read().await, &later).await.unwrap());
        assert_eq!(read().await.effort.as_deref(), Some("low"));

        // An unmapped model leaves everything, including the marker, alone.
        let before = read().await;
        let unknown = native("t3", "gpt-unknown", Some("low"));
        assert!(!apply(&state, &before, &unknown).await.unwrap());
        assert_eq!(read().await, before);
    }
}
