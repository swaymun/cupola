//! Project goals. The owning runtime keeps the objective and
//! status; Wonder stores only its thread mapping and optional active-time limit.
use crate::{projects, subagents, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Map, Value};
use std::path::Path as FsPath;
use std::sync::atomic::{AtomicI64, Ordering};
use wonder_app_server::RpcClient;
use wonder_store::EXECUTION_SCOPE_PROJECTS;

static LAST_LIMIT_CHECK_MS: AtomicI64 = AtomicI64::new(0);

struct GoalRoute {
    thread: String,
    rpc: RpcClient,
}

/// `check_folders` refuses a Project whose folder now points somewhere else
/// before its runtime is started or asked anything, so the refusal never
/// depends on whether that runtime is up.
async fn direct_thread(
    state: &AppState,
    conversation: &str,
    check_folders: bool,
) -> Result<Option<GoalRoute>, Box<Response>> {
    if let Some(response) = subagents::reject_user_mutation(state, conversation).await {
        return Err(Box::new(response));
    }
    if let Some(project_conversation) = state
        .store
        .project_conversation(conversation)
        .await
        .map_err(|_| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?
    {
        if !project_conversation.family.provider().capabilities.goals {
            return Err(Box::new(
                (
                    StatusCode::CONFLICT,
                    "Goals are not available for Claude yet.",
                )
                    .into_response(),
            ));
        }
        let project = state
            .store
            .project(&project_conversation.project_id)
            .await
            .map_err(|_| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?
            .ok_or_else(|| Box::new(StatusCode::NOT_FOUND.into_response()))?;
        let project =
            crate::projects::thread_project(state, project, &project_conversation.cwd).await;
        if !project.is_included
            || !project
                .root_for(&project_conversation.cwd)
                .is_some_and(|root| FsPath::new(&root.canonical_path).is_dir())
        {
            return Err(Box::new(
                (
                    StatusCode::CONFLICT,
                    "This Project is no longer available. Review its folders before continuing.",
                )
                    .into_response(),
            ));
        }
        if !crate::providers::is_default_store(
            state,
            project_conversation.family,
            &project_conversation.provider_store,
        ) {
            return Err(Box::new(
                (
                    StatusCode::CONFLICT,
                    "This Project thread belongs to a different Codex history.",
                )
                    .into_response(),
            ));
        }
        if check_folders {
            let checked = project.clone();
            let denied = state.denied_roots.clone();
            match tokio::task::spawn_blocking(move || {
                projects::validate_execution_roots(&checked, &denied)
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(detail)) => {
                    return Err(Box::new((StatusCode::CONFLICT, detail).into_response()))
                }
                Err(_) => {
                    return Err(Box::new(
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            "Project folders could not be checked. Try again.",
                        )
                            .into_response(),
                    ))
                }
            }
        }
        let binding = state
            .store
            .runtime_binding(conversation)
            .await
            .map_err(|_| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?;
        let (native, binding) = match (project_conversation.native_session_id, binding) {
            (None, None) => return Ok(None),
            (Some(native), Some(binding)) => (native, binding),
            _ => {
                return Err(Box::new((StatusCode::CONFLICT, "This Project thread's runtime identity changed. Reopen it before changing its Goal.").into_response()));
            }
        };
        let recorded = state
            .store
            .conversation_thread(conversation)
            .await
            .map_err(|_| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?;
        if !binding.family.provider().capabilities.goals
            || binding.execution_scope != EXECUTION_SCOPE_PROJECTS
            || binding.thread_id != native
            || recorded.as_deref() != Some(native.as_str())
        {
            return Err(Box::new((StatusCode::CONFLICT, "This Project thread's runtime identity changed. Reopen it before changing its Goal.").into_response()));
        }
        let rpc = crate::providers::rpc_for_store(
            state,
            project_conversation.family,
            &project_conversation.provider_store,
        )
        .await
        .map_err(|_| {
            Box::new(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Project Goals are unavailable on this computer. Try again.",
                )
                    .into_response(),
            )
        })?;
        return Ok(Some(GoalRoute {
            thread: native,
            rpc,
        }));
    }
    Err(Box::new(StatusCode::NOT_FOUND.into_response()))
}

async fn runtime_goal(route: &GoalRoute) -> Result<Value, Box<Response>> {
    let response = route
        .rpc
        .request("thread/goal/get", json!({"threadId":route.thread}))
        .await
        .map_err(|_| {
            Box::new(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Goal status is unavailable. Try again.",
                )
                    .into_response(),
            )
        })?;
    if response.error.is_some() {
        return Err(Box::new(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Goal status is unavailable. Try again.",
            )
                .into_response(),
        ));
    }
    Ok(response.result.unwrap_or(json!({"goal":null})))
}

async fn decorate(
    state: &AppState,
    conversation: &str,
    thread: &str,
    mut result: Value,
) -> Result<Value, Box<Response>> {
    if result.get("goal").is_some_and(|goal| !goal.is_null()) {
        state
            .store
            .set_goal_thread(conversation, thread)
            .await
            .map_err(|_| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?;
        // The first read of a goal the runtime started before this chat was
        // bound replays what arrived meanwhile.
        let pending = state
            .store
            .pending_app_server_notifications_for_thread(thread)
            .await
            .map_err(|_| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?;
        if !pending.is_empty() {
            crate::drain_goal_thread(state, thread).await;
        }
        if let Some(limit) = state
            .store
            .goal_time_limit(conversation)
            .await
            .map_err(|_| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?
        {
            if limit.thread_id == thread {
                result["goal"]["timeBudgetSeconds"] = json!(limit.budget_seconds);
            }
        }
    } else {
        state
            .store
            .clear_goal_thread(thread)
            .await
            .map_err(|_| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?;
        state
            .store
            .clear_goal_time_limit(conversation)
            .await
            .map_err(|_| Box::new(StatusCode::SERVICE_UNAVAILABLE.into_response()))?;
    }
    Ok(result)
}

pub async fn get(State(state): State<AppState>, Path(conversation): Path<String>) -> Response {
    let route = match direct_thread(&state, &conversation, false).await {
        Ok(Some(route)) => route,
        Ok(None) => return Json(json!({"goal":null})).into_response(),
        Err(response) => return *response,
    };
    match runtime_goal(&route).await {
        Ok(result) => match decorate(&state, &conversation, &route.thread, result).await {
            Ok(result) => Json(result).into_response(),
            Err(response) => *response,
        },
        Err(response) => *response,
    }
}

pub async fn set(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let route = match direct_thread(&state, &conversation, true).await {
        Ok(Some(route)) => route,
        Ok(None) => {
            return (
                StatusCode::CONFLICT,
                "Send a message before setting a Goal.",
            )
                .into_response()
        }
        Err(response) => return *response,
    };
    let Some(input) = body.as_object() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut params = Map::new();
    params.insert("threadId".into(), json!(route.thread));
    if let Some(objective) = input.get("objective") {
        let Some(objective) = objective
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= 4096)
        else {
            return (
                StatusCode::BAD_REQUEST,
                "Goal text must be between 1 and 4096 characters.",
            )
                .into_response();
        };
        params.insert("objective".into(), json!(objective));
    }
    if let Some(status) = input.get("status") {
        let Some(status) = status
            .as_str()
            .filter(|status| matches!(*status, "active" | "paused"))
        else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        params.insert("status".into(), json!(status));
    }
    if let Some(budget) = input.get("tokenBudget") {
        if !budget.is_null() && !budget.as_i64().is_some_and(|budget| budget > 0) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        params.insert("tokenBudget".into(), budget.clone());
    }
    let time_budget = input.get("timeBudgetSeconds");
    if let Some(time_budget) = time_budget {
        if !time_budget.is_null() && !time_budget.as_i64().is_some_and(|seconds| seconds > 0) {
            return StatusCode::BAD_REQUEST.into_response();
        }
    }
    if params.len() == 1 && time_budget.is_none() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let response = if params.len() > 1 {
        match route
            .rpc
            .request("thread/goal/set", Value::Object(params))
            .await
        {
            Ok(response) if response.error.is_none() => response,
            _ => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Goal could not be saved. Try again.",
                )
                    .into_response()
            }
        }
    } else {
        match route
            .rpc
            .request("thread/goal/get", json!({"threadId":route.thread}))
            .await
        {
            Ok(response) if response.error.is_none() => response,
            _ => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Goal could not be loaded. Try again.",
                )
                    .into_response()
            }
        }
    };
    if let Some(time_budget) = time_budget {
        if state
            .store
            .set_goal_time_limit(&conversation, &route.thread, time_budget.as_i64())
            .await
            .is_err()
        {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    match decorate(
        &state,
        &conversation,
        &route.thread,
        response.result.unwrap_or(json!({"goal":null})),
    )
    .await
    {
        Ok(result) => Json(result).into_response(),
        Err(response) => *response,
    }
}

pub async fn clear(State(state): State<AppState>, Path(conversation): Path<String>) -> Response {
    let route = match direct_thread(&state, &conversation, false).await {
        Ok(Some(route)) => route,
        Ok(None) => return StatusCode::NO_CONTENT.into_response(),
        Err(response) => return *response,
    };
    match route
        .rpc
        .request("thread/goal/clear", json!({"threadId":route.thread}))
        .await
    {
        Ok(response) if response.error.is_none() => {
            if state.store.clear_goal_thread(&route.thread).await.is_err()
                || state
                    .store
                    .clear_goal_time_limit(&conversation)
                    .await
                    .is_err()
            {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            StatusCode::NO_CONTENT.into_response()
        }
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Goal could not be removed. Try again.",
        )
            .into_response(),
    }
}

/// The runtime counts only active Goal time, so pauses do not consume a time limit.
pub async fn enforce_time_limits(state: &AppState) {
    let now = crate::now_ms() as i64;
    let last = LAST_LIMIT_CHECK_MS.load(Ordering::Relaxed);
    if now - last < 5_000
        || LAST_LIMIT_CHECK_MS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    enforce_time_limits_now(state).await;
}

pub(crate) async fn enforce_time_limits_now(state: &AppState) {
    let Ok(limits) = state.store.active_goal_time_limits().await else {
        return;
    };
    for limit in limits {
        let Ok(Some(route)) = direct_thread(state, &limit.conversation_id, false).await else {
            continue;
        };
        if route.thread != limit.thread_id {
            continue;
        }
        let Ok(response) = route
            .rpc
            .request("thread/goal/get", json!({"threadId":limit.thread_id}))
            .await
        else {
            continue;
        };
        let Some(goal) = response
            .result
            .as_ref()
            .and_then(|result| result.get("goal"))
        else {
            continue;
        };
        if goal.is_null() {
            let _ = state
                .store
                .clear_goal_time_limit(&limit.conversation_id)
                .await;
            let _ = state.store.clear_goal_thread(&limit.thread_id).await;
        } else if goal.get("status").and_then(Value::as_str) == Some("active")
            && goal
                .get("timeUsedSeconds")
                .and_then(Value::as_i64)
                .is_some_and(|used| used >= limit.budget_seconds)
        {
            let _ = route
                .rpc
                .request(
                    "thread/goal/set",
                    json!({"threadId":limit.thread_id,"status":"paused"}),
                )
                .await;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
    };
    use wonder_store::AgentFamily;
    use wonder_store::{ProjectConversationInsert, ProjectRootInput};

    async fn fixture() -> (tempfile::TempDir, AppState) {
        let (dir, mut state) = crate::test_support::fixture().await;
        let original = dir.path().join("codex");
        let project_bin = dir.path().join("project-codex");
        let script = fs::read_to_string(&original).unwrap().replacen(
            "#!/bin/sh",
            "#!/bin/sh\nexport WONDER_FIXTURE_SCOPE=project",
            1,
        );
        fs::write(&project_bin, script).unwrap();
        fs::set_permissions(&project_bin, fs::Permissions::from_mode(0o755)).unwrap();
        state.projects = projects::ProjectRuntime::configured(
            project_bin,
            "test".into(),
            &dir.path().join("codex-home"),
            &dir.path().join("claude-home"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        let root = dir.path().join("project-source");
        fs::create_dir(&root).unwrap();
        let path = root.to_str().unwrap();
        state
            .store
            .create_project(
                "project",
                "create-project",
                "hash",
                "Project",
                &[ProjectRootInput {
                    path: path.into(),
                    canonical_path: crate::canonical_path::canonicalize(&root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .into(),
                }],
                0,
                "now",
            )
            .await
            .unwrap();
        state
            .store
            .update_project_metadata("project", None, Some(true), None, "now")
            .await
            .unwrap();
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "project-chat",
                project_id: "project",
                family: AgentFamily::Codex,
                provider_store: &state.projects.codex_store,
                native_session_id: None,
                cwd: path,
                roots_revision: 1,
                title: "Project chat",
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
        state
            .store
            .set_project_native_session("project-chat", "project-thread", "now")
            .await
            .unwrap();
        state
            .store
            .bind_project_runtime(
                "project-chat",
                AgentFamily::Codex,
                &state.projects.codex_store,
                "project-thread",
                None,
                "now",
            )
            .await
            .unwrap();
        (dir, state)
    }

    fn goal_requests(dir: &tempfile::TempDir) -> Vec<Value> {
        fs::read_to_string(dir.path().join("routing-requests-jsonl"))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|request| {
                request["method"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("thread/goal/")
            })
            .collect()
    }

    #[tokio::test]
    async fn project_goal_uses_its_owner_runtime_for_controls_and_active_time() {
        let (dir, state) = fixture().await;
        let initial = get(State(state.clone()), Path("project-chat".into())).await;
        assert_eq!(initial.status(), StatusCode::OK);
        let created = set(
            State(state.clone()),
            Path("project-chat".into()),
            Json(json!({"objective":"Finish the Project review","timeBudgetSeconds":60})),
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK);
        assert_eq!(
            state
                .store
                .goal_conversation_for_thread("project-thread")
                .await
                .unwrap()
                .as_deref(),
            Some("project-chat")
        );
        let fetched = get(State(state.clone()), Path("project-chat".into())).await;
        assert_eq!(fetched.status(), StatusCode::OK);
        let body = axum::body::to_bytes(fetched.into_body(), 65_536)
            .await
            .unwrap();
        let result: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(result["goal"]["timeBudgetSeconds"], 60);

        let goal_path = dir.path().join("goal.json");
        let mut goal: Value = serde_json::from_slice(&fs::read(&goal_path).unwrap()).unwrap();
        goal["timeUsedSeconds"] = json!(60);
        fs::write(&goal_path, serde_json::to_vec(&goal).unwrap()).unwrap();
        enforce_time_limits_now(&state).await;
        let goal: Value = serde_json::from_slice(&fs::read(&goal_path).unwrap()).unwrap();
        assert_eq!(goal["status"], "paused");

        let removed = clear(State(state.clone()), Path("project-chat".into())).await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        assert!(state
            .store
            .goal_time_limit("project-chat")
            .await
            .unwrap()
            .is_none());
        let requests = goal_requests(&dir);
        for method in ["thread/goal/get", "thread/goal/set", "thread/goal/clear"] {
            assert!(
                requests.iter().any(|request| request["scope"] == "project"
                    && request["method"] == method
                    && request["params"]["threadId"] == "project-thread"),
                "{method}"
            );
        }
        assert!(requests.iter().any(|request| request["scope"] == "project"
            && request["method"] == "thread/goal/set"
            && request["params"]["status"] == "paused"));
        assert!(requests.iter().any(
            |request| request["scope"] == "project" && request["method"] == "thread/goal/clear"
        ));
        assert!(requests
            .iter()
            .filter(|request| request["params"]["threadId"] == "project-thread")
            .all(|request| request["scope"] == "project"));
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Contract: a thread working in a linked worktree of the Project's folder
    // routes its Goal to its own runtime thread, like one in the folder itself.
    #[tokio::test]
    async fn worktree_thread_goal_routes_to_its_runtime() {
        let (dir, state) = fixture().await;
        let source = dir.path().join("project-source");
        let worktree = dir.path().join("goal-worktree");
        let git = |args: &[&str]| {
            let output = crate::test_git::git()
                .arg("-C")
                .arg(&source)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ]);
        git(&[
            "worktree",
            "add",
            "-qb",
            "agent/worktree",
            worktree.to_str().unwrap(),
        ]);
        let worktree = crate::canonical_path::canonicalize(&worktree).unwrap();
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "worktree-chat",
                project_id: "project",
                family: AgentFamily::Codex,
                provider_store: &state.projects.codex_store,
                native_session_id: None,
                cwd: worktree.to_str().unwrap(),
                roots_revision: 1,
                title: "Worktree chat",
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
        state
            .store
            .set_project_native_session("worktree-chat", "worktree-thread", "now")
            .await
            .unwrap();
        state
            .store
            .bind_project_runtime(
                "worktree-chat",
                AgentFamily::Codex,
                &state.projects.codex_store,
                "worktree-thread",
                None,
                "now",
            )
            .await
            .unwrap();
        let fetched = get(State(state.clone()), Path("worktree-chat".into())).await;
        assert_eq!(fetched.status(), StatusCode::OK);
        let created = set(
            State(state.clone()),
            Path("worktree-chat".into()),
            Json(json!({"objective":"Finish the worktree"})),
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK);
        let body = axum::body::to_bytes(created.into_body(), 65_536)
            .await
            .unwrap();
        let result: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            result["goal"]["objective"], "Finish the worktree",
            "{result}"
        );
        let requests = goal_requests(&dir);
        for method in ["thread/goal/get", "thread/goal/set"] {
            assert!(
                requests.iter().any(|request| request["scope"] == "project"
                    && request["method"] == method
                    && request["params"]["threadId"] == "worktree-thread"),
                "{method} {requests:?}"
            );
        }
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn project_goal_rejects_replaced_folder_before_provider_mutation() {
        let (dir, state) = fixture().await;
        let source = dir.path().join("project-source");
        let original = dir.path().join("original-source");
        let replacement = dir.path().join("replacement-source");
        fs::rename(&source, &original).unwrap();
        fs::create_dir(&replacement).unwrap();
        symlink(&replacement, &source).unwrap();

        let before = goal_requests(&dir).len();
        let response = set(
            State(state.clone()),
            Path("project-chat".into()),
            Json(json!({"objective":"Inspect the changed folder"})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(goal_requests(&dir).len(), before);
        // Refused before the runtime starts, so a slow or failed start can
        // never answer in its place.
        assert!(!state.projects.codex.lock().await.health().is_alive());
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn project_goal_rejects_revoked_claude_and_unbound_threads() {
        let (dir, state) = fixture().await;
        let path = dir.path().join("project-source");
        for (id, family, store, native) in [
            (
                "claude-chat",
                AgentFamily::Claude,
                state.projects.claude_store.as_str(),
                None,
            ),
            (
                "unbound-chat",
                AgentFamily::Codex,
                state.projects.codex_store.as_str(),
                Some("unbound-thread"),
            ),
            (
                "foreign-chat",
                AgentFamily::Codex,
                "codex:another-home",
                None,
            ),
        ] {
            state
                .store
                .create_project_conversation(ProjectConversationInsert {
                    conversation_id: id,
                    project_id: "project",
                    family,
                    provider_store: store,
                    native_session_id: native,
                    cwd: path.to_str().unwrap(),
                    roots_revision: 1,
                    title: id,
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
        }
        for id in ["claude-chat", "unbound-chat", "foreign-chat"] {
            assert_eq!(
                get(State(state.clone()), Path(id.into())).await.status(),
                StatusCode::CONFLICT
            );
            assert_eq!(
                set(
                    State(state.clone()),
                    Path(id.into()),
                    Json(json!({"objective":"Unsafe"}))
                )
                .await
                .status(),
                StatusCode::CONFLICT
            );
        }
        let parent = "project-chat".to_owned();
        crate::test_support::bind_project_thread(
            &state,
            &parent,
            wonder_store::AgentFamily::Codex,
            "project-thread",
            None,
            "now",
        )
        .await
        .unwrap();
        state
            .store
            .register_subagent_ownership(
                "child-chat",
                &parent,
                "child-thread",
                "project-thread",
                Some("Child"),
                None,
                None,
                r#"{"subAgent":{"thread_spawn":{"parent_thread_id":"project-thread","depth":1}}}"#,
                Some("runtime"),
                Some(false),
                "idle",
                None,
                "now",
            )
            .await
            .unwrap();
        assert_eq!(
            get(State(state.clone()), Path("child-chat".into()))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        state
            .store
            .update_project_metadata("project", None, Some(false), None, "later")
            .await
            .unwrap();
        assert_eq!(
            get(State(state.clone()), Path("project-chat".into()))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            clear(State(state.clone()), Path("project-chat".into()))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert!(goal_requests(&dir).is_empty());
        state.projects.shutdown().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }
}

/// A goal the runtime runs on its own: its turns carry no Cupola receipt.
#[cfg(all(test, unix))]
mod binding_tests {
    use super::*;
    use crate::process_app_server_notification as process;
    use wonder_store::AgentFamily;

    fn goal(thread: &str) -> Value {
        json!({"threadId":thread,"createdAt":1700000000,"updatedAt":1700000000,
            "objective":"Replay the Windows trace","status":"active",
            "tokensUsed":0,"timeUsedSeconds":0,"tokenBudget":null})
    }

    fn delta(thread: &str, turn: &str, text: &str) -> Value {
        json!({"method":"item/agentMessage/delta","params":{"threadId":thread,"turnId":turn,
            "itemId":format!("{turn}-reply"),"sequence":1,"delta":{"text":text}}})
    }

    async fn replies(state: &AppState, conversation: &str) -> Vec<String> {
        state
            .store
            .assistant_messages_for_conversation(conversation)
            .await
            .unwrap()
            .into_iter()
            .map(|message| message.text)
            .collect()
    }

    // Regression: a goal update for a thread no chat was bound to returned
    // without writing anything, and its turns' replies waited forever.
    #[tokio::test]
    async fn goal_started_before_its_chat_is_bound_replays_on_first_read() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let (chat, thread) = ("goal-unbound-chat", "goal-unbound-thread");
        let update = json!({"method":"thread/goal/updated",
            "params":{"threadId":thread,"turnId":"goal-turn","goal":goal(thread)}});
        assert!(process(&state, update.clone(), false).await);
        assert!(
            process(
                &state,
                delta(thread, "goal-turn", "Tracing the replay"),
                false
            )
            .await
        );
        // Only the latest goal update is kept for a thread nobody opened.
        assert!(process(&state, update, false).await);
        let kept = state
            .store
            .pending_app_server_notifications_for_thread(thread)
            .await
            .unwrap();
        assert_eq!(
            kept.iter()
                .filter(|pending| pending.method == "thread/goal/updated")
                .count(),
            1
        );
        assert_eq!(kept.len(), 2);

        crate::test_support::bind_project_thread(
            &state,
            chat,
            AgentFamily::Codex,
            thread,
            None,
            "now",
        )
        .await
        .unwrap();
        std::fs::write(dir.path().join("goal.json"), goal(thread).to_string()).unwrap();
        let response = get(State(state.clone()), Path(chat.into())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["goal"]["status"], "active");

        assert_eq!(
            state
                .store
                .goal_conversation_for_thread(thread)
                .await
                .unwrap()
                .as_deref(),
            Some(chat)
        );
        assert_eq!(replies(&state, chat).await, ["Tracing the replay"]);
        assert!(state
            .store
            .pending_app_server_notifications_for_thread(thread)
            .await
            .unwrap()
            .is_empty());
    }

    // Regression: a turn the runtime started before announcing the goal was
    // buffered, and only a Cupola dispatch receipt ever drained the buffer.
    #[tokio::test]
    async fn goal_update_drains_turns_buffered_before_it() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let (chat, thread) = ("goal-bound-chat", "goal-bound-thread");
        crate::test_support::bind_project_thread(
            &state,
            chat,
            AgentFamily::Codex,
            thread,
            None,
            "now",
        )
        .await
        .unwrap();
        let start = json!({"method":"turn/started","params":{"threadId":thread,
            "turn":{"id":"first","status":"inProgress"}}});
        assert!(process(&state, start, false).await);
        assert!(process(&state, delta(thread, "first", "Starting the goal"), false).await);
        assert!(replies(&state, chat).await.is_empty());

        let update = json!({"method":"thread/goal/updated",
            "params":{"threadId":thread,"goal":goal(thread)}});
        assert!(process(&state, update, false).await);
        assert_eq!(replies(&state, chat).await, ["Starting the goal"]);
        assert!(state
            .store
            .pending_app_server_notifications_for_thread(thread)
            .await
            .unwrap()
            .is_empty());
    }

    // A turn Cupola is still submitting is that dispatch's to drain, never
    // claimed by the goal.
    #[tokio::test]
    async fn goal_binding_leaves_a_submitting_turn_to_its_dispatch() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let (chat, thread) = ("goal-submit-chat", "goal-submit-thread");
        crate::test_support::bind_project_thread(
            &state,
            chat,
            AgentFamily::Codex,
            thread,
            None,
            "now",
        )
        .await
        .unwrap();
        let wonder_store::MessageInsert::Inserted(message) = state
            .store
            .insert_message("owner", "goal-submit", "hello", "hash", chat, "now")
            .await
            .unwrap()
        else {
            panic!("new message")
        };
        state
            .store
            .update_message_delivery(&message.id, "dispatching_to_codex", None, None)
            .await
            .unwrap();
        assert!(process(&state, delta(thread, "sent", "Answering you"), false).await);
        let update = json!({"method":"thread/goal/updated",
            "params":{"threadId":thread,"goal":goal(thread)}});
        assert!(process(&state, update, false).await);
        assert!(replies(&state, chat).await.is_empty());
        assert_eq!(
            state
                .store
                .goal_conversation_for_turn(thread, "sent")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            state
                .store
                .pending_app_server_notifications_for_thread(thread)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    async fn bound(state: &AppState, chat: &str, thread: &str) {
        crate::test_support::bind_project_thread(
            state,
            chat,
            AgentFamily::Codex,
            thread,
            None,
            "now",
        )
        .await
        .unwrap();
    }

    fn updated(thread: &str, turn: Option<&str>) -> Value {
        let mut params = json!({"threadId":thread,"goal":goal(thread)});
        if let Some(turn) = turn {
            params["turnId"] = json!(turn);
        }
        json!({"method":"thread/goal/updated","params":params})
    }

    // Owner's report: a goal thread started outside Cupola looked idle. Its
    // first goal update arrived before the Mac knew the thread, was dropped,
    // and every reply after it waited for a receipt that never comes.
    #[tokio::test]
    async fn goal_thread_unknown_to_the_mac_shows_its_replies_after_a_goal_update() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let (chat, thread) = ("goal-outside-chat", "goal-outside-thread");
        assert!(process(&state, updated(thread, Some("first")), false).await);
        assert!(process(&state, delta(thread, "first", "Reading the trace"), false).await);
        assert!(process(&state, delta(thread, "second", "Replaying step 24"), false).await);
        assert!(state
            .store
            .goal_conversation_for_thread(thread)
            .await
            .unwrap()
            .is_none());

        bound(&state, chat, thread).await;
        assert!(process(&state, updated(thread, Some("third")), false).await);
        assert_eq!(
            state
                .store
                .goal_conversation_for_thread(thread)
                .await
                .unwrap()
                .as_deref(),
            Some(chat)
        );
        assert_eq!(
            replies(&state, chat).await,
            ["Reading the trace", "Replaying step 24"]
        );
        assert!(state
            .store
            .pending_app_server_notifications_for_thread(thread)
            .await
            .unwrap()
            .is_empty());
        // Later turns go straight to the chat.
        assert!(process(&state, delta(thread, "third", "Step 24 passes"), false).await);
        assert_eq!(
            replies(&state, chat).await,
            ["Reading the trace", "Replaying step 24", "Step 24 passes"]
        );
    }

    async fn fail_replies(dir: &tempfile::TempDir, fail: bool) {
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path().join("state.db").display()
        ))
        .await
        .unwrap();
        let sql = if fail {
            "CREATE TRIGGER fail_replay BEFORE INSERT ON assistant_messages BEGIN SELECT RAISE(ABORT, 'replay write fault'); END"
        } else {
            "DROP TRIGGER fail_replay"
        };
        sqlx::query(sql).execute(&pool).await.unwrap();
    }

    // A replay that fails keeps what it could not write buffered, and the
    // next goal update replays it exactly once.
    #[tokio::test]
    async fn failed_goal_replay_shows_each_reply_once_on_retry() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let (chat, thread) = ("goal-retry-chat", "goal-retry-thread");
        assert!(process(&state, delta(thread, "first", "Reading the trace"), false).await);
        bound(&state, chat, thread).await;
        fail_replies(&dir, true).await;
        assert!(process(&state, updated(thread, None), false).await);
        assert!(replies(&state, chat).await.is_empty());
        assert_eq!(
            state
                .store
                .pending_app_server_notifications_for_thread(thread)
                .await
                .unwrap()
                .len(),
            1
        );
        fail_replies(&dir, false).await;
        assert!(process(&state, updated(thread, None), false).await);
        assert!(process(&state, updated(thread, None), false).await);
        assert_eq!(replies(&state, chat).await, ["Reading the trace"]);
        assert!(state
            .store
            .pending_app_server_notifications_for_thread(thread)
            .await
            .unwrap()
            .is_empty());
    }

    fn chunk(thread: &str, turn: &str, sequence: u64, text: &str) -> Value {
        json!({"method":"item/agentMessage/delta","params":{"threadId":thread,"turnId":turn,
            "itemId":format!("{turn}-reply"),"sequence":sequence,"delta":{"text":text}}})
    }

    async fn buffered(state: &AppState, thread: &str) -> usize {
        state
            .store
            .pending_app_server_notifications_for_thread(thread)
            .await
            .unwrap()
            .len()
    }

    // Measured: a reply chunk arriving live while an earlier chunk of the
    // same turn waited in the buffer was written first ("worldHello ").
    #[tokio::test]
    async fn live_chunk_after_a_failed_replay_follows_the_buffered_one() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let (chat, thread) = ("goal-order-chat", "goal-order-thread");
        assert!(process(&state, chunk(thread, "first", 1, "Hello "), false).await);
        bound(&state, chat, thread).await;
        fail_replies(&dir, true).await;
        assert!(process(&state, updated(thread, None), false).await);
        assert!(replies(&state, chat).await.is_empty());
        fail_replies(&dir, false).await;
        assert!(process(&state, chunk(thread, "first", 2, "world"), false).await);
        assert!(process(&state, updated(thread, None), false).await);
        assert_eq!(replies(&state, chat).await, ["Hello world"]);
        assert_eq!(buffered(&state, thread).await, 0);
    }

    // Live chunks that arrive while the replay keeps failing queue behind
    // the buffered one and land in order once it succeeds.
    #[tokio::test]
    async fn live_chunks_during_failed_replays_land_in_order_on_retry() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let (chat, thread) = ("goal-queue-chat", "goal-queue-thread");
        assert!(process(&state, chunk(thread, "first", 1, "Replaying "), false).await);
        bound(&state, chat, thread).await;
        fail_replies(&dir, true).await;
        assert!(process(&state, updated(thread, None), false).await);
        for (sequence, text) in [(2, "step "), (3, "24 "), (4, "passes")] {
            assert!(process(&state, chunk(thread, "first", sequence, text), false).await);
        }
        assert!(replies(&state, chat).await.is_empty());
        assert_eq!(buffered(&state, thread).await, 4);
        fail_replies(&dir, false).await;
        assert!(process(&state, updated(thread, None), false).await);
        assert_eq!(replies(&state, chat).await, ["Replaying step 24 passes"]);
        assert_eq!(buffered(&state, thread).await, 0);
        // With the buffer empty, live chunks go straight to the chat.
        assert!(process(&state, chunk(thread, "first", 5, "."), false).await);
        assert_eq!(replies(&state, chat).await, ["Replaying step 24 passes."]);
        assert_eq!(buffered(&state, thread).await, 0);
    }
}
