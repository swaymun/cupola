//! Product metadata only. Discovery never grants access or resumes waiting work.
use super::*;
use serde_json::{json, Value};

// Pinned runtime built-ins are not user-connected service accounts.
const INTERNAL_APPS: [&str; 4] = [
    "connector_openai_codex_document_control",
    "connector_openai_hotline",
    "connector_openai_plugin_management",
    "connector_openai_safety_settings",
];

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct AppQuery {
    cursor: Option<String>,
    agent_family: Option<AgentFamily>,
    conversation_id: Option<String>,
    #[serde(default)]
    refresh: bool,
}

pub(super) async fn list(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Query(query): Query<AppQuery>,
) -> Response {
    if query.cursor.as_ref().is_some_and(|c| c.len() > 4096) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let family = if let Some(conversation) = &query.conversation_id {
        match claude::conversation_family(&state, conversation).await {
            Ok(family)
                if query
                    .agent_family
                    .is_none_or(|requested| requested == family) =>
            {
                family
            }
            _ => {
                return (
                    StatusCode::CONFLICT,
                    "This conversation belongs to a different agent family.",
                )
                    .into_response()
            }
        }
    } else {
        query.agent_family.unwrap_or_default()
    };
    if !family.provider().capabilities.connected_apps {
        return (
            StatusCode::CONFLICT,
            format!(
                "{} does not list connected apps.",
                family.provider().display_name
            ),
        )
            .into_response();
    }
    let project_conversation = if let Some(conversation) = &query.conversation_id {
        match state.store.project_conversation(conversation).await {
            Ok(project) => project,
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    } else {
        None
    };
    let thread = if let Some(project) = &project_conversation {
        match project_thread(&state, project).await {
            Ok(thread) => Some(thread),
            Err(response) => return *response,
        }
    } else if let Some(conversation) = &query.conversation_id {
        // Preserve the conversation scope rather than substituting host scope.
        match state.store.conversation_thread(conversation).await {
            Ok(Some(thread)) => Some(thread),
            _ => {
                return (
                    StatusCode::CONFLICT,
                    "Start this conversation before checking its app access.",
                )
                    .into_response()
            }
        }
    } else {
        None
    };
    let rpc = match (
        project_conversation.is_some(),
        query.conversation_id.is_some(),
        family,
    ) {
        (true, _, _) | (false, false, AgentFamily::Codex) => {
            // Only the computer's current provider home can expose app access.
            let runtime = match &project_conversation {
                Some(project) => {
                    crate::providers::rpc_for_store(&state, family, &project.provider_store).await
                }
                None => projects::rpc_for(&state, family).await,
            };
            match runtime {
                Ok(rpc) => rpc,
                Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
            }
        }
        _ => match claude::client(&state, family) {
            Ok(client) => client.lock().await.rpc(),
            Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
        },
    };
    let result = rpc
        .request(
            "app/installed",
            json!({"threadId":thread,"forceRefresh":query.refresh}),
        )
        .await;
    // Effective app access requires a loaded provider thread. Keep an unloaded
    // Project distinct from authentication/transport failure; never resume it
    // or substitute the account-wide app permissions as conversation authority.
    if project_conversation.is_some()
        && family == AgentFamily::Codex
        && result
            .as_ref()
            .ok()
            .and_then(|response| response.error.as_ref())
            .is_some_and(|error| {
                error.code == -32600
                    && thread
                        .as_ref()
                        .is_some_and(|id| error.message == format!("thread not found: {id}"))
            })
    {
        return (
            StatusCode::FAILED_DEPENDENCY,
            "This conversation’s app access isn’t available right now. Check account-wide apps in Settings.",
        ).into_response();
    }
    if result
        .as_ref()
        .ok()
        .and_then(|response| response.error.as_ref())
        .is_some_and(|error| error.code == -32601)
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "This provider does not offer Connected Apps in Cupola yet.",
        )
            .into_response();
    }
    let installed = match result {
        Ok(response) if response.error.is_none() => response.result,
        _ => None,
    };
    let Some(mut runtime) = installed.and_then(|v| v["apps"].as_array().cloned()) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "App access could not be verified. Check your agent’s sign-in on your computer, then refresh.",
        )
            .into_response();
    };
    runtime.retain(|app| {
        app["id"]
            .as_str()
            .is_some_and(|id| !INTERNAL_APPS.contains(&id))
    });
    runtime.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let fingerprint = hex::encode(Sha256::digest(
        serde_json::to_vec(&runtime).unwrap_or_default(),
    ));
    let Some(offset) = page_offset(query.cursor.as_deref(), &fingerprint, runtime.len()) else {
        return (
            StatusCode::CONFLICT,
            "App access changed. Refresh to load the current list.",
        )
            .into_response();
    };
    let page = runtime.iter().skip(offset).take(50).collect::<Vec<_>>();
    let ids = page
        .iter()
        .filter_map(|app| app["id"].as_str())
        .collect::<Vec<_>>();
    let metadata = if ids.is_empty() {
        Some(json!({"apps":[]}))
    } else {
        match rpc
            .request(
                "app/read",
                json!({"appIds":ids,"threadId":thread,"includeTools":false}),
            )
            .await
        {
            Ok(response) if response.error.is_none() => response.result,
            _ => None,
        }
    };
    let details = page
        .iter()
        .filter_map(|installed| {
            let id = installed["id"].as_str()?;
            let detail = metadata
                .as_ref()
                .and_then(|v| v["apps"].as_array())
                .and_then(|apps| apps.iter().find(|app| app["id"] == id))
                .cloned();
            Some(detail.unwrap_or_else(|| json!({"id":id,"name":installed["runtimeName"].as_str().unwrap_or("Connected app")})))
        })
        .collect::<Vec<_>>();
    let accessible = account_access(&rpc, thread.as_deref(), &page, &details).await;
    let apps = details
        .into_iter()
        .filter_map(|mut detail| {
            if let Some(&access) = detail["id"].as_str().and_then(|id| accessible.get(id)) {
                detail["isAccessible"] = Value::Bool(access);
            }
            project(&detail, Some(&runtime))
        })
        .collect::<Vec<_>>();
    let next_cursor = (offset + page.len() < runtime.len())
        .then(|| format!("{fingerprint}:{}", offset + page.len()));
    ([(header::CACHE_CONTROL,"no-store")],Json(json!({
        "agentFamily":family,"hostInstallationId":state.host_installation_id,"conversationId":query.conversation_id,
        "apps":apps,"nextCursor":next_cursor,"checkedAtMs":now_ms(),
        "warning":if metadata.is_none(){Some("Some app details could not be loaded. Access status is current.")}else{None}
    }))).into_response()
}

// Codex's app metadata omits whether the account is linked; its directory
// says so. Read it only for enabled apps that can't run, and leave them
// "unavailable" when the directory can't answer rather than guessing.
async fn account_access(
    rpc: &wonder_app_server::RpcClient,
    thread: Option<&str>,
    page: &[&Value],
    details: &[Value],
) -> std::collections::HashMap<String, bool> {
    let mut wanted = page
        .iter()
        .filter(|item| item["enabled"] == true && item["callable"] == false)
        .filter_map(|item| item["id"].as_str())
        .filter(|id| {
            !details
                .iter()
                .any(|detail| detail["id"] == *id && detail["isAccessible"].is_boolean())
        })
        .map(str::to_owned)
        .collect::<std::collections::HashSet<_>>();
    let mut found = std::collections::HashMap::new();
    let mut cursor: Option<String> = None;
    for _ in 0..5 {
        if wanted.is_empty() {
            break;
        }
        let Ok(response) = rpc
            .request(
                "app/list",
                json!({"limit":100,"cursor":cursor,"threadId":thread}),
            )
            .await
        else {
            break;
        };
        let Some(result) = response.result.filter(|_| response.error.is_none()) else {
            break;
        };
        for app in result["data"].as_array().into_iter().flatten() {
            if let (Some(id), Some(access)) = (app["id"].as_str(), app["isAccessible"].as_bool()) {
                if wanted.remove(id) {
                    found.insert(id.to_owned(), access);
                }
            }
        }
        cursor = result["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    found
}

async fn project_thread(
    state: &AppState,
    project: &wonder_store::StoredProjectConversation,
) -> Result<String, Box<Response>> {
    if !crate::providers::is_default_store(state, project.family, &project.provider_store) {
        return Err(Box::new(
            (
                StatusCode::CONFLICT,
                "This Project belongs to another provider history on your computer.",
            )
                .into_response(),
        ));
    }
    let Some(native_session) = project.native_session_id.as_deref() else {
        return Err(Box::new(
            (
                StatusCode::CONFLICT,
                "Start this Project chat before checking its app access.",
            )
                .into_response(),
        ));
    };
    // Claude's native session ID differs from its bridge thread ID. The
    // durable binding is the exact transport identity for both families.
    let binding = state.store.runtime_binding(&project.conversation_id).await;
    let Ok(Some(binding)) = binding else {
        return Err(Box::new(
            (
                StatusCode::CONFLICT,
                "This Project's app access changed. Reopen the chat and try again.",
            )
                .into_response(),
        ));
    };
    let matches_native =
        crate::providers::binds_native_session(project.family, native_session, &binding);
    if binding.family != project.family
        || binding.execution_scope != wonder_store::EXECUTION_SCOPE_PROJECTS
        || !matches_native
    {
        return Err(Box::new(
            (
                StatusCode::CONFLICT,
                "This Project's app access changed. Reopen the chat and try again.",
            )
                .into_response(),
        ));
    }
    Ok(binding.thread_id)
}

fn page_offset(cursor: Option<&str>, fingerprint: &str, length: usize) -> Option<usize> {
    let Some(cursor) = cursor else {
        return Some(0);
    };
    let (hash, offset) = cursor.split_once(':')?;
    let offset = offset.parse::<usize>().ok()?;
    (hash == fingerprint && offset <= length).then_some(offset)
}

fn project(app: &Value, runtime: Option<&Vec<Value>>) -> Option<Value> {
    let id = app["id"].as_str()?;
    if INTERNAL_APPS.contains(&id) {
        return None;
    }
    let name = app["name"].as_str()?;
    let installed = runtime.and_then(|apps| apps.iter().find(|item| item["id"] == id));
    let status = match installed {
        Some(item) if item["enabled"] == false => "disabled",
        Some(item) if item["enabled"] == true && item["callable"] == true => "available",
        // Installed but its account isn't linked: the setup link can fix it.
        Some(item) if item["callable"] == false && app["isAccessible"] == false => "not_connected",
        Some(item) if item["callable"] == false => "unavailable",
        Some(_) => "unknown",
        None if runtime.is_none() => "unknown",
        None if app["isEnabled"] == false => "disabled",
        None if app["isAccessible"] == false => "not_connected",
        None => "unknown",
    };
    Some(
        json!({"id":id,"name":name,"description":app["description"].as_str(),"status":status,
        "logoUrl":safe_url(app.get("logoUrl").filter(|v|v.is_string()).unwrap_or(&app["iconUrl"]),false),"logoUrlDark":safe_url(app.get("logoUrlDark").filter(|v|v.is_string()).unwrap_or(&app["iconUrlDark"]),false),
        "setupUrl":safe_url(&app["installUrl"],true),"accountName":Value::Null}),
    )
}

// Only vendor-owned metadata destinations; no loopback callbacks or credentials.
fn safe_url(value: &Value, setup: bool) -> Option<String> {
    let value = value.as_str()?;
    if value.len() > 2048 {
        return None;
    }
    let uri: axum::http::Uri = value.parse().ok()?;
    let authority = uri.authority()?;
    if uri.scheme_str() != Some("https")
        || authority.as_str().contains('@')
        || authority.port().is_some()
    {
        return None;
    }
    let host = authority.host();
    let trusted = if setup {
        (host == "chatgpt.com" && uri.path().starts_with("/apps/"))
            || (host == "claude.ai" && uri.path() == "/settings/connectors")
    } else {
        [
            "openai.com",
            "oaistatic.com",
            "oaiusercontent.com",
            "chatgpt.com",
        ]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
    };
    (trusted && uri.query().is_none() && !value.contains('#')).then(|| value.to_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use std::{fs, os::unix::fs::PermissionsExt};
    use wonder_store::{ProjectConversationInsert, ProjectRootInput};

    #[tokio::test]
    async fn providers_without_connected_apps_are_refused_before_any_runtime_read() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let response = list(
            State(state),
            Extension(OwnerAuthority),
            Query(AppQuery {
                cursor: None,
                agent_family: Some(AgentFamily::OpenCode),
                conversation_id: None,
                refresh: false,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), 10_000).await.unwrap();
        assert_eq!(&body[..], b"OpenCode does not list connected apps.");
    }

    #[tokio::test]
    async fn account_and_app_reads_use_owner_runtime_and_exact_project_thread() {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        fs::write(
            dir.path().join("usage-fixture.json"),
            r#"{"rateLimits":{"primary":{"usedPercent":20,"windowDurationMins":300}}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("apps-fixture.json"),
            r#"{"apps":[{"id":"app-fixture","enabled":true,"callable":true}]}"#,
        )
        .unwrap();
        let original = fs::read_to_string(dir.path().join("codex")).unwrap();
        let owner_bin = dir.path().join("codex-owner");
        fs::write(
            &owner_bin,
            original.replacen(
                "#!/bin/sh\n",
                "#!/bin/sh\nexport WONDER_FIXTURE_SCOPE=owner\n",
                1,
            ),
        )
        .unwrap();
        fs::set_permissions(&owner_bin, fs::Permissions::from_mode(0o755)).unwrap();
        state.projects = crate::projects::ProjectRuntime::configured(
            owner_bin,
            "test".into(),
            &dir.path().join("owner-home"),
            &dir.path().join("claude-home"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        let source = dir.path().join("project-source");
        fs::create_dir(&source).unwrap();
        let root = source.to_string_lossy().into_owned();
        state
            .store
            .create_project(
                "project",
                "request",
                "hash",
                "Project",
                &[ProjectRootInput {
                    path: root.clone(),
                    canonical_path: root.clone(),
                }],
                0,
                "now",
            )
            .await
            .unwrap();
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "project-chat",
                project_id: "project",
                family: AgentFamily::Codex,
                provider_store: &state.projects.codex_store,
                native_session_id: Some("project-thread"),
                cwd: &root,
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

        let usage = crate::account_usage::read(
            State(state.clone()),
            Extension(OwnerAuthority),
            Query(crate::account_usage::UsageQuery::default()),
        )
        .await;
        assert_eq!(usage.status(), StatusCode::OK);
        let body = to_bytes(usage.into_body(), 100_000).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["windows"][0]["remainingPercent"], 80.0);

        for (conversation, expected) in [(None, None), (Some("project-chat"), Some("project-chat"))]
        {
            let response = list(
                State(state.clone()),
                Extension(OwnerAuthority),
                Query(AppQuery {
                    cursor: None,
                    agent_family: Some(AgentFamily::Codex),
                    conversation_id: conversation.map(str::to_owned),
                    refresh: false,
                }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 100_000).await.unwrap();
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["conversationId"].as_str(), expected);
            assert_eq!(value["apps"][0]["name"], "Fixture app");
        }
        let log = fs::read_to_string(dir.path().join("routing-requests-jsonl")).unwrap();
        let requests = log
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let installed = requests
            .iter()
            .filter(|r| r["method"] == "app/installed")
            .collect::<Vec<_>>();
        assert_eq!(installed.len(), 2);
        assert_eq!(installed[0]["scope"], "owner");
        assert!(installed[0]["params"]["threadId"].is_null());
        assert_eq!(installed[1]["scope"], "owner");
        assert_eq!(installed[1]["params"]["threadId"], "project-thread");
        assert!(requests
            .iter()
            .any(|r| r["scope"] == "owner" && r["method"] == "account/rateLimits/read"));

        // Observed 0.160.0 unloaded-thread error: only the exact Project thread
        // receives the specific unavailable state. Other failures retain their
        // real recovery class, including global-account requests.
        for (code, message, conversation, expected) in [
            (
                -32600,
                "thread not found: project-thread",
                Some("project-chat"),
                StatusCode::FAILED_DEPENDENCY,
            ),
            (
                -32600,
                "thread not found: other-thread",
                Some("project-chat"),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                -32600,
                "invalid request",
                Some("project-chat"),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                -32601,
                "Method not found",
                Some("project-chat"),
                StatusCode::NOT_IMPLEMENTED,
            ),
            (
                -32600,
                "thread not found: project-thread",
                None,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            fs::write(
                dir.path().join("apps-error.json"),
                json!({"code":code,"message":message}).to_string(),
            )
            .unwrap();
            let response = list(
                State(state.clone()),
                Extension(OwnerAuthority),
                Query(AppQuery {
                    cursor: None,
                    agent_family: Some(AgentFamily::Codex),
                    conversation_id: conversation.map(str::to_owned),
                    refresh: false,
                }),
            )
            .await;
            assert_eq!(response.status(), expected);
        }
        fs::remove_file(dir.path().join("apps-error.json")).unwrap();
        let requests = fs::read_to_string(dir.path().join("routing-requests-jsonl")).unwrap();
        assert!(requests
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .all(|request| !matches!(
                request["method"].as_str(),
                Some("thread/resume" | "turn/start")
            )));

        // A provider read failure is retryable; 409 is reserved for a stale
        // conversation binding or changed pagination snapshot.
        fs::write(
            dir.path().join("apps-fixture.json"),
            r#"{"unexpected":true}"#,
        )
        .unwrap();
        let response = list(
            State(state.clone()),
            Extension(OwnerAuthority),
            Query(AppQuery {
                cursor: None,
                agent_family: Some(AgentFamily::Codex),
                conversation_id: Some("project-chat".into()),
                refresh: true,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        // Optional usage failures never tear down the process or its model
        // catalog. The native Windows runtime returns -32603 for revoked auth.
        fs::write(
            dir.path().join("models.json"),
            json!({"data":[{"id":"fixture-codex","model":"fixture-codex","displayName":"Fixture Codex"}]}).to_string(),
        ).unwrap();
        for (error, expected) in [
            (
                json!({"code":-32601,"message":"Method not found"}),
                StatusCode::NOT_IMPLEMENTED,
            ),
            (
                json!({"code":-32603,"message":"401 Unauthorized: token_revoked"}),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            fs::write(dir.path().join("unsupported-usage"), error.to_string()).unwrap();
            state.projects.shutdown().await;
            let response = crate::account_usage::read(
                State(state.clone()),
                Extension(OwnerAuthority),
                Query(crate::account_usage::UsageQuery::default()),
            )
            .await;
            assert_eq!(response.status(), expected);
            let rpc = crate::projects::rpc_for(&state, AgentFamily::Codex)
                .await
                .unwrap();
            let models = rpc.request("model/list", json!({})).await.unwrap();
            assert!(models.error.is_none());
            assert!(!models.result.unwrap()["data"]
                .as_array()
                .unwrap()
                .is_empty());
            assert!(state
                .runtime_catalog
                .read()
                .await
                .models
                .iter()
                .any(|model| model.id == "fixture-codex"));
        }
    }

    #[tokio::test]
    async fn claude_project_app_scope_uses_bound_thread_not_native_session() {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        state.projects = crate::projects::ProjectRuntime::configured(
            dir.path().join("codex"),
            "test".into(),
            &dir.path().join("codex-home"),
            &dir.path().join("claude-home"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        let root = source.to_string_lossy().into_owned();
        state
            .store
            .create_project(
                "project",
                "request",
                "hash",
                "Project",
                &[ProjectRootInput {
                    path: root.clone(),
                    canonical_path: root.clone(),
                }],
                0,
                "now",
            )
            .await
            .unwrap();
        state
            .store
            .create_project_conversation(ProjectConversationInsert {
                conversation_id: "claude-chat",
                project_id: "project",
                family: AgentFamily::Claude,
                provider_store: &state.projects.claude_store,
                native_session_id: Some("session-claude"),
                cwd: &root,
                roots_revision: 1,
                title: "Claude chat",
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
            .bind_project_runtime(
                "claude-chat",
                AgentFamily::Claude,
                &state.projects.claude_store,
                "claude-project-thread",
                Some("session-claude"),
                "now",
            )
            .await
            .unwrap();
        let project = state
            .store
            .project_conversation("claude-chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            project_thread(&state, &project).await.unwrap(),
            "claude-project-thread"
        );
        let mut stale = project.clone();
        stale.native_session_id = Some("another-session".into());
        assert_eq!(
            project_thread(&state, &stale).await.unwrap_err().status(),
            StatusCode::CONFLICT
        );
    }
    #[tokio::test]
    async fn an_app_whose_account_isnt_linked_is_not_connected_with_its_setup_link() {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        state.projects = crate::projects::ProjectRuntime::configured(
            dir.path().join("codex"),
            "test".into(),
            &dir.path().join("codex-home"),
            &dir.path().join("claude-home"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        fs::write(
            dir.path().join("apps-fixture.json"),
            json!({"apps":[
                {"id":"calendar","enabled":true,"callable":true},
                {"id":"drive","enabled":true,"callable":false},
                {"id":"linear","enabled":true,"callable":false},
                {"id":"notion","enabled":true,"callable":false},
            ]})
            .to_string(),
        )
        .unwrap();
        let read = ["calendar", "drive", "linear", "notion"].map(
            |id| json!({"id":id,"name":id,"installUrl":format!("https://chatgpt.com/apps/{id}")}),
        );
        fs::write(
            dir.path().join("apps-read.json"),
            json!({ "apps": read }).to_string(),
        )
        .unwrap();
        // The directory answers across pages; Notion isn't in it.
        fs::write(
            dir.path().join("apps-list.json"),
            json!([
                [{"id":"calendar","name":"calendar","isAccessible":true},
                 {"id":"drive","name":"drive","isAccessible":false}],
                [{"id":"linear","name":"linear","isAccessible":true}],
            ])
            .to_string(),
        )
        .unwrap();
        let response = list(
            State(state.clone()),
            Extension(OwnerAuthority),
            Query(AppQuery {
                cursor: None,
                agent_family: Some(AgentFamily::Codex),
                conversation_id: None,
                refresh: true,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 100_000).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        let rows = value["apps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|app| (app["id"].as_str().unwrap(), app["status"].as_str().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                ("calendar", "available"),
                ("drive", "not_connected"),
                ("linear", "unavailable"),
                ("notion", "unavailable"),
            ]
        );
        assert_eq!(
            value["apps"][1]["setupUrl"],
            "https://chatgpt.com/apps/drive"
        );
        let log = fs::read_to_string(dir.path().join("routing-requests-jsonl")).unwrap();
        let lists = log
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|r| r["method"] == "app/list")
            .count();
        assert_eq!(lists, 2);

        // Every app either runs or is off: the directory isn't read.
        fs::write(
            dir.path().join("apps-fixture.json"),
            r#"{"apps":[{"id":"calendar","enabled":true,"callable":true},{"id":"drive","enabled":false,"callable":false}]}"#,
        )
        .unwrap();
        let response = list(
            State(state.clone()),
            Extension(OwnerAuthority),
            Query(AppQuery {
                cursor: None,
                agent_family: Some(AgentFamily::Codex),
                conversation_id: None,
                refresh: false,
            }),
        )
        .await;
        let body = to_bytes(response.into_body(), 100_000).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["apps"][1]["status"], "disabled");
        let log = fs::read_to_string(dir.path().join("routing-requests-jsonl")).unwrap();
        assert_eq!(log.matches("\"app/list\"").count(), 2);
        state.projects.shutdown().await;
    }
    #[test]
    fn pagination_rejects_a_changed_runtime_snapshot() {
        assert_eq!(page_offset(None, "current", 120), Some(0));
        assert_eq!(page_offset(Some("current:50"), "current", 120), Some(50));
        assert_eq!(page_offset(Some("old:50"), "current", 120), None);
        assert_eq!(page_offset(Some("current:121"), "current", 120), None);
        assert_eq!(page_offset(Some("current:-1"), "current", 120), None);
    }
    #[test]
    fn directory_access_never_implies_callability() {
        assert!(project(
            &json!({"id":"connector_openai_hotline","name":"Internal"}),
            None
        )
        .is_none());
        let app = json!({"id":"a","name":"Calendar","isEnabled":true,"isAccessible":true});
        assert_eq!(project(&app, None).unwrap()["status"], "unknown");
        assert_eq!(project(&app, Some(&vec![])).unwrap()["status"], "unknown");
        assert_eq!(
            project(
                &app,
                Some(&vec![json!({"id":"a","enabled":true,"callable":true})])
            )
            .unwrap()["status"],
            "available"
        );
        assert_eq!(
            project(
                &app,
                Some(&vec![json!({"id":"a","enabled":false,"callable":true})])
            )
            .unwrap()["status"],
            "disabled"
        );
        // Enabled but not runnable: only an unlinked account is "not connected".
        let unlinked = json!({"id":"a","name":"Calendar","isAccessible":false});
        let blocked = vec![json!({"id":"a","enabled":true,"callable":false})];
        assert_eq!(
            project(&unlinked, Some(&blocked)).unwrap()["status"],
            "not_connected"
        );
        assert_eq!(
            project(&app, Some(&blocked)).unwrap()["status"],
            "unavailable"
        );
        let unknown = json!({"id":"a","name":"Calendar"});
        assert_eq!(
            project(&unknown, Some(&blocked)).unwrap()["status"],
            "unavailable"
        );
        let off = vec![json!({"id":"a","enabled":false,"callable":false})];
        assert_eq!(
            project(&unlinked, Some(&off)).unwrap()["status"],
            "disabled"
        );
    }
    #[test]
    fn metadata_links_cannot_become_arbitrary_setup_or_private_fetches() {
        for url in [
            "http://chatgpt.com/apps/a",
            "https://chatgpt.com.evil.test/apps/a",
            "https://user@chatgpt.com/apps/a",
            "https://127.0.0.1/apps/a",
            "https://chatgpt.com/apps/a?token=secret",
            "https://chatgpt.com:444/apps/a",
            "https://claude.ai/settings/connectors?token=secret",
            "https://claude.ai.evil.test/settings/connectors",
            "https://claude.ai/settings/billing",
        ] {
            assert!(safe_url(&json!(url), true).is_none());
        }
        assert!(safe_url(&json!("https://chatgpt.com/apps/calendar"), true).is_some());
        assert!(safe_url(&json!("https://claude.ai/settings/connectors"), true).is_some());
        assert!(safe_url(&json!("https://cdn.oaistatic.com/icon.png"), false).is_some());
        assert!(safe_url(&json!("https://unknown.test/icon.png"), false).is_none());
    }
}
