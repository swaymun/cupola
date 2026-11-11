//! Native, Project-scoped Codex skills. Catalogs are read on demand and never
//! persisted or reconstructed from files. Every delivery revalidates membership.
use crate::{projects, AppState, OwnerAuthority};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, time::Duration};
use wonder_store::{AgentFamily, CodexSkillSelection, StoredProject};

type Failure = (StatusCode, String);
fn invalid(message: &str) -> Failure {
    (StatusCode::UNPROCESSABLE_ENTITY, message.into())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ScopeQuery {
    folder_id: String,
    roots_revision: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Skill {
    name: String,
    path: String,
    description: String,
    enabled: bool,
    scope: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SkillError {
    path: String,
    message: String,
}
#[derive(Debug, Deserialize)]
struct Entry {
    cwd: String,
    skills: Vec<Skill>,
    errors: Vec<SkillError>,
}
#[derive(Deserialize)]
struct NativeCatalog {
    data: Vec<Entry>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    project_id: String,
    folder_id: String,
    roots_revision: i64,
    skills: Vec<Skill>,
    errors: Vec<SkillError>,
}

async fn project_scope(
    state: &AppState,
    id: &str,
    folder: &str,
    revision: i64,
) -> Result<(StoredProject, String), Failure> {
    let project = state
        .store
        .project(id)
        .await
        .map_err(|_| invalid("Project folders could not be read."))?
        .filter(|p| p.is_included)
        .ok_or_else(|| invalid("This Project is no longer available."))?;
    if project.roots_revision != revision {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "The Project folders changed. Choose the skill again.".into(),
        ));
    }
    let (project, root) = projects::folder_project(state, project, folder)
        .await
        .ok_or_else(|| invalid("Choose a current Project folder."))?;
    let cwd = root.path;
    let checked = project.clone();
    let denied = state.denied_roots.clone();
    tokio::task::spawn_blocking(move || projects::validate_execution_roots(&checked, &denied))
        .await
        .map_err(|_| invalid("Project folders could not be checked."))?
        .map_err(invalid)?;
    Ok((project, cwd))
}

fn project_catalog(value: Value, cwd: &str) -> Result<Entry, Failure> {
    let mut catalog: NativeCatalog = serde_json::from_value(value).map_err(|_| {
        invalid("Codex returned an unreadable skill list. Try opening Skills again.")
    })?;
    let matches = catalog.data.iter().filter(|entry| entry.cwd == cwd).count();
    if matches != 1 {
        return Err(invalid("Codex did not return skills for this folder."));
    }
    let entry = catalog.data.remove(
        catalog
            .data
            .iter()
            .position(|entry| entry.cwd == cwd)
            .unwrap(),
    );
    let mut paths = HashSet::new();
    if entry.errors.len() > 128
        || entry
            .errors
            .iter()
            .any(|e| e.path.len() > 4096 || e.message.len() > 4096)
        || entry.skills.len() > 2048
        || entry.skills.iter().any(|s| {
            s.name.is_empty()
                || s.name.len() > 256
                || s.name.chars().any(char::is_whitespace)
                || !std::path::Path::new(&s.path).is_absolute()
                || s.path.len() > 4096
                || s.description.len() > 16384
                || s.scope.len() > 64
                || !paths.insert(s.path.clone())
        })
    {
        return Err(invalid("Codex returned an invalid skill list."));
    }
    Ok(entry)
}

async fn native_catalog(state: &AppState, cwd: &str) -> Result<Entry, Failure> {
    tokio::time::timeout(Duration::from_secs(8), async {
        let rpc = projects::codex_rpc(state)
            .await
            .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?;
        let response = rpc
            .request("skills/list", json!({"cwds":[cwd],"forceReload":false}))
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Codex skills could not be loaded. Try again.".into(),
                )
            })?;
        let current_health = state.projects.codex.lock().await.health();
        if !rpc.health().is_alive()
            || !current_health.is_alive()
            || current_health.id() != rpc.health().id()
        {
            return Err(invalid("Codex restarted. Open Skills again."));
        }
        if let Some(error) = response.error {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                if error.message.len() <= 4096 {
                    error.message
                } else {
                    "Codex skills could not be loaded.".into()
                },
            ));
        }
        project_catalog(response.result.unwrap_or(Value::Null), cwd)
    })
    .await
    .map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Codex skills took too long to load. Try again.".into(),
        )
    })?
}

async fn read(state: &AppState, id: &str, folder: &str, revision: i64) -> Result<Catalog, Failure> {
    let (_, cwd) = project_scope(state, id, folder, revision).await?;
    let entry = native_catalog(state, &cwd).await?;
    let (_, current) = project_scope(state, id, folder, revision).await?;
    if current != cwd {
        return Err(invalid("The Project folder changed. Open Skills again."));
    }
    Ok(Catalog {
        project_id: id.into(),
        folder_id: folder.into(),
        roots_revision: revision,
        skills: entry.skills,
        errors: entry.errors,
    })
}

pub(crate) async fn list_project(
    State(state): State<AppState>,
    Extension(_): Extension<OwnerAuthority>,
    Path(id): Path<String>,
    Query(scope): Query<ScopeQuery>,
) -> Response {
    match read(&state, &id, &scope.folder_id, scope.roots_revision).await {
        Ok(catalog) => Json(catalog).into_response(),
        Err(error) => error.into_response(),
    }
}
pub(crate) async fn list_conversation(
    State(state): State<AppState>,
    Extension(_): Extension<OwnerAuthority>,
    Path(id): Path<String>,
) -> Response {
    let scope = conversation_scope(&state, &id).await;
    let scope = match scope {
        Ok(scope) => scope,
        Err(error) => return error.into_response(),
    };
    if !scope.family.provider().capabilities.skills {
        return invalid("Skills are available for Codex chats.").into_response();
    }
    match read(&state, &scope.project, &scope.folder, scope.revision).await {
        Ok(catalog) => match check_conversation_scope(&state, &id, &scope).await {
            Ok(()) => Json(catalog).into_response(),
            Err(error) => error.into_response(),
        },
        Err(error) => error.into_response(),
    }
}
#[derive(PartialEq, Eq)]
struct ConversationScope {
    project: String,
    folder: String,
    revision: i64,
    family: AgentFamily,
    provider_store: String,
    native_session: Option<String>,
}
async fn check_conversation_scope(
    state: &AppState,
    id: &str,
    scope: &ConversationScope,
) -> Result<(), Failure> {
    if conversation_scope(state, id).await? != *scope {
        return Err(invalid("This chat changed. Choose the skill again."));
    }
    Ok(())
}
async fn conversation_scope(state: &AppState, id: &str) -> Result<ConversationScope, Failure> {
    let conversation = state
        .store
        .project_conversation(id)
        .await
        .map_err(|_| invalid("This chat could not be read."))?
        .ok_or_else(|| invalid("This Project chat is unavailable."))?;
    let project = state
        .store
        .project(&conversation.project_id)
        .await
        .map_err(|_| invalid("Project folders could not be read."))?
        .ok_or_else(|| invalid("This Project is unavailable."))?;
    let project = crate::projects::thread_project(state, project, &conversation.cwd).await;
    let folder = project
        .root_for(&conversation.cwd)
        .ok_or_else(|| invalid("This chat's folder was removed from its Project."))?;
    Ok(ConversationScope {
        project: project.id.clone(),
        folder: folder.id.clone(),
        revision: project.roots_revision,
        family: conversation.family,
        provider_store: conversation.provider_store,
        native_session: conversation.native_session_id,
    })
}

pub(crate) async fn validate(
    state: &AppState,
    conversation: &str,
    family: Option<AgentFamily>,
    body: &str,
    selections: &[CodexSkillSelection],
) -> Result<Vec<Value>, Failure> {
    if selections.is_empty() {
        return Ok(vec![]);
    }
    if selections.len() > 8 {
        return Err(invalid("Choose at most eight skills per message."));
    }
    let scope = conversation_scope(state, conversation).await?;
    if !family
        .unwrap_or(scope.family)
        .provider()
        .capabilities
        .skills
    {
        return Err(invalid("This message includes Codex skills. Use Codex or remove the selected skills before sending."));
    }
    let mut paths = HashSet::new();
    for selection in selections {
        if selection.project_id != scope.project
            || selection.folder_id != scope.folder
            || selection.roots_revision != scope.revision
            || selection.name.is_empty()
            || selection.name.len() > 256
            || selection.path.len() > 4096
            || !paths.insert(&selection.path)
            || !wonder_store::skill_token_present(body, &selection.name)
        {
            return Err(invalid("A selected skill no longer matches this message or Project folder. Choose it again."));
        }
    }
    let catalog = read(state, &scope.project, &scope.folder, scope.revision).await?;
    check_conversation_scope(state, conversation, &scope).await?;
    selections.iter().map(|selection| {
        if !catalog.skills.iter().any(|skill| skill.enabled && skill.name == selection.name && skill.path == selection.path) {
            return Err(invalid("A selected skill is no longer available. Restore it on your computer, or send a new message with a current skill."));
        }
        Ok(json!({"type":"skill","name":selection.name,"path":selection.path}))
    }).collect()
}

/// Replays compare durable typed inputs without requiring a currently available
/// provider. New acceptance and actual delivery each query native membership.
pub(crate) fn rejection_response((status, detail): Failure) -> Response {
    if matches!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY | StatusCode::PRECONDITION_FAILED
    ) {
        (
            status,
            Json(json!({"error":"codex_skill_rejected","detail":detail})),
        )
            .into_response()
    } else {
        (status, detail).into_response()
    }
}

pub(crate) async fn validate_acceptance(
    state: &AppState,
    conversation: &str,
    device: &str,
    client: &str,
    family: Option<AgentFamily>,
    body: &str,
    skills: &[CodexSkillSelection],
) -> Result<(), Failure> {
    if let Some(existing) = state
        .store
        .message_by_device_and_client_message_id(device, client)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The saved message could not be checked. Retry this message.".into(),
            )
        })?
    {
        let saved = state
            .store
            .original_message_codex_skills(&existing.id)
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The saved skills could not be checked. Retry this message.".into(),
                )
            })?;
        if saved != skills {
            return Err((
                StatusCode::CONFLICT,
                "This message was already sent with different skills.".into(),
            ));
        }
        return Ok(());
    }
    validate(state, conversation, family, body, skills)
        .await
        .map(|_| ())
}

pub(crate) async fn message_input(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    family: Option<AgentFamily>,
) -> Result<Vec<Value>, String> {
    let skills = state
        .store
        .message_codex_skills(&message.id)
        .await
        .map_err(|e| e.to_string())?;
    validate(
        state,
        &message.conversation_id,
        family,
        &message.body,
        &skills,
    )
    .await
    .map_err(|(_, detail)| detail)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{call, fixture};

    async fn setup() -> (
        tempfile::TempDir,
        AppState,
        String,
        StoredProject,
        CodexSkillSelection,
    ) {
        let (dir, state) = fixture().await;
        let file = dir.path().join("runtime.py");
        let source = std::fs::read_to_string(&file).unwrap().replace("    elif method == 'thread/start':", "    elif method == 'skills/list':\n        result = json.load(open(root + '/skills.json'))\n        if __import__('os').path.exists(root + '/hold-skills'):\n            open(root + '/skills-read', 'w').close()\n            while __import__('os').path.exists(root + '/hold-skills'): __import__('time').sleep(0.01)\n    elif method == 'thread/start':");
        state.projects.shutdown().await;
        std::fs::write(file, source).unwrap();
        let cwd = dir.path().join("source");
        std::fs::create_dir(&cwd).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let conversation = crate::test_support::project_thread(
            &state.store,
            &id,
            "Skills",
            cwd.to_str().unwrap(),
            &state.projects.codex_store,
            Some("fake"),
            None,
            "now",
        )
        .await
        .unwrap();
        let project = state
            .store
            .project(&conversation.project_id)
            .await
            .unwrap()
            .unwrap();
        let skill = CodexSkillSelection {
            name: "review".into(),
            path: dir
                .path()
                .join("user-skills/review/SKILL.md")
                .to_string_lossy()
                .into(),
            project_id: project.id.clone(),
            folder_id: project.primary_root_id.clone(),
            roots_revision: 1,
        };
        // The skill path deliberately does not exist: only the native catalog
        // has authority. Wonder must never scan or parse SKILL.md.
        std::fs::write(dir.path().join("skills.json"), json!({"data":[{"cwd":conversation.cwd,"skills":[
            {"name":"review","path":skill.path,"enabled":true,"description":"Review changes","scope":"user"},
            {"name":"review","path":"/another/review/SKILL.md","enabled":true,"description":"Project review","scope":"repo"},
            {"name":"disabled","path":"/disabled/SKILL.md","enabled":false,"description":"Disabled","scope":"user"}],
            "errors":[{"path":"/broken/SKILL.md","message":"A skill could not load"}]}]}).to_string()).unwrap();
        // The synthetic runtime is already protocol-validated by the owning
        // fixture. Restart its same child directly to avoid capability reads.
        let config = state.projects.config.lock().unwrap().clone();
        state
            .projects
            .codex
            .lock()
            .await
            .restart(config)
            .await
            .unwrap();
        (dir, state, id, project, skill)
    }

    #[tokio::test]
    async fn catalog_and_typed_delivery_use_exact_native_scope_and_message_identity() {
        let (dir, state, conversation, project, skill) = setup().await;
        std::fs::write(dir.path().join("payloads"), "").unwrap();
        let (status, catalog) = call(
            &state,
            "GET",
            &format!(
                "/api/v1/projects/{}/skills?folderId={}&rootsRevision=1",
                project.id, project.primary_root_id
            ),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{catalog}");
        crate::test_support::assert_http_contract("codexSkillCatalog", &catalog);
        assert_eq!(catalog["skills"].as_array().unwrap().len(), 3);
        assert_eq!(catalog["errors"][0]["message"], "A skill could not load");
        let payloads = std::fs::read_to_string(dir.path().join("payloads")).unwrap();
        let requests: Vec<Value> = payloads
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            requests.len(),
            1,
            "Before first Send discovery must do no thread or turn work"
        );
        assert_eq!(requests[0]["method"], "skills/list");
        assert_eq!(
            requests[0]["params"],
            json!({"cwds":[project.primary_root().path],"forceReload":false})
        );
        let _service = crate::ingestion::spawn(state.clone()).await;
        for _ in 0..6000 {
            if state.ingestion.project_readiness(&state.store).await.ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(state.ingestion.project_readiness(&state.store).await.ready);
        for option in state
            .runtime_catalog
            .write()
            .await
            .models
            .iter_mut()
            .filter(|m| m.id == "fake")
        {
            option.service_tiers = vec![crate::ChoiceOption {
                id: "default".into(),
                label: "Standard".into(),
                description: None,
            }];
            option.default_service_tier = Some("default".into());
        }
        let client = uuid::Uuid::new_v4().to_string();
        let request = json!({"deviceId":"owner","clientMessageId":client,"body":"Please $review this","codexSkills":[skill]});
        let path = format!("/api/v1/conversations/{conversation}/messages");
        let accepted = call(&state, "POST", &path, request.clone()).await;
        assert_eq!(accepted.0, StatusCode::ACCEPTED, "{}", accepted.1);
        let replay = call(&state, "POST", &path, request.clone()).await;
        assert_eq!(replay.0, StatusCode::ACCEPTED);
        let native_bytes = std::fs::read(dir.path().join("skills.json")).unwrap();
        std::fs::write(dir.path().join("skills.json"), "{\"data\":[]}").unwrap();
        assert_eq!(
            call(&state, "POST", &path, request.clone()).await.0,
            StatusCode::ACCEPTED
        );
        std::fs::write(dir.path().join("skills.json"), native_bytes).unwrap();
        let mut other = request.clone();
        other["codexSkills"][0]["path"] = json!("/another/review/SKILL.md");
        assert_eq!(
            call(&state, "POST", &path, other).await.0,
            StatusCode::CONFLICT
        );
        let message = state
            .store
            .message_by_device_and_client_message_id("owner", &client)
            .await
            .unwrap()
            .unwrap();
        assert!(state
            .store
            .claim_message_for_dispatch(&message.id)
            .await
            .unwrap());
        let mut submitting = false;
        crate::projects::dispatch_inner(&state, &message, &mut submitting)
            .await
            .unwrap();
        let payloads = std::fs::read_to_string(dir.path().join("payloads")).unwrap();
        let turn: Value = payloads
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|r| r["method"] == "turn/start")
            .unwrap();
        assert!(turn["params"]["input"]
            .as_array()
            .unwrap()
            .contains(&json!({"type":"skill","name":"review","path":skill.path})));
        assert_eq!(turn["params"]["input"].as_array().unwrap().len(), 2);
        state
            .store
            .update_message_delivery(&message.id, "streaming", Some("thread"), Some("turn"))
            .await
            .unwrap();
        let guide_id = uuid::Uuid::new_v4().to_string();
        let guide_request = json!({"deviceId":"owner","clientMessageId":guide_id,"body":"Now $review this","expectedTurnId":"turn","codexSkills":[skill]});
        let guide = call(
            &state,
            "POST",
            &format!("/api/v1/conversations/{conversation}/turns/turn/steer"),
            guide_request,
        )
        .await;
        assert_eq!(guide.0, StatusCode::ACCEPTED, "{}", guide.1);
        let guide = state
            .store
            .message_by_device_and_client_message_id("owner", &guide_id)
            .await
            .unwrap()
            .unwrap();
        let response = crate::dispatch_guide(state.clone(), guide, "turn".into()).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let payloads = std::fs::read_to_string(dir.path().join("payloads")).unwrap();
        let guide: Value = payloads
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|r| r["method"] == "turn/steer")
            .unwrap();
        assert!(guide["params"]["input"]
            .as_array()
            .unwrap()
            .contains(&json!({"type":"skill","name":"review","path":skill.path})));
        let waiting_id = uuid::Uuid::new_v4().to_string();
        let waiting_request = json!({"deviceId":"owner","clientMessageId":waiting_id,"body":"Next $review this","expectedTurnId":"turn","codexSkills":[skill]});
        assert_eq!(
            call(
                &state,
                "POST",
                &format!("/api/v1/conversations/{conversation}/turns/turn/steer"),
                waiting_request
            )
            .await
            .0,
            StatusCode::ACCEPTED
        );
        let waiting = state
            .store
            .message_by_device_and_client_message_id("owner", &waiting_id)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(dir.path().join("skills.json"), "{\"data\":[]}").unwrap();
        let response = crate::dispatch_guide(state.clone(), waiting.clone(), "turn".into()).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(state.store.is_durable_dispatch(&waiting.id).await.unwrap());
        assert_eq!(
            state.store.message_codex_skills(&waiting.id).await.unwrap(),
            vec![skill]
        );
        let payloads = std::fs::read_to_string(dir.path().join("payloads")).unwrap();
        assert_eq!(
            payloads
                .lines()
                .filter(
                    |line| serde_json::from_str::<Value>(line).unwrap()["method"] == "turn/steer"
                )
                .count(),
            1,
            "A refused Guide must not reach the provider"
        );
        state.projects.shutdown().await;
    }

    #[tokio::test]
    async fn catalog_changes_and_scope_or_provider_mismatch_refuse_before_turn_work() {
        let (dir, state, conversation, _, skill) = setup().await;
        for (body, family, selection) in [
            ("$review", Some(AgentFamily::Claude), skill.clone()),
            ("$reviewer", None, skill.clone()),
            (
                "$review",
                None,
                CodexSkillSelection {
                    roots_revision: 2,
                    ..skill.clone()
                },
            ),
            (
                "$review",
                None,
                CodexSkillSelection {
                    folder_id: "elsewhere".into(),
                    ..skill.clone()
                },
            ),
        ] {
            assert!(validate(&state, &conversation, family, body, &[selection])
                .await
                .is_err());
        }
        let mut native: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("skills.json")).unwrap())
                .unwrap();
        native["data"][0]["skills"][0]["enabled"] = json!(false);
        std::fs::write(dir.path().join("skills.json"), native.to_string()).unwrap();
        assert!(validate(&state, &conversation, None, "$review", &[skill])
            .await
            .is_err());
        native["data"][0]["cwd"] = json!("/unrelated");
        assert!(project_catalog(native, "/wanted").is_err());
        // Ordinary sends don't initialize a provider just to inspect skills.
        state.projects.shutdown().await;
        assert_eq!(
            validate(&state, "not-a-chat", None, "ordinary", &[])
                .await
                .unwrap(),
            Vec::<Value>::new()
        );
        assert!(!state.projects.codex_alive().await);
        let requests = std::fs::read_to_string(dir.path().join("requests")).unwrap();
        assert!(!requests
            .lines()
            .any(|m| matches!(m, "thread/start" | "turn/start" | "turn/steer")));
    }
    #[tokio::test]
    async fn native_deadline_includes_waiting_for_the_runtime_owner() {
        let (_dir, state, _, project, _) = setup().await;
        let owner = state.projects.codex.lock().await;
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        let error = native_catalog(&state, &project.primary_root().path)
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!((Duration::from_secs(8)..Duration::from_millis(8010)).contains(&start.elapsed()));
        drop(owner);
        tokio::time::resume();
        state.projects.shutdown().await;
    }

    #[tokio::test]
    async fn a_late_native_catalog_cannot_reuse_changed_project_roots() {
        let (dir, state, _, project, _) = setup().await;
        let hold = dir.path().join("hold-skills");
        std::fs::write(&hold, "").unwrap();
        let reader_state = state.clone();
        let reader_project = project.clone();
        let task = tokio::spawn(async move {
            read(
                &reader_state,
                &reader_project.id,
                &reader_project.primary_root_id,
                1,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(60), async {
            while !dir.path().join("skills-read").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let root = project.primary_root();
        state
            .store
            .update_project_roots(
                &project.id,
                1,
                &[wonder_store::ProjectRootInput {
                    path: root.path.clone(),
                    canonical_path: root.canonical_path.clone(),
                }],
                0,
                "later",
            )
            .await
            .unwrap();
        std::fs::remove_file(hold).unwrap();
        let error = task
            .await
            .unwrap()
            .err()
            .expect("Late catalog must be refused");
        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        state.projects.shutdown().await;
    }

    #[test]
    fn malformed_or_unbounded_native_lists_are_not_known_empty() {
        let valid = json!({"data":[{"cwd":"/project","skills":[],"errors":[]}]});
        assert!(project_catalog(valid.clone(), "/project")
            .unwrap()
            .skills
            .is_empty());
        let mut cases = vec![
            json!({"data":[]}),
            json!({"data":[valid["data"][0],valid["data"][0]]}),
        ];
        let mut oversized = valid.clone();
        oversized["data"][0]["errors"] = json!([{"path":"/skill","message":"x".repeat(4097)}]);
        cases.push(oversized);
        let skill = json!({"name":"review","path":"/skill","enabled":true,"scope":"repo","description":"review"});
        for (field, value) in [
            ("description", json!("x".repeat(16385))),
            ("scope", json!("x".repeat(65))),
            ("path", json!("relative")),
        ] {
            let mut item = skill.clone();
            item[field] = value;
            let mut catalog = valid.clone();
            catalog["data"][0]["skills"] = json!([item]);
            cases.push(catalog);
        }
        let mut duplicate = valid.clone();
        duplicate["data"][0]["skills"] = json!([skill, skill]);
        cases.push(duplicate);
        for case in cases {
            assert!(project_catalog(case, "/project").is_err());
        }
    }
    #[tokio::test]
    async fn receipt_read_failure_is_uncertain_even_with_an_accepted_message() {
        let (dir, state, conversation, _, skill) = setup().await;
        state
            .store
            .insert_message_with_skills(
                "owner",
                "receipt",
                "$review",
                "hash",
                &conversation,
                &[],
                "now",
                None,
                std::slice::from_ref(&skill),
            )
            .await
            .unwrap();
        let queued = state.store.pending_queue(&conversation).await.unwrap();
        assert!(state
            .store
            .edit_pending(
                &conversation,
                &queued[0].id,
                queued[0].revision,
                Some(("$reviewer", "edited"))
            )
            .await
            .unwrap());
        assert!(
            validate_acceptance(
                &state,
                &conversation,
                "owner",
                "receipt",
                None,
                "$review",
                std::slice::from_ref(&skill)
            )
            .await
            .is_ok(),
            "Skill replay identity survives a queue edit without fresh native membership"
        );
        assert_eq!(
            validate_acceptance(
                &state,
                &conversation,
                "owner",
                "receipt",
                None,
                "$reviewer",
                &[]
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        assert!(
            matches!(
                state
                    .store
                    .insert_message_with_skills(
                        "owner",
                        "receipt",
                        "$review",
                        "hash",
                        &conversation,
                        &[],
                        "now",
                        None,
                        std::slice::from_ref(&skill)
                    )
                    .await
                    .unwrap(),
                wonder_store::MessageInsert::Conflict
            ),
            "The existing generic edited-body replay contract stays unchanged"
        );
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        sqlx::query(
            "UPDATE messages SET original_codex_skills_json='broken' WHERE client_message_id='receipt'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let error = validate_acceptance(
            &state,
            &conversation,
            "owner",
            "receipt",
            None,
            "$review",
            std::slice::from_ref(&skill),
        )
        .await
        .unwrap_err();
        assert_eq!(
            rejection_response(error).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "An accepted receipt with unreadable typed input cannot be declared unsent"
        );
        sqlx::query("ALTER TABLE messages RENAME TO unavailable_messages")
            .execute(&pool)
            .await
            .unwrap();
        let error = validate_acceptance(
            &state,
            &conversation,
            "owner",
            "receipt",
            None,
            "$review",
            &[skill],
        )
        .await
        .unwrap_err();
        assert_eq!(
            rejection_response(error).status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "A failed receipt lookup must preserve the pending request identity"
        );
        sqlx::query("ALTER TABLE unavailable_messages RENAME TO messages")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        state.projects.shutdown().await;
    }
}
