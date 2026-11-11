//! Shared synthetic Project runtime fixtures for independent server contracts.
use crate::*;
use axum::body::to_bytes;
use serde_json::{json, Value};
use tower::ServiceExt;

pub(crate) async fn fixture() -> (tempfile::TempDir, AppState) {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    let file = dir.path().join("runtime.py");
    let source = std::fs::read_to_string(&file).unwrap()
            .replace("with open(root + '/requests', 'a') as log: log.write(method + '\\n')", "with open(root + '/requests', 'a') as log: log.write(method + '\\n')\n    with open(root + '/payloads', 'a') as log: log.write(json.dumps(r) + '\\n')");
    std::fs::write(file, source).unwrap();
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
        .runtime_catalog
        .write()
        .await
        .models
        .push(ModelOption {
            agent_family: AgentFamily::Codex,
            capabilities: ModelCapabilities::for_family(AgentFamily::Codex),
            id: "fake".into(),
            display_name: "Fake".into(),
            description: None,
            model_specialty: None,
            hidden: false,
            reasoning_efforts: vec![],
            default_reasoning_effort: None,
            service_tiers: vec![],
            default_service_tier: None,
            is_default: false,
            provider_default: None,
            native_ids: vec![],
        });
    (dir, state)
}
pub(crate) async fn call(
    state: &AppState,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-wonder-loopback-capability", &state.loopback_capability)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"error":String::from_utf8_lossy(&bytes)})),
    )
}

pub(crate) fn assert_http_contract(definition: &str, value: &Value) {
    let authority: Value = serde_json::from_str(include_str!(
        "../../../packages/protocol/schemas/wonder-http-v1.json"
    ))
    .unwrap();
    let schema = json!({
        "$ref": format!("#/$defs/{definition}"),
        "$defs": authority["$defs"].clone()
    });
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<_> = validator
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "{definition}: {errors:?}");
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn project_thread(
    store: &Store,
    id: &str,
    title: &str,
    cwd: &str,
    provider_store: &str,
    model: Option<&str>,
    effort: Option<&str>,
    now: &str,
) -> Result<wonder_store::StoredProjectConversation, sqlx::Error> {
    if let Some(existing) = store.project_conversation(id).await.unwrap() {
        return Ok(existing);
    }
    let cwd =
        crate::canonical_path::canonicalize(cwd).unwrap_or_else(|_| std::path::PathBuf::from(cwd));
    let cwd = cwd.to_str().unwrap();
    let project = format!("project-{id}");
    store
        .create_project(
            &project,
            &project,
            "fixture",
            title,
            &[wonder_store::ProjectRootInput {
                path: cwd.into(),
                canonical_path: cwd.into(),
            }],
            0,
            now,
        )
        .await
        .unwrap();
    store
        .create_project_conversation(wonder_store::ProjectConversationInsert {
            conversation_id: id,
            project_id: &project,
            family: if model.is_some_and(|m| m.starts_with("claude:")) {
                AgentFamily::Claude
            } else {
                AgentFamily::Codex
            },
            provider_store,
            native_session_id: None,
            cwd,
            roots_revision: 1,
            title,
            model,
            effort,
            service_tier: None,
            access_mode: "workspace",
            claude_approval: "ask",
            plan_mode: false,
            creation_request_id: None,
            now,
        })
        .await
        .unwrap();
    Ok(store.project_conversation(id).await.unwrap().unwrap())
}

pub(crate) async fn bind_project_thread(
    state: &AppState,
    id: &str,
    family: AgentFamily,
    thread: &str,
    session: Option<&str>,
    now: &str,
) -> Result<(), sqlx::Error> {
    if state.store.project_conversation(id).await?.is_none() {
        project_thread(
            &state.store,
            id,
            "Project",
            state.data_dir.to_str().unwrap(),
            crate::providers::provider_store(state, family),
            if family == AgentFamily::Claude {
                Some("claude:haiku")
            } else {
                None
            },
            None,
            now,
        )
        .await?;
    }
    state
        .store
        .set_project_native_session(id, thread, now)
        .await?;
    state
        .store
        .bind_project_runtime(
            id,
            family,
            crate::providers::provider_store(state, family),
            thread,
            session,
            now,
        )
        .await
}

/// A seeded generator for property tests over network input: the same seed
/// gives the same cases, so a failure reproduces from its seed.
pub(crate) struct Fuzz(u64);

impl Fuzz {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub(crate) fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    pub(crate) fn bytes(&mut self, max: usize) -> Vec<u8> {
        (0..self.below(max + 1))
            .map(|_| self.next() as u8)
            .collect()
    }

    /// Text biased toward the characters parsers and routers treat specially.
    pub(crate) fn text(&mut self, max: usize) -> String {
        const SPECIAL: &[&str] = &[
            "/",
            "?",
            "#",
            "%",
            "%2F",
            "..",
            ".",
            ":",
            "@",
            "[",
            "]",
            "\\",
            "\u{0}",
            " ",
            "\"",
            "{",
            "}",
            ",",
            "\u{e9}",
            "\u{1F600}",
            "\r\n",
            "A",
            "z",
            "0",
            "-",
            "_",
            "~",
        ];
        (0..self.below(max + 1))
            .map(|_| SPECIAL[self.below(SPECIAL.len())])
            .collect()
    }
}
