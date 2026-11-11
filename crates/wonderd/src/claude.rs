//! Claude's owned process and provider routing. Session bindings, never model
//! output or a guessed thread prefix, decide which transport receives a request.
use crate::{ingestion, AppState, RuntimeCatalog};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use wonder_app_server::{AppServerClient, BridgeLaunchConfig, RpcClient};
use wonder_store::{AgentFamily, Store};

/// Internal acknowledgement of a native reset, consumed under dispatch_lock.
/// No client command endpoint: only the exact registered Claude process can
/// rotate its existing binding. Replaying a lost ACK does not reset twice.
pub(crate) async fn native_session_reset<'a>(
    state: &'a AppState,
    frame: &serde_json::Value,
    params: &serde_json::Value,
    dispatch: &mut Option<tokio::sync::MutexGuard<'a, ()>>,
) -> bool {
    let Some(runtime_id) = params["_wonderRuntimeId"].as_str() else {
        return true;
    };
    let Some(route) = state.ingestion.runtime_route(runtime_id) else {
        return true;
    };
    if route.family != AgentFamily::Claude {
        return true;
    }
    let (Some(id), Some(thread), Some(old), Some(new)) = (
        frame.get("id"),
        params["threadId"].as_str(),
        params["oldSessionId"].as_str(),
        params["sessionId"].as_str(),
    ) else {
        return true;
    };
    let binding = match crate::projection_binding(state, params).await {
        Ok(binding) => binding,
        Err(_) => return false,
    };
    let relock = dispatch.is_some();
    drop(dispatch.take());
    let Ok(rpc) = route.rpc().await else {
        return true;
    };
    let Ok(owner) = for_thread(state, thread).await else {
        return true;
    };
    if owner.health().id() != runtime_id {
        return true;
    }
    let retained: Vec<String> = match params.get("retainedTurnIds") {
        None => Vec::new(),
        Some(value) => match serde_json::from_value(value.clone()) {
            Ok(turns) => turns,
            Err(_) => return true,
        },
    };
    if relock {
        *dispatch = Some(state.dispatch_lock.lock().await);
        if crate::projection_binding(state, params).await.ok().as_ref() != Some(&binding) {
            return false;
        }
    }
    let result = match state
        .store
        .reset_project_native_session(thread, old, new, &retained, &crate::now_ms().to_string())
        .await
    {
        Ok((_, revision)) => json!({"sessionId":new,"historyRevision":revision}),
        Err(_) => return false, // Durable inbox retries; bridge retains pending intent.
    };
    drop(dispatch.take());
    let _ = rpc.respond_value(id.clone(), Some(result), None).await;
    true
}

// ACP cancel is only a notification. Its bridge waits for the exact prompt
// outcome; normal terminal ingestion owns the receipt and timeline state.
// A reply can finish naturally while Stop is on its way.
pub(crate) fn confirmed_acp_stop(
    response: &wonder_app_server::RpcResponse,
    turn_id: &str,
) -> Result<(), String> {
    if let Some(error) = &response.error {
        return Err(crate::sanitize_ansi_and_secrets(&error.message, 2000));
    }
    let turn = response.result.as_ref().and_then(|value| value.get("turn"));
    if turn
        .and_then(|value| value.get("id"))
        .and_then(serde_json::Value::as_str)
        == Some(turn_id)
        && matches!(
            turn.and_then(|value| value.get("status"))
                .and_then(serde_json::Value::as_str),
            Some("completed" | "interrupted" | "failed")
        )
    {
        Ok(())
    } else {
        Err("The provider has not confirmed that this response stopped.".into())
    }
}

pub struct Runtime {
    pub client: Arc<Mutex<AppServerClient>>,
    pub config: BridgeLaunchConfig,
}
impl Runtime {
    pub fn configured(data: &Path, store: &Store) -> Option<Arc<Self>> {
        Self::bridge(
            data,
            store,
            "WONDER_CLAUDE_BRIDGE",
            "claude-runtime",
            AgentFamily::Claude,
        )
    }

    /// The bundled experimental ACP bridge discovers the owner's OpenCode CLI.
    /// As with Codex, installed providers appear without a separate feature flag.
    pub fn opencode(data: &Path, store: &Store) -> Option<Arc<Self>> {
        Self::bridge(
            data,
            store,
            "WONDER_OPENCODE_BRIDGE",
            "acp-runtime",
            AgentFamily::OpenCode,
        )
    }

    pub fn cursor(data: &Path, store: &Store) -> Option<Arc<Self>> {
        Self::bridge(
            data,
            store,
            "WONDER_CURSOR_BRIDGE",
            "acp-runtime",
            AgentFamily::Cursor,
        )
    }

    fn bridge(
        data: &Path,
        store: &Store,
        override_variable: &str,
        directory: &str,
        family: AgentFamily,
    ) -> Option<Arc<Self>> {
        let resources = std::env::current_exe()
            .ok()?
            .parent()?
            .parent()?
            .join("Resources");
        let entrypoint = std::env::var_os(override_variable)
            .map(PathBuf::from)
            .unwrap_or_else(|| resources.join(directory).join("main.mjs"));
        if !entrypoint.is_file() {
            return None;
        }
        let node_bin = std::env::var_os("WONDER_NODE_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| resources.join("node/bin/node"));
        // Only Claude's runtime stages SDK updates with npm.
        let npm_cli = (family == AgentFamily::Claude)
            .then(|| {
                std::env::var_os("WONDER_NPM_CLI")
                    .map(PathBuf::from)
                    .or_else(|| {
                        Some(resources.join("node/lib/node_modules/npm/bin/npm-cli.js"))
                            .filter(|p| p.is_file())
                    })
            })
            .flatten();
        Some(Arc::new(Self {
            client: Arc::new(Mutex::new(AppServerClient::unavailable(
                env!("CARGO_PKG_VERSION").into(),
                ingestion::notification_sink(store.clone()),
            ))),
            config: BridgeLaunchConfig {
                node_bin,
                entrypoint,
                npm_cli,
                claude_config_dir: None,
                state_dir: data.join(if family == AgentFamily::Cursor {
                    "cursor-runtime"
                } else {
                    directory
                }),
                wonder_version: env!("CARGO_PKG_VERSION").into(),
                family: family.as_str(),
            },
        }))
    }
}

/// The one thing Cupola says when its bundled Claude runtime is missing,
/// wherever that is noticed: it ships inside Cupola, so the fix is the same.
pub(crate) const RUNTIME_MISSING: &str =
    "Cupola’s Claude runtime is missing. Reinstall Cupola on your computer.";

/// The bridge runtime serving `family`, when it is configured.
pub(crate) fn bridge_runtime(state: &AppState, family: AgentFamily) -> Option<&Arc<Runtime>> {
    match family {
        AgentFamily::Codex => None,
        AgentFamily::Claude => state.claude.as_ref(),
        AgentFamily::OpenCode => state.opencode.as_ref(),
        AgentFamily::Cursor => state.cursor.as_ref(),
    }
}

/// Whether Wonder can run `family` on this Mac right now.
pub(crate) fn available(state: &AppState, family: AgentFamily) -> bool {
    family == AgentFamily::Codex || bridge_runtime(state, family).is_some()
}

pub(crate) fn client(
    state: &AppState,
    family: AgentFamily,
) -> Result<Arc<Mutex<AppServerClient>>, String> {
    match family {
        AgentFamily::Codex => Ok(state.projects.codex.clone()),
        family => bridge_runtime(state, family)
            .map(|r| r.client.clone())
            .ok_or_else(|| match family {
                AgentFamily::Claude => RUNTIME_MISSING.into(),
                _ => format!(
                    "Cupola’s {} runtime is missing. Update Cupola on your computer.",
                    family.provider().display_name
                ),
            }),
    }
}

pub(crate) async fn for_thread(state: &AppState, thread: &str) -> Result<RpcClient, String> {
    let (family, scope, store) = state
        .store
        .runtime_route_for_thread(thread)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("The conversation has no runtime binding. Reopen it and try again.")?;
    // Use the provider that owns this Project thread and its descendants, in
    // the computer's own home. Legacy stores are refused.
    if scope == wonder_store::EXECUTION_SCOPE_PROJECTS {
        return match store {
            Some(store) => crate::providers::rpc_for_store(state, family, &store).await,
            None => crate::projects::rpc_for(state, family).await,
        };
    }
    Err("The conversation runtime binding is unavailable.".into())
}

pub(crate) async fn conversation_family(state: &AppState, id: &str) -> Result<AgentFamily, String> {
    if let Some(project) = state
        .store
        .project_conversation(id)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(project.family);
    }
    if let Some(binding) = state
        .store
        .runtime_binding(id)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(binding.family);
    }
    Err("The conversation could not be found.".into())
}

/// Why OpenCode or Cursor cannot take new work although its runtime answered.
/// Restarting the runtime fixes none of these, so the runtime stays up and
/// the provider is checked again later, or at once on Reconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetupIssue {
    NotInstalled,
    SignedOut,
    NoModels,
    /// The check itself failed (a model listing that did not finish).
    CheckFailed,
}

impl SetupIssue {
    fn reported(availability: &serde_json::Value) -> Self {
        match availability.as_str() {
            Some("notInstalled") => Self::NotInstalled,
            Some("signedOut") => Self::SignedOut,
            Some("noModels") => Self::NoModels,
            _ => Self::CheckFailed,
        }
    }

    /// What the owner can do about it, wherever the provider is refused.
    pub(crate) fn detail(self, family: AgentFamily) -> &'static str {
        match (family, self) {
            (AgentFamily::OpenCode, Self::NotInstalled) => {
                "Install OpenCode’s command-line tool on your computer, then reconnect OpenCode."
            }
            (AgentFamily::OpenCode, Self::SignedOut | Self::NoModels) => {
                "OpenCode has no models set up. Add a model provider in OpenCode on your computer."
            }
            (AgentFamily::OpenCode, Self::CheckFailed) => {
                "OpenCode couldn’t list its models just now. Cupola will try again shortly."
            }
            (AgentFamily::Cursor, Self::NotInstalled) => {
                "Install Cursor’s command-line tool (agent) on your computer, then reconnect Cursor."
            }
            (AgentFamily::Cursor, Self::SignedOut) => {
                "Sign in to Cursor on your computer (run agent login), then reconnect Cursor."
            }
            (AgentFamily::Cursor, Self::NoModels) => {
                "Cursor listed no models. Check your Cursor account on your computer."
            }
            (AgentFamily::Cursor, Self::CheckFailed) => {
                "Cursor couldn’t be checked just now. Cupola will try again shortly."
            }
            _ => "This agent isn’t set up on your computer yet.",
        }
    }
}

/// A discovery that did not produce a catalog.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryFailure {
    /// The runtime answered: the provider is not usable yet.
    NotSetUp {
        issue: SetupIssue,
        /// The runtime's short reason, for the host log only.
        diagnostic: Option<String>,
    },
    /// The runtime answered, but with a catalog or account Cupola cannot
    /// use. Its transport is healthy: it is kept, unvalidated, and checked
    /// again rather than restarted.
    Runtime(String),
    /// The runtime did not answer; recovery restarts it.
    Unreachable(String),
}

impl DiscoveryFailure {
    pub(crate) fn message(&self, family: AgentFamily) -> String {
        match self {
            Self::NotSetUp { issue, .. } => issue.detail(family).into(),
            Self::Runtime(message) | Self::Unreachable(message) => message.clone(),
        }
    }
}

/// Reads a bridge provider's account and models into the shared catalog.
#[cfg(test)]
pub(crate) async fn discover_family(
    state: &AppState,
    runtime: &mut AppServerClient,
    family: AgentFamily,
) -> Result<(), String> {
    discover(state, runtime, family)
        .await
        .map_err(|failure| failure.message(family))
}

/// As `discover_family`, telling a provider that is not set up from a
/// runtime that failed. Claude keeps its own sign-in path: every Claude
/// failure is a runtime failure.
pub(crate) async fn discover(
    state: &AppState,
    runtime: &mut AppServerClient,
    family: AgentFamily,
) -> Result<(), DiscoveryFailure> {
    discover_with(state, &runtime.rpc(), family, true).await
}

/// As `discover`. Without `refresh` the runtime answers from its own catalog
/// while that is fresh and nothing it depends on changed, so a periodic
/// check starts no provider process; `refresh` makes it check now. Capture the
/// generation-bound RPC handle before calling; discovery must not hold the
/// runtime lifecycle lock while a send tries to reach the bridge.
pub(crate) async fn discover_with(
    state: &AppState,
    runtime: &RpcClient,
    family: AgentFamily,
    refresh: bool,
) -> Result<(), DiscoveryFailure> {
    let name = family.provider().display_name;
    // Only the ACP runtime reports `availability`; Claude's has its own path.
    let acp = family != AgentFamily::Claude;
    let not_set_up = |issue, diagnostic| DiscoveryFailure::NotSetUp { issue, diagnostic };
    let account = runtime
        .request("account/read", json!({ "refresh": refresh }))
        .await
        .map_err(|e| DiscoveryFailure::Unreachable(e.to_string()))?;
    let Some(account) = account.result else {
        return Err(match family {
            AgentFamily::Claude => DiscoveryFailure::Runtime(
                "Claude could not check your subscription. Sign in on your computer.".into(),
            ),
            _ => not_set_up(
                SetupIssue::CheckFailed,
                account.error.map(|e| e.message.chars().take(200).collect()),
            ),
        });
    };
    if account["connected"] != true {
        return Err(if acp {
            not_set_up(
                SetupIssue::reported(&account["availability"]),
                account["diagnostic"]
                    .as_str()
                    .map(|d| d.chars().take(200).collect()),
            )
        } else {
            DiscoveryFailure::Runtime(
                "Sign in to your Claude subscription on your computer.".into(),
            )
        });
    }
    let models = runtime
        .request("model/list", json!({}))
        .await
        .map_err(|e| DiscoveryFailure::Unreachable(e.to_string()))?
        .result;
    let Some(models) = models else {
        return Err(if acp {
            not_set_up(SetupIssue::CheckFailed, None)
        } else {
            DiscoveryFailure::Runtime(format!("{name} models are temporarily unavailable."))
        });
    };
    let mut discovered = RuntimeCatalog::default();
    let refused = discovered.apply_models_page(family, &models);
    if discovered.models.is_empty() || refused > 0 {
        return Err(DiscoveryFailure::Runtime(format!(
            "{name} returned an incompatible model list. Update Cupola on your computer."
        )));
    }
    let mut catalog = state.runtime_catalog.write().await;
    if !runtime.health().is_alive() {
        return Err(DiscoveryFailure::Unreachable(format!(
            "{name} restarted during its model check. Try again."
        )));
    }
    catalog.models.retain(|m| m.agent_family != family);
    catalog.models.extend(discovered.models);
    catalog.resolve_defaults();
    Ok(())
}
