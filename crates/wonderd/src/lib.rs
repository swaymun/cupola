//! Wonder's authenticated loopback API boundary.
// Handlers return axum's ready `Response` as their error (a refusal to send back);
// boxing it would only move the copy, so this lint is allowed crate-wide.
#![allow(clippy::result_large_err)]
/// The version this build reports (host log, `initialize` serverInfo, `cupola
/// version`) and `update` compares: the release's own stamp
/// (`WONDER_RELEASE_VERSION` at build time), else the crate version. Protocol
/// and runtime-compatibility versions are separate.
pub const RELEASE_VERSION: &str = match option_env!("WONDER_RELEASE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

mod automation_schedule;
#[cfg(all(test, unix))]
mod automation_tests;
pub mod canonical_path;
mod diagnostics;
#[cfg(all(feature = "diagnostics-ssh-spike", debug_assertions, unix))]
pub mod ssh_spike;
use automation_schedule::next_automation_run;
mod media_refs;
#[cfg(test)]
mod test_git;
#[cfg(all(test, unix))]
mod test_support;
mod tool_media;

mod history;
mod history_compression;
use history::*;
mod sync;
use sync::stream_events;

mod account_cli;
mod account_usage;
mod agent_defaults;
mod agent_images;
mod agent_settings;
#[cfg(all(test, unix))]
mod agent_settings_tests;
mod approval_lifecycle;
mod artifact_annotations;
mod attachments;
mod chatgpt_approvals;
pub mod claude;
mod claude_app;
pub use claude_app::ClaudeApp;
#[cfg(all(test, unix))]
mod claude_send_now_tests;
mod codex_activity;
mod codex_desktop_ipc;
mod codex_skills;
mod composer_commands;
mod composer_mentions;
mod computer_apps;
pub mod computer_runtime;
pub mod computer_sessions;
mod connected_apps;
pub mod context_usage;
mod context_windows;
pub mod cupola_env;
#[cfg(unix)]
pub mod data_home;
mod default_models;
mod desktop_activity;
mod dev_preview;
pub mod dispatch;
pub mod external_push;
mod filesystem;
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
pub mod linux_desktop;
mod settled_elsewhere;
pub mod triage_redaction;
pub use filesystem::configure_git_reads;
pub use filesystem::{claude_config_home, sensitive_roots, with_firmlink_aliases};
pub use t3_code::T3Code;
mod goals;
mod host_name;
pub mod ingestion;
mod injected_context;
pub mod lan;
pub mod live_activity;
pub mod logging;
pub mod mac_dialogs;
mod mcp_http;
#[cfg(all(test, unix))]
mod mcp_http_tests;
mod mcp_oauth;
mod model_names;
mod native_search;
mod native_settings;
#[cfg(all(test, unix))]
mod phone_approval_tests;
pub(crate) mod platform;
mod preamble;
mod project_subagents;
pub mod projects;
pub mod provider_runtime;
pub mod provider_runtime_config;
#[cfg(all(test, unix))]
mod provider_status_tests;
mod provider_switch;
#[cfg(all(test, unix))]
mod provider_switch_tests;
mod provider_updates;
mod providers;
mod pull_requests;
pub mod push;
mod questions;
mod queue;
pub mod relay;
pub mod rescue;
mod rewind;
#[cfg(all(test, unix))]
mod rewind_tests;
pub mod rpc;
pub mod secrets;
pub mod simulators;
mod subagents;
mod t3_code;
mod task_checklist;
pub mod terminal;
mod thread_tools;
mod transcript_watch;
mod turn_diff;
pub mod update_admission;
mod update_handoff;
mod uploads;
pub mod usage_resume;
pub mod voice;
mod wonder_skills;
#[cfg(all(test, unix))]
mod wonder_skills_tests;
mod worktree_handoff;

use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

use axum::{
    body::Body,
    extract::{ws::WebSocketUpgrade, DefaultBodyLimit, Extension, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use base64::Engine;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(test)]
use tokio::io::AsyncReadExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use wonder_api::{
    pairing::ActionTranscript,
    pairing_protocol::{
        csrf_token_hash, session_expiration_deadline, session_token_hash, DevicePublicKeyJwk,
        PairingError, PairingState, NEVER_EXPIRES_AT_MS,
    },
    ClientMessageReceipt, DeliveryState, HostEventEnvelope, ResyncReason, WonderEvent,
};
use wonder_app_server::AppServerClient;
use wonder_store::{
    AgentFamily, ComputerControlLeaseCreate, ComputerLeaseAcquireResult, ComputerSessionCreate,
    ComputerSessionState, DefaultSpeed, MessageInsert, Store, StoredComputerControlLease,
    StoredComputerSession, StoredConversationFile,
};

#[derive(Clone)]
pub struct AppState {
    pub ingestion: ingestion::Ingestion,
    pub store: Store,
    pub logger: Arc<logging::JsonlLogger>,
    pub loopback_capability: String,
    pub host_epoch: String,
    pub started_at: String,
    pub events: tokio::sync::broadcast::Sender<HostEventEnvelope>,
    pub journal_wake: sync::JournalWake,
    pub revocations: tokio::sync::broadcast::Sender<String>,
    pub pairing: Arc<tokio::sync::Mutex<HostPairingState>>,
    pub public_origin: String,
    pub public_origin_file: Option<PathBuf>,
    /// What the home network listeners serve now (empty when the route is off).
    pub home_routes: lan::HomeRoutes,
    pub host_installation_id: String,
    pub claude: Option<Arc<claude::Runtime>>,
    /// OpenCode's ACP bridge, present only while the experiment is on.
    pub opencode: Option<Arc<claude::Runtime>>,
    /// Cursor Agent through the shared ACP bridge, with a separate journal.
    pub cursor: Option<Arc<claude::Runtime>>,
    /// Owner-selected folder projects in the normal provider homes.
    pub projects: Arc<projects::ProjectRuntime>,
    pub denied_roots: Vec<String>,
    pub data_dir: PathBuf,
    pub codex_home: PathBuf,
    /// Shared admission for Project submissions through receipt storage;
    /// exclusive admission for reconnect and idle retirement. Take after a
    /// chat's ordering gate and before dispatch_lock. Stop does not take it.
    pub provider_reconnect: Arc<RwLock<()>>,
    /// Per-host native status cache and in-flight probe ownership.
    pub installed_providers: Arc<provider_runtime::InstalledCaches>,
    pub dispatch_lock: Arc<tokio::sync::Mutex<()>>,
    /// Wakes the dispatch loop as soon as a send is accepted; its one-second
    /// sleep remains the backstop for everything else.
    pub dispatch_wake: Arc<tokio::sync::Notify>,
    /// In-process wait ownership; independent hosts never share dependencies.
    pub thread_waits: Arc<thread_tools::ThreadWaits>,
    pub update_admission: Arc<update_admission::UpdateAdmission>,
    pub approval_lock: Arc<tokio::sync::Mutex<()>>,
    pub runtime_catalog: Arc<RwLock<RuntimeCatalog>>,
    pub computer_use_enabled: bool,
    pub computer_use_bin: Option<PathBuf>,
    pub computer_supervisor: Arc<computer_sessions::ComputerSessionSupervisor>,
    /// Typed settings for this Mac's booted simulators (`simulators`).
    pub simulators: Arc<simulators::Simulators>,
    /// Voice mode's OpenAI key (kept on this Mac) and client-secret minting.
    pub voice: Arc<voice::Voice>,
    /// The owner's connection to T3 Code on this Mac (see `t3_code`).
    pub t3_code: Arc<t3_code::T3Code>,
    /// Claude Code sessions the Claude app archived (see `claude_app`).
    pub claude_app: Arc<claude_app::ClaudeApp>,
    /// Owner terminals on host PTYs (see `terminal`).
    pub terminals: Arc<terminal::TerminalHost>,
    /// The Mac's agent status for Live Activities (see `live_activity`).
    pub live_activity: Arc<live_activity::Hub>,
    /// Unauthenticated sealed RPC handshakes in progress (see `rpc::server`).
    pub sealed_handshakes: rpc::server::HandshakeSlots,
    /// This computer's relay tunnel (see `relay`).
    pub relay: relay::Manager,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeCatalog {
    pub models: Vec<ModelOption>,
    /// The owner's default model, effort and speed per harness.
    pub preferences: HashMap<AgentFamily, wonder_store::AgentDefaultPreference>,
    /// Models the owner hid per harness. `resolve_defaults` folds them into
    /// each model's `hidden`, so every list and check that skips hidden
    /// models skips these too.
    pub owner_hidden: HashMap<AgentFamily, std::collections::HashSet<String>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub agent_family: AgentFamily,
    pub capabilities: ModelCapabilities,
    pub id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub model_specialty: Option<String>,
    pub hidden: bool,
    pub reasoning_efforts: Vec<ChoiceOption>,
    pub default_reasoning_effort: Option<String>,
    pub service_tiers: Vec<ChoiceOption>,
    pub default_service_tier: Option<String>,
    /// The host's pick for this family when a thread has no model yet.
    pub is_default: bool,
    /// The provider's own effort and speed defaults, kept so the published
    /// ones can follow the owner's preference and return to these.
    #[serde(skip)]
    pub provider_default: Option<ProviderDefaults>,
    /// Ids the provider's own transcripts use for this model.
    #[serde(skip)]
    pub native_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderDefaults {
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    /// Whether the provider itself hides the model, apart from the owner.
    pub hidden: bool,
}

/// The per-model view of the provider's capability record; every value comes
/// from `ProviderCapabilities`, which `GET /api/v1/projects` publishes whole.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilities {
    /// Send now (Guide) into a running reply.
    pub guide: bool,
    pub goals: bool,
    pub image_generation: bool,
}
impl ModelCapabilities {
    fn for_family(family: AgentFamily) -> Self {
        let provider = family.provider().capabilities;
        Self {
            guide: provider.steer,
            goals: provider.goals,
            image_generation: provider.image_generation,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChoiceOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeAppSummary {
    id: String,
    name: String,
    description: Option<String>,
    category: Option<String>,
    enabled: bool,
    accessible: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeSkillSummary {
    name: String,
    description: String,
    enabled: bool,
    plugin_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeMcpServerSummary {
    name: String,
    plugin_id: Option<String>,
    auth_status: String,
    runtime_status: Option<String>,
    tool_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeCapabilitiesResponse {
    apps: Vec<RuntimeAppSummary>,
    skills: Vec<RuntimeSkillSummary>,
    mcp_servers: Vec<RuntimeMcpServerSummary>,
    warnings: Vec<String>,
    refreshed_at: String,
}

impl RuntimeCatalog {
    #[cfg(test)]
    /// The provider that listed `model`, or `None` when no provider has.
    pub fn family_of(&self, model: &str) -> Option<AgentFamily> {
        self.models
            .iter()
            .find(|m| m.id == model)
            .map(|m| m.agent_family)
    }

    /// Adds the models `family`'s provider listed. The provider that answered
    /// owns them; a model id is never read to decide its family. A provider
    /// cannot list an id in another provider's namespace or one the other
    /// provider already lists, so ids stay unambiguous in the shared catalog.
    ///
    /// Returns how many listed models were refused for leaving the provider's
    /// namespace or shadowing another provider's model.
    pub fn apply_models_page(&mut self, family: AgentFamily, result: &serde_json::Value) -> usize {
        let Some(data) = result.get("data").and_then(serde_json::Value::as_array) else {
            return 0;
        };
        let mut refused = 0;
        for model in data {
            let Some(id) = model
                .get("id")
                .or_else(|| model.get("model"))
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            if !family.model_in_own_namespace(id)
                || family.model_in_foreign_namespace(id)
                || self
                    .models
                    .iter()
                    .any(|existing| existing.id == id && existing.agent_family != family)
            {
                refused += 1;
                continue;
            }
            let reasoning_efforts = model
                .get("supportedReasoningEfforts")
                .and_then(serde_json::Value::as_array)
                .map(|values| values.iter().filter_map(choice_option).collect())
                .unwrap_or_default();
            let mut service_tiers: Vec<ChoiceOption> = model
                .get("serviceTiers")
                .and_then(serde_json::Value::as_array)
                .map(|values| values.iter().filter_map(choice_option).collect())
                .unwrap_or_default();
            if family.provider().default_speed == DefaultSpeed::Standard
                && !service_tiers
                    .iter()
                    .any(|option: &ChoiceOption| option.id == "default")
            {
                service_tiers.insert(
                    0,
                    ChoiceOption {
                        id: "default".into(),
                        label: "Standard".into(),
                        description: Some("Standard speed, standard usage".into()),
                    },
                );
            }
            let option = ModelOption {
                agent_family: family,
                capabilities: ModelCapabilities::for_family(family),
                id: id.to_owned(),
                display_name: model_names::display_name(
                    family,
                    id,
                    model.get("displayName").and_then(serde_json::Value::as_str),
                ),
                description: model
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                model_specialty: model
                    .get("modelSpecialty")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                hidden: model
                    .get("hidden")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                reasoning_efforts,
                default_reasoning_effort: model
                    .get("defaultReasoningEffort")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                service_tiers,
                default_service_tier: model
                    .get("defaultServiceTier")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                is_default: false,
                provider_default: None,
                native_ids: model
                    .get("nativeModelIds")
                    .and_then(serde_json::Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            self.models.retain(|existing| existing.id != option.id);
            self.models.push(option);
        }
        self.resolve_defaults();
        refused
    }
}

fn choice_option(value: &serde_json::Value) -> Option<ChoiceOption> {
    if let Some(id) = value.as_str() {
        return Some(ChoiceOption {
            id: id.to_owned(),
            label: id.to_owned(),
            description: None,
        });
    }
    let id = value
        .get("id")
        .or_else(|| value.get("reasoningEffort"))
        .or_else(|| value.get("name"))
        .and_then(serde_json::Value::as_str)?;
    Some(ChoiceOption {
        id: id.to_owned(),
        label: value
            .get("name")
            .or_else(|| value.get("displayName"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(id)
            .to_owned(),
        description: value
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ATTACHMENT_REQUEST_BYTES: usize = 12 * 1024 * 1024;

#[derive(Clone, Debug)]
struct AuthenticatedDevice {
    device_id: String,
    session_binding: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignedActionFields {
    action_nonce: Option<String>,
    issued_at_ms: Option<u64>,
    signature: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResetWorkspaceRequest {
    keep_device_id: Option<String>,
    #[serde(flatten)]
    action: SignedActionFields,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResetWorkspaceSummary {
    preserved_device_id: String,
    revoked_device_count: usize,
}

fn action_body_sha256(value: &serde_json::Value) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_string(value)
            .unwrap_or_else(|_| "null".into())
            .as_bytes(),
    ))
}

async fn verify_signed_action(
    state: &AppState,
    device: &AuthenticatedDevice,
    action: &str,
    target: &str,
    body_sha256: &str,
    fields: &SignedActionFields,
    expected_state: &str,
) -> Result<(), &'static str> {
    let Some(action_nonce) = fields
        .action_nonce
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        return Err("signed action fields are required");
    };
    let Some(issued_at_ms) = fields.issued_at_ms else {
        return Err("signed action fields are required");
    };
    let Some(signature) = fields
        .signature
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        return Err("signed action fields are required");
    };
    if issued_at_ms.abs_diff(now_ms()) > 60_000 {
        return Err("signed action is stale");
    }
    let transcript = ActionTranscript {
        action,
        target,
        body_sha256,
        action_nonce,
        session_binding: &device.session_binding,
        device_id: &device.device_id,
        host_installation_id: &state.host_installation_id,
        issued_at_ms,
        expected_state,
    };
    if state
        .pairing
        .lock()
        .await
        .verify_action_signature(&device.device_id, &transcript, signature)
        .is_err()
    {
        return Err("invalid action signature");
    }
    if !state
        .store
        .claim_action_nonce(action_nonce, &now_ms().to_string())
        .await
        .map_err(|_| "action replay state unavailable")?
    {
        return Err("signed action was already used");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct OwnerAuthority;

#[derive(Clone, Copy, Debug)]
struct LocalOwnerAuthority;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageRequest {
    /// The model this message is sent with; for a Project thread it may belong
    /// to the other provider, which moves the thread when the message is
    /// released for delivery.
    #[serde(default)]
    pub project_model: Option<provider_switch::MessageModel>,
    pub device_id: String,
    pub client_message_id: String,
    pub body: String,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    #[serde(default)]
    pub codex_skills: Vec<wonder_store::CodexSkillSelection>,
    /// Send now on a chat another app on the Mac has open: Wonder ends that
    /// app's process for the chat instead of waiting for it. Resending a
    /// queued message's clientMessageId with this set takes it over too.
    #[serde(default)]
    pub take_over: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteerTurnRequest {
    pub device_id: String,
    pub client_message_id: String,
    pub body: String,
    pub expected_turn_id: String,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    #[serde(default)]
    pub codex_skills: Vec<wonder_store::CodexSkillSelection>,
}

fn steer_turn_params_with_attachments(
    thread_id: &str,
    expected_turn_id: &str,
    client_message_id: &str,
    input: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "threadId": thread_id,
        "expectedTurnId": expected_turn_id,
        "clientUserMessageId": client_message_id,
        "input": input,
    })
}

/// A Codex or Claude turn's input, built only from verified bytes
/// (`agent_images::for_agent`) so a file swapped after the check is never what
/// the provider reads. Images and audio go inline. Other files go as a fresh
/// copy of those bytes (`agent_images::sent_copy`) for the agent to open,
/// never as the stored path.
async fn turn_input(
    body: &str,
    workspace: &str,
    attachments: &[(StoredConversationFile, Option<Vec<u8>>)],
) -> Result<Vec<serde_json::Value>, String> {
    let mut input = vec![serde_json::json!({ "type": "text", "text": body })];
    for (attachment, bytes) in attachments {
        // A picture kept by reference has no stored copy to send.
        let Some(bytes) = bytes.as_deref() else {
            if uploads::is_video(attachment.mime_type.as_deref().unwrap_or_default()) {
                let relative = attachment
                    .relative_path
                    .as_deref()
                    .ok_or("The video attachment is unavailable.")?;
                let path = FsPath::new(workspace).join(relative);
                input.push(serde_json::json!({"type": "text", "text": format!("Attached file `{}` is available at `{}`.", attachment.name, path.display())}));
            }
            continue;
        };
        let mime_type = attachment.mime_type.as_deref().unwrap_or_default();
        let inline = |kind: &str, mime: &str| {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            serde_json::json!({ "type": kind, "url": format!("data:{mime};base64,{data}") })
        };
        if mime_type.starts_with("image/") {
            let mime = agent_images::agent_mime(bytes).unwrap_or(mime_type);
            input.push(inline("image", mime));
        } else if mime_type.starts_with("audio/") {
            input.push(inline("audio", mime_type));
        } else {
            let path = agent_images::sent_copy(workspace, attachment, bytes).await?;
            let path = path
                .to_str()
                .ok_or("An attached file couldn't be prepared for the agent. Try again.")?;
            input.push(serde_json::json!({
                "type": "text",
                "text": format!("Attached file `{}` is available at `{}`.", attachment.name, path),
            }));
        }
    }
    Ok(input)
}

fn valid_attachment_ids(attachment_ids: &[String]) -> bool {
    if attachment_ids.len() > 32
        || attachment_ids
            .iter()
            .any(|id| id.is_empty() || uuid::Uuid::parse_str(id).is_err())
    {
        return false;
    }
    let mut sorted = attachment_ids.to_vec();
    sorted.sort();
    sorted.windows(2).all(|pair| pair[0] != pair[1])
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub host_name: String,
    pub api_version: &'static str,
    pub host_epoch: String,
    pub host_installation_id: String,
    pub state: &'static str,
    pub codex_transport: &'static str,
    pub started_at: String,
    pub public_origin: Option<String>,
    /// Operating system of the host (`macos`, `linux`), so clients can name
    /// the computer instead of assuming a Mac and hide Mac-only features.
    pub platform: &'static str,
    pub execution: ingestion::ReadinessReport,
    /// Additive capabilities so older clients and hosts degrade honestly.
    pub features: Vec<&'static str>,
    /// Every way a paired phone can reach this computer now, home network
    /// first. Only authenticated devices read host status, and a phone uses a
    /// home route only with the key pin it saved at pairing.
    pub routes: Vec<PublishedRoute>,
    /// The key the home network listeners present, while they run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_key_pin: Option<String>,
}

/// One route in host status: the kinds a phone knows (`home`, `tailscale`,
/// `relay`). An owner's own address is listed as `tailscale`, as pairing
/// saves it.
#[derive(Debug, Serialize)]
pub struct PublishedRoute {
    pub kind: &'static str,
    pub origin: String,
}

/// Home network origins first, then the running host's other routes.
fn published_routes(home: Vec<String>, trusted: Vec<TrustedOrigin>) -> Vec<PublishedRoute> {
    let mut routes: Vec<PublishedRoute> = home
        .into_iter()
        .map(|origin| PublishedRoute {
            kind: "home",
            origin,
        })
        .collect();
    for trusted in trusted {
        let kind = match trusted.kind {
            OriginKind::Home => "home",
            OriginKind::Tailscale | OriginKind::Custom => "tailscale",
            OriginKind::Relay => "relay",
        };
        if !routes.iter().any(|route| route.origin == trusted.origin) {
            routes.push(PublishedRoute {
                kind,
                origin: trusted.origin,
            });
        }
    }
    routes
}

async fn no_store_signaling(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub fn router(state: AppState) -> Router {
    let upload_routes = uploads::routes(&state);
    let signaling_routes = Router::new()
        .route(
            "/api/v1/computer/sessions/{id}/signaling",
            post(computer_sessions::signal_poll),
        )
        .route(
            "/api/v1/computer/sessions/{id}/signaling/answer",
            post(computer_sessions::signal_answer),
        )
        .route(
            "/api/v1/computer/sessions/{id}/signaling/candidate",
            post(computer_sessions::signal_candidate),
        )
        .route(
            "/api/v1/computer/sessions/{id}/signaling/close",
            post(computer_sessions::signal_close),
        )
        .layer(DefaultBodyLimit::max(96 * 1024))
        .layer(middleware::from_fn(no_store_signaling));

    let api = Router::new()
        .route(
            "/api/v1/diagnostics/batches",
            post(diagnostics::ingest).layer(DefaultBodyLimit::max(diagnostics::MAX_BATCH)),
        )
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/host/status", get(host_status))
        .route("/api/v1/host/proof", get(host_proof))
        .route(
            "/api/v1/host/update/prepare",
            post(update_admission::prepare),
        )
        .route("/api/v1/host/update/cancel", post(update_admission::cancel))
        .route("/api/v1/host/update/work", get(update_admission::work))
        .route("/api/v1/host/providers", get(provider_runtime::status))
        .route(
            "/api/v1/host/media/orphans",
            get(media_refs::orphan_report_route),
        )
        .route(
            "/api/v1/host/media/orphans/prune",
            post(media_refs::prune_orphans_route),
        )
        .route(
            "/api/v1/host/providers/{id}/reconnect",
            post(provider_runtime::reconnect),
        )
        .route("/api/v1/providers", get(provider_runtime::owner_status))
        .route(
            "/api/v1/providers/{id}/reconnect",
            post(provider_runtime::owner_reconnect),
        )
        .route(
            "/api/v1/providers/{id}/update",
            post(provider_updates::owner_update),
        )
        .route("/api/v1/push/config", get(push::config))
        .route("/api/v1/push/registration", post(push::register))
        .route("/api/v1/push/revoke", post(push::revoke))
        .route("/api/v1/push/presence", post(push::presence))
        .route("/api/v1/push/routes/{route}", get(push::route))
        .route("/api/v1/push/live-activity", post(live_activity::register))
        .route("/api/v1/account/usage", get(account_usage::read))
        .route("/api/v1/voice", get(voice::status))
        .route("/api/v1/voice/credits", get(voice::credits))
        .route(
            "/api/v1/voice/openai-key",
            put(voice::save_key).delete(voice::remove_key),
        )
        .route("/api/v1/voice/sessions", post(voice::create_session))
        .route(
            "/api/v1/voice/settings",
            get(voice::settings).put(voice::update_settings),
        )
        .route(
            "/api/v1/voice/dictionary",
            get(voice::dictionary).put(voice::update_dictionary),
        )
        .route("/api/v1/voice/panels/{panel}", delete(voice::end_panel))
        .route(
            "/api/v1/voice/panels/{panel}/usage",
            post(voice::report_usage),
        )
        .route(
            "/api/v1/voice/panels/{panel}/transcript",
            post(voice::save_transcript),
        )
        .route(
            "/api/v1/voice/transcripts",
            get(voice::transcripts).delete(voice::delete_transcripts),
        )
        .route("/api/v1/voice/transcripts/{panel}", get(voice::transcript))
        .route(
            "/api/v1/t3-code",
            get(t3_code::status).delete(t3_code::disconnect),
        )
        .route("/api/v1/t3-code/connect", post(t3_code::connect))
        .route(
            "/api/v1/t3-code/search",
            get(t3_code::threads::search_threads),
        )
        .route(
            "/api/v1/t3-code/threads/{thread}",
            get(t3_code::threads::thread_status),
        )
        .route(
            "/api/v1/t3-code/threads/{thread}/organize",
            post(t3_code::threads::organize_thread),
        )
        .route(
            "/api/v1/t3-code/threads/{thread}/fork",
            post(t3_code::threads::fork_thread),
        )
        .route(
            "/api/v1/t3-code/threads/{thread}/rename",
            post(t3_code::threads::rename_thread),
        )
        .route(
            "/api/v1/projects/{project_id}/t3-code/threads",
            post(projects::launch_in_t3_code),
        )
        .route("/api/v1/runtime/capabilities", get(runtime_capabilities))
        .route(
            "/api/v1/computer/sources/simulators",
            get(computer_sessions::simulator_sources),
        )
        .route(
            "/api/v1/computer/sources/windows",
            get(computer_sessions::window_sources),
        )
        .route(
            "/api/v1/computer/sources/windows/browser-permission",
            post(computer_sessions::ask_browser_permission),
        )
        .route(
            "/api/v1/computer/takeover-requests",
            get(computer_sessions::takeover_requests),
        )
        .route(
            "/api/v1/computer/takeover-requests/{id}/decline",
            post(computer_sessions::decline_takeover),
        )
        .route(
            "/api/v1/computer/streams",
            get(computer_sessions::remote_streams),
        )
        .route(
            "/api/v1/computer/streams/{id}/allow",
            post(computer_sessions::allow_remote_stream),
        )
        .route(
            "/api/v1/computer/streams/{id}/decline",
            post(computer_sessions::decline_remote_stream),
        )
        .route(
            "/api/v1/computer/streams/{id}/stop",
            post(computer_sessions::stop_remote_stream),
        )
        .route(
            "/api/v1/computer/streams/{id}/frame",
            get(computer_sessions::remote_stream_frame),
        )
        .route(
            "/api/v1/computer/apps",
            get(computer_sessions::installed_apps),
        )
        .route("/api/v1/computer/dock", get(computer_sessions::dock))
        .route(
            "/api/v1/computer/apps/icon",
            get(computer_sessions::installed_app_icon),
        )
        .route(
            "/api/v1/computer/sessions/{id}/apps/open",
            post(computer_sessions::open_app),
        )
        .route("/api/v1/simulators", get(simulators::list_route))
        .route(
            "/api/v1/simulators/{udid}/settings",
            get(simulators::settings_route),
        )
        .route(
            "/api/v1/simulators/{udid}/appearance",
            post(simulators::appearance_route),
        )
        .route(
            "/api/v1/simulators/{udid}/content-size",
            post(simulators::content_size_route),
        )
        .route(
            "/api/v1/simulators/{udid}/language",
            post(simulators::language_route),
        )
        .route(
            "/api/v1/simulators/{udid}/apps",
            get(simulators::apps_route),
        )
        .route(
            "/api/v1/simulators/{udid}/permissions/reset",
            post(simulators::reset_permissions_route),
        )
        .route(
            "/api/v1/simulators/{udid}/open-url",
            post(simulators::open_url_route),
        )
        .route(
            "/api/v1/simulators/{udid}/screenshot",
            get(simulators::screenshot_route),
        )
        .route(
            "/api/v1/simulators/{udid}/boot",
            post(simulators::boot_route),
        )
        .route(
            "/api/v1/simulators/{udid}/shutdown",
            post(simulators::shutdown_route),
        )
        .route(
            "/api/v1/simulators/{udid}/hinge",
            post(simulators::hinge_route),
        )
        .route(
            "/api/v1/simulators/{udid}/orientation",
            post(simulators::orientation_route),
        )
        .route("/api/v1/computer/sessions", post(computer_sessions::create))
        .route(
            "/api/v1/computer/sessions/{id}/quality",
            post(computer_sessions::set_quality),
        )
        .route(
            "/api/v1/computer/sessions/{id}",
            get(computer_sessions::read).delete(computer_sessions::end),
        )
        .route(
            "/api/v1/computer/sessions/{id}/admission",
            post(computer_sessions::admit),
        )
        .merge(signaling_routes)
        .route(
            "/api/v1/computer/sessions/{id}/control/acquire",
            post(computer_sessions::acquire_control),
        )
        .route(
            "/api/v1/computer/sessions/{id}/control/heartbeat",
            post(computer_sessions::heartbeat_control),
        )
        .route(
            "/api/v1/computer/sessions/{id}/control/input",
            post(computer_sessions::input_control),
        )
        .route(
            "/api/v1/computer/sessions/{id}/control/release",
            post(computer_sessions::release_control),
        )
        .route(
            "/api/v1/computer/sessions/{id}/control/resume",
            post(computer_sessions::resume_control),
        )
        .route("/api/v1/connected-apps", get(connected_apps::list))
        .route(
            "/api/v1/projects",
            get(projects::list).post(projects::create),
        )
        .route("/api/v1/projects/candidates", get(projects::candidates))
        .route("/api/v1/threads/recent", get(projects::recent_threads))
        .route("/api/v1/projects/{project_id}", patch(projects::update))
        .route(
            "/api/v1/projects/{project_id}/claude-trust",
            get(projects::claude_trust)
                .put(projects::trust_claude)
                .delete(projects::revoke_claude_trust),
        )
        .route(
            "/api/v1/projects/{project_id}/threads",
            get(projects::threads).post(projects::create_thread),
        )
        .route("/api/v1/projects/{project_id}/files", get(project_outputs))
        .route(
            "/api/v1/projects/{project_id}/files/{file_id}/pin",
            put(pin_project_output).delete(unpin_project_output),
        )
        .route(
            "/api/v1/projects/{project_id}/threads/attach",
            post(projects::attach),
        )
        .route(
            "/api/v1/projects/{project_id}/threads/archived",
            get(projects::archived_threads),
        )
        .route(
            "/api/v1/projects/{project_id}/threads/archive",
            post(projects::archive_threads),
        )
        .route(
            "/api/v1/projects/{project_id}/preview/servers",
            get(dev_preview::servers),
        )
        .route(
            "/api/v1/projects/{project_id}/preview/{port}",
            axum::routing::any(dev_preview::proxy),
        )
        .route(
            "/api/v1/projects/{project_id}/preview/{port}/",
            axum::routing::any(dev_preview::proxy),
        )
        .route(
            "/api/v1/projects/{project_id}/preview/{port}/{*path}",
            axum::routing::any(dev_preview::proxy),
        )
        .route(
            "/api/v1/projects/{project_id}/skills",
            get(codex_skills::list_project),
        )
        .route(
            "/api/v1/project-conversations/{conversation_id}/skills",
            get(codex_skills::list_conversation),
        )
        .route(
            "/api/v1/settings/projects",
            get(projects::settings).put(projects::set_settings),
        )
        .route("/api/v1/agent-options", get(default_models::options))
        .route(
            "/api/v1/settings/default-models",
            get(default_models::list).put(default_models::set_new_chat_defaults),
        )
        .route(
            "/api/v1/secrets",
            get(secrets::list).delete(secrets::delete),
        )
        .route("/api/v1/settings/wonder-skills", get(wonder_skills::list))
        .route(
            "/api/v1/settings/wonder-skills/{skill_id}",
            put(wonder_skills::set),
        )
        .route(
            "/api/v1/settings/default-models/{family}",
            put(default_models::set),
        )
        .route(
            "/api/v1/settings/model-visibility",
            get(default_models::visibility_list),
        )
        .route(
            "/api/v1/settings/model-visibility/{family}",
            put(default_models::set_visibility),
        )
        .route(
            "/api/v1/project-conversations/{conversation_id}",
            get(projects::conversation)
                .patch(projects::update_conversation)
                .delete(projects::delete_conversation),
        )
        .route(
            "/api/v1/project-conversations/{conversation_id}/fork",
            post(projects::fork),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/rewind",
            post(rewind::rewind),
        )
        .route(
            "/api/v1/project-conversations/{conversation_id}/worktree",
            post(worktree_handoff::handoff),
        )
        .route(
            "/api/v1/settings/thread-tools",
            get(agent_settings::thread_tools),
        )
        .route(
            "/api/v1/settings/thread-tools/{family}",
            put(agent_settings::set_thread_tools),
        )
        .route(
            "/api/v1/settings/chat-detail",
            get(agent_settings::chat_detail),
        )
        .route(
            "/api/v1/settings/chat-detail/{family}",
            put(agent_settings::set_chat_detail),
        )
        .route(
            "/api/v1/host/name",
            get(host_name::get_name).put(host_name::set_name),
        )
        .route(
            "/api/v1/settings/custom-instructions",
            get(agent_settings::custom_instructions),
        )
        .route(
            "/api/v1/settings/custom-instructions/{family}",
            put(agent_settings::set_custom_instruction),
        )
        .route(
            "/api/v1/settings/computer-use",
            get(computer_runtime::computer_use).put(computer_runtime::set_computer_use),
        )
        .route(
            "/api/v1/settings/claude-app",
            get(agent_settings::claude_app).put(agent_settings::set_claude_app),
        )
        .route(
            "/api/v1/settings/provider-runtime",
            get(provider_runtime_config::list),
        )
        .route(
            "/api/v1/settings/provider-runtime/{family}",
            put(provider_runtime_config::set),
        )
        .route("/api/v1/settings/file-restore", get(rewind::file_restore))
        .route(
            "/api/v1/settings/file-restore/{family}",
            put(rewind::set_file_restore),
        )
        .route(
            "/api/v1/project-conversations/{conversation_id}/subagents",
            get(project_subagents::list),
        )
        .route(
            "/api/v1/project-conversations/{conversation_id}/pull-requests",
            get(pull_requests::list),
        )
        .route(
            "/api/v1/project-conversations/{conversation_id}/pull-requests/detail",
            get(pull_requests::detail),
        )
        .route(
            "/api/v1/project-conversations/{conversation_id}/subagents/{thread_id}/transcript",
            get(project_subagents::transcript),
        )
        .route(
            "/api/v1/project-conversations/{conversation_id}/subagents/{thread_id}/stop",
            post(project_subagents::stop),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/desktop-continuation",
            get(projects::continuation),
        )
        .route("/api/v1/filesystem", get(filesystem::browse))
        .route(
            "/api/v1/conversations/{conversation_id}/workspace/roots",
            get(filesystem::workspace_roots),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/workspace/directory",
            get(filesystem::workspace_directory),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/workspace/file",
            get(filesystem::workspace_file),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/workspace/media",
            get(filesystem::workspace_media),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/workspace/search",
            get(filesystem::workspace_search),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/commands",
            get(composer_commands::list),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/mentions/threads",
            get(composer_mentions::search),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/workspace/git/status",
            get(filesystem::workspace_git_status),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/repos",
            get(filesystem::thread_repositories),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/repos/actions",
            post(filesystem::repository_action),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/repos/pull-request",
            get(filesystem::pull_request_merge_state),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/workspace/git/summary",
            get(filesystem::workspace_git_summary),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/workspace/git/changes",
            get(filesystem::workspace_git_changes),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/workspace/git/thread-changes",
            get(filesystem::workspace_thread_changes),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/workspace/git/diff",
            get(filesystem::workspace_git_diff),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/terminals",
            get(terminal::list).post(terminal::open),
        )
        .route(
            "/api/v1/terminals/{terminal_id}",
            axum::routing::delete(terminal::close),
        )
        .route(
            "/api/v1/terminals/{terminal_id}/socket",
            get(terminal::socket),
        )
        .route("/api/v1/search", get(search_endpoint))
        .route(
            "/api/v1/conversations/{conversation_id}/search/occurrences",
            get(search_occurrences_endpoint),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/chat-detail",
            put(agent_settings::set_conversation_chat_detail),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/notifications",
            get(chat_notifications).put(set_chat_notifications),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/goal",
            get(goals::get).put(goals::set).delete(goals::clear),
        )
        .route(
            "/api/v1/automations",
            get(list_automations).post(create_automation),
        )
        .route("/api/v1/automations/preview", post(preview_automation))
        .route(
            "/api/v1/automations/{automation_id}",
            patch(update_automation).delete(delete_automation),
        )
        .route(
            "/api/v1/automations/{automation_id}/run",
            post(run_automation_now),
        )
        .route(
            "/api/v1/automations/{automation_id}/runs",
            get(list_automation_runs),
        )
        .route("/api/v1/events", get(events_socket))
        .route("/api/v1/rpc", get(rpc::server::socket))
        .route("/api/v1/rpc/sealed", get(rpc::server::sealed_socket))
        .route("/api/v1/sync/checkpoint", get(sync::checkpoint))
        .route("/api/v1/events/challenge", get(event_challenge))
        .route("/api/v1/pairing/offers", post(create_pairing_offer))
        .route("/api/v1/pairing/pending", get(list_pending_enrollments))
        .route(
            "/api/v1/pairing/pending/{device_id}/confirm",
            post(confirm_enrollment),
        )
        .route(
            "/api/v1/pairing/pending/{device_id}/reject",
            post(reject_enrollment),
        )
        .route(
            "/api/v1/pairing/code",
            post(claim_pairing_code).layer(DefaultBodyLimit::max(MAX_PUBLIC_PAIRING_BODY_BYTES)),
        )
        .route(
            "/api/v1/pairing/offers/{offer_id}/cancel",
            post(cancel_pairing_offer),
        )
        .route(
            "/api/v1/pairing/claim",
            post(claim_pairing_offer).layer(DefaultBodyLimit::max(MAX_PUBLIC_PAIRING_BODY_BYTES)),
        )
        .route(
            "/api/v1/pairing/session/refresh-challenge",
            post(create_session_refresh_challenge)
                .layer(DefaultBodyLimit::max(MAX_PUBLIC_PAIRING_BODY_BYTES)),
        )
        .route(
            "/api/v1/pairing/session",
            post(create_session).layer(DefaultBodyLimit::max(MAX_PUBLIC_PAIRING_BODY_BYTES)),
        )
        .route("/api/v1/devices", get(list_devices))
        .route("/api/v1/devices/{device_id}", patch(rename_device))
        .route("/api/v1/devices/{device_id}/revoke", post(revoke_device))
        .route("/api/v1/devices/{device_id}/forget", post(forget_device))
        .route("/api/v1/workspace/reset", post(reset_workspace))
        .route("/api/v1/relay", get(relay_status))
        .route("/api/v1/relay/settings", put(relay_settings))
        .route("/api/v1/relay/enable", post(relay_enable))
        .route(
            "/api/v1/conversations/{conversation_id}/queue",
            get(queue::list).post(queue::reorder),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/queue/{message_id}",
            post(queue::edit),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/questions",
            get(questions::list),
        )
        .route("/api/v1/questions", get(questions::list_pending))
        .route(
            "/api/v1/conversations/{conversation_id}/questions/{question_id}",
            post(questions::answer),
        )
        .route("/api/v1/approvals", get(list_approvals))
        .route(
            "/api/v1/approvals/{approval_id}/resolve",
            post(resolve_approval),
        )
        .route(
            "/api/v1/secret-requests/{approval_id}",
            post(save_requested_secret),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/usage-resume",
            delete(cancel_usage_resume),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/messages",
            post(send_message),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/delegations",
            post(projects::delegate),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/turns/{turn_id}/steer",
            post(steer_turn),
        )
        .route(
            "/api/v1/conversations/{conversation_id}",
            get(conversation_snapshot).patch(update_conversation),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/history",
            get(conversation_snapshot),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/items/{item_id}/output",
            get(history::tool_output),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/history/refresh",
            get(history_refresh_status).post(refresh_history_request),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/history/activity",
            get(history::history_activity),
        )
        .merge(upload_routes)
        .route(
            "/api/v1/conversations/{conversation_id}/files",
            get(conversation_files)
                .post(upload_conversation_file)
                .layer(DefaultBodyLimit::max(MAX_ATTACHMENT_REQUEST_BYTES)),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/files/{file_id}",
            get(conversation_file).delete(remove_unsent_upload),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/turns/{turn_id}/interrupt",
            post(interrupt_turn_http),
        )
        .route("/api/v1/messages/{message_id}/retry", post(retry_message))
        // A pairing link opened outside the app (a Camera-app scan) lands here.
        // API and WebSocket authentication stay in the shared middleware.
        .route("/pair", get(pairing_link_opened_in_browser))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            update_admission::guard_new_work,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_loopback_capability,
        ));
    mcp_http::mount(api, state.clone()).with_state(state)
}

pub async fn publish_event(
    state: &AppState,
    event: WonderEvent,
) -> Result<HostEventEnvelope, sqlx::Error> {
    publish_event_with_context(state, event, EventContext::default()).await
}

#[derive(Default)]
struct EventContext {
    request_id: Option<String>,
    conversation_id: Option<String>,
    message_id: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    item_id: Option<String>,
    approval_id: Option<String>,
}

async fn publish_event_with_context(
    state: &AppState,
    event: WonderEvent,
    context: EventContext,
) -> Result<HostEventEnvelope, sqlx::Error> {
    let mut envelope = event_envelope(state, event, context);
    state.store.commit_event(&mut envelope).await?;
    let _ = state.events.send(envelope.clone());
    Ok(envelope)
}

/// An uncommitted envelope; its sequence is assigned when it is journaled.
fn event_envelope(
    state: &AppState,
    event: WonderEvent,
    context: EventContext,
) -> HostEventEnvelope {
    HostEventEnvelope {
        event_id: uuid::Uuid::new_v4().to_string(),
        host_epoch: state.host_epoch.clone(),
        sequence: 0,
        occurred_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| state.started_at.clone()),
        request_id: context.request_id,
        device_id: None,
        conversation_id: context.conversation_id,
        message_id: context.message_id,
        thread_id: context.thread_id,
        turn_id: context.turn_id,
        item_id: context.item_id,
        approval_id: context.approval_id,
        event,
    }
}

/// Tests deliver Codex notifications the way durable ingestion does
/// (`ingestion.rs`), so they exercise the shipped projection path.
#[cfg(test)]
pub async fn publish_app_server_notification(state: &AppState, notification: serde_json::Value) {
    process_app_server_notification(state, notification, false).await;
}

fn notification_thread_id(params: &serde_json::Value) -> Option<String> {
    let item = params.get("item");
    string_field(params, "threadId")
        .or_else(|| item.and_then(|item| string_field(item, "threadId")))
}

fn notification_turn_id(params: &serde_json::Value) -> Option<String> {
    let item = params.get("item");
    string_field(params, "turnId")
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| item.and_then(|item| string_field(item, "turnId")))
}

#[allow(clippy::too_many_arguments)]
async fn enqueue_pending_app_server_notification(
    state: &AppState,
    method: &str,
    notification: &serde_json::Value,
    params: &serde_json::Value,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
    item_id: Option<&str>,
    identity_key: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let Ok(params_json) = serde_json::to_string(params) else {
        return Err(sqlx::Error::Protocol(
            "failed to serialize App Server notification parameters".into(),
        ));
    };
    state
        .store
        .enqueue_pending_app_server_notification(
            &uuid::Uuid::new_v4().to_string(),
            identity_key,
            method,
            thread_id,
            turn_id,
            item_id,
            &notification.to_string(),
            &params_json,
            &now_ms().to_string(),
        )
        .await
}

#[allow(clippy::too_many_arguments)]
async fn fail_app_server_notification(
    state: &AppState,
    method: &str,
    notification: &serde_json::Value,
    params: &serde_json::Value,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
    item_id: Option<&str>,
    notification_key: Option<&str>,
    queue_for_retry: bool,
) -> bool {
    if queue_for_retry {
        // Preserve the complete envelope. If this enqueue itself fails, the
        // receipt is still released below so the App Server can retry.
        let _ = enqueue_pending_app_server_notification(
            state,
            method,
            notification,
            params,
            thread_id,
            turn_id,
            item_id,
            notification_key,
        )
        .await;
    }
    if let Some(notification_key) = notification_key {
        let _ = state
            .store
            .release_app_server_notification(notification_key)
            .await;
    }
    false
}

#[allow(clippy::collapsible_if)]
async fn process_app_server_notification(
    state: &AppState,
    notification: serde_json::Value,
    queue_failed_assistant: bool,
) -> bool {
    apply_app_server_notification(state, notification, queue_failed_assistant, false).await
}

/// `replaying` is set by the buffer drains: a replayed event is applied, never
/// queued again behind itself.
async fn apply_app_server_notification(
    state: &AppState,
    notification: serde_json::Value,
    queue_failed_assistant: bool,
    replaying: bool,
) -> bool {
    apply_app_server_notification_locked(
        state,
        notification,
        queue_failed_assistant,
        replaying,
        None,
        &mut None,
    )
    .await
}

async fn projection_binding(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, sqlx::Error> {
    let thread = notification_thread_id(params);
    let turn = notification_turn_id(params);
    let message = match (thread.as_deref(), turn.as_deref()) {
        (Some(thread), Some(turn)) => {
            state
                .store
                .message_for_codex_thread_and_turn(thread, turn)
                .await?
        }
        _ => None,
    };
    let child = match thread.as_deref() {
        Some(thread) => state.store.subagent_ownership_for_thread(thread).await?,
        None => None,
    };
    let conversation = match (&message, &child, thread.as_deref()) {
        (Some(message), _, _) => Some(message.conversation_id.clone()),
        (_, Some(child), _) => Some(child.conversation_id.clone()),
        (_, _, Some(thread)) => state.store.conversation_for_thread(thread).await?,
        _ => None,
    };
    let project = match conversation.as_deref() {
        Some(conversation) => state
            .store
            .project_conversation(conversation)
            .await?
            .map(|p| {
                serde_json::json!([
                    p.family,
                    p.provider_store,
                    p.native_session_id,
                    p.cwd,
                    p.roots_revision
                ])
            }),
        None => None,
    };
    let hidden = match (thread.as_deref(), turn.as_deref()) {
        (Some(thread), Some(turn)) => {
            state
                .store
                .runtime_turn_hidden_by_reset(thread, turn)
                .await?
        }
        _ => false,
    };
    Ok(serde_json::json!([
        conversation,
        message.map(|m| m.id),
        project,
        child.map(|c| (c.parent_conversation_id, c.parent_thread_id, c.runtime_id)),
        hidden,
        params
            .get("_wonderRuntimeId")
            .and_then(serde_json::Value::as_str)
            .map(|r| state.ingestion.runtime_alive(r))
    ]))
}

async fn apply_app_server_notification_with_deltas(
    state: &AppState,
    notification: serde_json::Value,
    queue_failed_assistant: bool,
    replaying: bool,
    batch: Option<&mut ingestion::DeltaBatch>,
) -> bool {
    let mut dispatch = Some(state.dispatch_lock.lock().await);
    if !replaying {
        if let (Some(thread), Some(turn)) = (
            notification_thread_id(&notification["params"]),
            notification_turn_id(&notification["params"]),
        ) {
            if state
                .store
                .message_for_codex_thread_and_turn(&thread, &turn)
                .await
                .ok()
                .flatten()
                .is_some()
            {
                // A receipt replay dropped lifecycle serialization to take its
                // lane. A live row that wins that race still drains older events.
                if !drain_pending_locked(state, &thread, &turn, &mut dispatch).await {
                    return false;
                }
            }
        }
    }
    apply_app_server_notification_locked(
        state,
        notification,
        queue_failed_assistant,
        replaying,
        batch,
        &mut dispatch,
    )
    .await
}

// The caller owns lifecycle serialization. Only this context can release it;
// untrusted envelopes cannot assert that preparation or validation happened.
async fn apply_app_server_notification_locked<'a>(
    state: &'a AppState,
    notification: serde_json::Value,
    queue_failed_assistant: bool,
    replaying: bool,
    mut batch: Option<&mut ingestion::DeltaBatch>,
    dispatch: &mut Option<tokio::sync::MutexGuard<'a, ()>>,
) -> bool {
    let mut notification = notification;
    crate::secrets::redact_notification(&mut notification);
    let Some(method) = notification
        .get("method")
        .and_then(serde_json::Value::as_str)
    else {
        return true;
    };
    let mut params = notification.get("params").cloned().unwrap_or_default();
    if let Some(runtime_id) = notification.get("_wonderRuntimeId") {
        if params.is_object() {
            params["_wonderRuntimeId"] = runtime_id.clone();
            if let Some(id) = notification
                .get("id")
                .or_else(|| notification.get("params").and_then(|p| p.get("requestId")))
            {
                params["_wonderRequestId"] = id.clone();
            }
        }
    }
    if method == "thread/session/reset" {
        let relock = dispatch.is_some();
        let result = claude::native_session_reset(state, &notification, &params, dispatch).await;
        if relock && dispatch.is_none() {
            *dispatch = Some(state.dispatch_lock.lock().await);
        }
        return result;
    }
    if method == "runtime/activated" {
        if let Some(runtime) = params.get("_wonderRuntimeId").and_then(|id| id.as_str()) {
            state.ingestion.request_discovery(runtime);
        }
        return true;
    }
    let item = params.get("item");
    let thread_id = notification_thread_id(&params);
    let turn_id = string_field(&params, "turnId")
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| item.and_then(|item| string_field(item, "turnId")));
    if let (Some(thread), Some(turn)) = (thread_id.as_deref(), turn_id.as_deref()) {
        match state.store.runtime_turn_hidden_by_reset(thread, turn).await {
            Ok(true) => return true,
            Ok(false) => {}
            Err(_) => return false,
        }
    }
    let binding = if method == "turn/completed" || approval_lifecycle::is_request(method) {
        let binding = match projection_binding(state, &params).await {
            Ok(binding) => binding,
            Err(_) => return false,
        };
        let relock = dispatch.is_some();
        drop(dispatch.take());
        #[cfg(test)]
        if let Some(signal) = params
            .get("_wonderRuntimeId")
            .and_then(serde_json::Value::as_str)
            .and_then(|runtime| ingestion::PREPARING.lock().unwrap().remove(runtime))
        {
            let _ = signal.send(());
        }
        let observed = approval_lifecycle::observe(state, method, &params).await;
        if relock {
            *dispatch = Some(state.dispatch_lock.lock().await);
            if projection_binding(state, &params).await.ok().as_ref() != Some(&binding) {
                return false;
            }
        }
        match observed {
            Ok(true) => {}
            Ok(false) => {
                if approval_lifecycle::is_request(method) {
                    approval_lifecycle::decline_late_request(state, method, &params).await;
                }
                return true;
            }
            Err(_) => return false,
        }
        binding
    } else {
        serde_json::Value::Null
    };
    if method == "turn/diff/updated" {
        if let (Some(thread), Some(turn), Some(diff)) = (
            thread_id.as_deref(),
            turn_id.as_deref(),
            params.get("diff").and_then(serde_json::Value::as_str),
        ) {
            turn_diff::record(&state.data_dir, thread, turn, diff);
        }
        return true;
    }
    // A runtime turn starts from the baseline taken before Cupola sent it,
    // and ends at its completion.
    if let (Some(thread), Some(turn)) = (thread_id.as_deref(), turn_id.as_deref()) {
        use filesystem::thread_changes::{queue_step, Step};
        match method {
            "turn/started" => drop(queue_step(state, thread, Step::Began(turn.to_owned()))),
            "turn/completed" => {
                drop(queue_step(state, thread, Step::Ended(turn.to_owned())));
                // No newer diff will come: save the turn's latest now.
                let (data_dir, thread) = (state.data_dir.clone(), thread.to_owned());
                drop(tokio::task::spawn_blocking(move || {
                    turn_diff::flush_thread(&data_dir, &thread)
                }));
            }
            _ => {}
        }
    }
    let item_id = string_field(&params, "itemId").or_else(|| {
        params
            .get("item")
            .and_then(|item| item.get("id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    });
    if method == "turn/completed" {
        if let (Some(thread), Some(turn)) = (thread_id.as_deref(), turn_id.as_deref()) {
            let relock = dispatch.is_some();
            drop(dispatch.take());
            let terminal = update_handoff::terminal(
                state,
                thread,
                turn,
                params
                    .pointer("/turn/status")
                    .and_then(serde_json::Value::as_str),
            )
            .await;
            if relock {
                *dispatch = Some(state.dispatch_lock.lock().await);
                if projection_binding(state, &params).await.ok().as_ref() != Some(&binding) {
                    return false;
                }
            }
            match terminal {
                Ok(true) => return true,
                Ok(false) => {}
                Err(_) => return false,
            }
        }
    }
    if params
        .pointer("/item/type")
        .and_then(serde_json::Value::as_str)
        == Some("userMessage")
    {
        if let (Some(thread), Some(client)) = (
            thread_id.as_deref(),
            params
                .pointer("/item/clientId")
                .and_then(serde_json::Value::as_str),
        ) {
            match state.store.is_update_continuation(thread, client).await {
                Ok(true) => return true,
                Ok(false) => {}
                Err(_) => return false,
            }
        }
    }
    if matches!(method, "thread/goal/updated" | "thread/goal/cleared") {
        let Some(thread) = thread_id.as_deref() else {
            return false;
        };
        if method == "thread/goal/cleared" {
            if let Ok(Some(conversation)) = state.store.goal_conversation_for_thread(thread).await {
                if state
                    .store
                    .clear_goal_time_limit(&conversation)
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            return state.store.clear_goal_thread(thread).await.is_ok()
                && state
                    .store
                    .delete_pending_goal_updates(thread)
                    .await
                    .is_ok();
        }
        let conversation = match state.store.conversation_for_thread(thread).await {
            Ok(Some(conversation)) => conversation,
            // No chat is bound to this thread yet (it was started elsewhere
            // or is still being opened). Keep the latest update; binding the
            // chat replays it.
            Ok(None) => {
                return state
                    .store
                    .delete_pending_goal_updates(thread)
                    .await
                    .is_ok()
                    && enqueue_pending_app_server_notification(
                        state,
                        method,
                        &notification,
                        &params,
                        Some(thread),
                        turn_id.as_deref(),
                        None,
                        None,
                    )
                    .await
                    .is_ok();
            }
            Err(_) => return false,
        };
        if state
            .store
            .set_goal_thread(&conversation, thread)
            .await
            .is_err()
        {
            return false;
        }
        if let Some(turn) = turn_id.as_deref() {
            if claim_goal_turn(state, &conversation, thread, turn)
                .await
                .is_err()
            {
                return false;
            }
        }
        // What cannot be replayed now stays buffered for the next binding.
        drain_goal_thread_with_dispatch(state, thread, dispatch).await;
        return true;
    }
    // Any turn on a goal's thread that no Cupola message owns is the goal's,
    // whether or not its start was seen.
    if let (Some(thread), Some(turn)) = (thread_id.as_deref(), turn_id.as_deref()) {
        let conversation = match state.store.goal_conversation_for_thread(thread).await {
            Ok(conversation) => conversation,
            Err(_) => return false,
        };
        if let Some(conversation) = conversation {
            // While earlier events wait in the thread's buffer, a live one
            // queues behind them so the chat sees them in order.
            if !replaying {
                match state
                    .store
                    .pending_app_server_notifications_for_thread(thread)
                    .await
                {
                    Ok(pending) if pending.is_empty() => {}
                    Ok(_) => {
                        let identity_key =
                            app_server_notification_key(method, &notification, &params);
                        if enqueue_pending_app_server_notification(
                            state,
                            method,
                            &notification,
                            &params,
                            Some(thread),
                            Some(turn),
                            item_id.as_deref(),
                            identity_key.as_deref(),
                        )
                        .await
                        .is_err()
                        {
                            return false;
                        }
                        drain_goal_thread_with_dispatch(state, thread, dispatch).await;
                        return true;
                    }
                    Err(_) => return false,
                }
            }
            match state.store.goal_conversation_for_turn(thread, turn).await {
                Ok(Some(_)) => {}
                Ok(None) => match claim_goal_turn(state, &conversation, thread, turn).await {
                    Ok(true) => {
                        drain_goal_thread_with_dispatch(state, thread, dispatch).await;
                    }
                    Ok(false) => {}
                    Err(()) => return false,
                },
                Err(_) => return false,
            }
        }
    }
    let message = match (thread_id.as_deref(), turn_id.as_deref()) {
        (Some(thread_id), Some(turn_id)) => {
            state
                .store
                .message_for_codex_thread_and_turn(thread_id, turn_id)
                .await
        }
        (_, Some(turn_id)) => state.store.message_for_codex_turn(turn_id).await,
        (None, None) => Ok(None),
        (Some(_), None) => Ok(None),
    };
    let message = match message {
        Ok(message) => message,
        Err(_) => return false,
    };
    let mapped_child = match message.as_ref() {
        Some(message) => match state
            .store
            .subagent_ownership_for_conversation(&message.conversation_id)
            .await
        {
            Ok(child) => child.is_some(),
            Err(_) => return false,
        },
        None => false,
    };
    let has_child_seed = params.get("item").is_some_and(|item| {
        matches!(
            item.get("type").and_then(serde_json::Value::as_str),
            Some("subAgentActivity" | "collabAgentToolCall")
        ) && (item
            .get("agentThreadId")
            .and_then(serde_json::Value::as_str)
            .is_some()
            || item
                .get("receiverThreadIds")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|ids| !ids.is_empty()))
    });
    // A receipt already proves the ordinary parent conversation. Runtime
    // thread discovery is needed only for a mapped child, a structured child
    // seed, or a child notification that arrived before its first receipt.
    let ownership = if message.is_some() && !mapped_child && !has_child_seed {
        None
    } else {
        match subagents::observe(state, &notification).await {
            Ok(ownership) => ownership,
            Err(_) => return false,
        }
    };
    let goal_conversation = if message.is_none() && ownership.is_none() {
        match (thread_id.as_deref(), turn_id.as_deref()) {
            (Some(thread), Some(turn)) => {
                match state.store.goal_conversation_for_turn(thread, turn).await {
                    Ok(conversation) => conversation,
                    Err(_) => return false,
                }
            }
            _ => None,
        }
    } else {
        None
    };
    let conversation_id = message
        .as_ref()
        .map(|message| message.conversation_id.clone())
        .or_else(|| {
            ownership
                .as_ref()
                .map(|ownership| ownership.conversation_id.clone())
        })
        .or(goal_conversation.clone());
    if method == "item/tool/call"
        && params
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .is_some_and(thread_tools::registered)
    {
        let relock = dispatch.is_some();
        drop(dispatch.take());
        let result = thread_tools::handle(state, &notification, &params, message.as_ref()).await;
        if relock {
            *dispatch = Some(state.dispatch_lock.lock().await);
        }
        return result;
    }
    let is_message_bound_notification = method == "turn/completed"
        || method == "item/agentMessage/delta"
        || method == "item/completed"
        || method == "item/fileChange/patchUpdated";
    // A notification can race the dispatch response that stores the turn id.
    // Keep it durably until the turn is mapped to a Wonder conversation.
    if is_message_bound_notification
        && message.is_none()
        && ownership.is_none()
        && goal_conversation.is_none()
    {
        let identity_key = app_server_notification_key(method, &notification, &params);
        return enqueue_pending_app_server_notification(
            state,
            method,
            &notification,
            &params,
            thread_id.as_deref(),
            turn_id.as_deref(),
            item_id.as_deref(),
            identity_key.as_deref(),
        )
        .await
        .is_ok();
    }
    if method == "item/completed" {
        if let (Some(conversation), Some(turn), Some(item)) = (
            message
                .as_ref()
                .map(|message| message.conversation_id.as_str())
                .or_else(|| {
                    ownership
                        .as_ref()
                        .map(|ownership| ownership.conversation_id.as_str())
                }),
            turn_id.as_deref(),
            params.get("item"),
        ) {
            let authority = match projection_binding(state, &params).await {
                Ok(value) => value,
                Err(_) => return false,
            };
            let relock = dispatch.is_some();
            drop(dispatch.take());
            let normalized = tool_media::normalize(state, conversation, turn, item).await;
            if relock {
                *dispatch = Some(state.dispatch_lock.lock().await);
                if projection_binding(state, &params).await.ok().as_ref() != Some(&authority) {
                    return false;
                }
            }
            match normalized {
                Ok(item) => params["item"] = item,
                Err(_) => return false,
            }
        }
    }
    if params
        .pointer("/item/type")
        .and_then(serde_json::Value::as_str)
        == Some("userMessage")
    {
        if let Some(message) = message.as_ref() {
            match state
                .store
                .question_answer_messages(&message.conversation_id)
                .await
            {
                Ok(answers) if answers.iter().any(|m| m.id == message.id) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }
    let server_request_id = request_id(&notification, &params);
    let mut event = project_app_server_notification(method, &params, server_request_id.as_deref());
    let is_approval_request = approval_lifecycle::is_request(method);
    let notification_key = app_server_notification_key(method, &notification, &params);
    // Delta receipts commit with the append, including every original key in
    // an ingestion batch. Other notifications retain their existing recovery.
    let atomic_delta = method == "item/agentMessage/delta"
        && conversation_id.is_some()
        && thread_id.is_some()
        && turn_id.is_some()
        && item_id.is_some()
        && params.get("delta").and_then(extract_text).is_some();
    if let Some(notification_key) = notification_key.as_deref().filter(|_| !atomic_delta) {
        match state
            .store
            .claim_app_server_notification(notification_key, &now_ms().to_string())
            .await
        {
            Ok(true) => {}
            Ok(false) => return true,
            Err(_) => return false,
        }
    }
    let assistant_delta_key = notification_key
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if method == "turn/completed" {
        if let Some(message) = message.as_ref() {
            let completion = match (thread_id.as_deref(), turn_id.as_deref()) {
                (Some(thread_id), Some(turn_id)) => {
                    state
                        .store
                        .complete_messages_for_codex_thread_and_turn(thread_id, turn_id)
                        .await
                }
                (_, Some(turn_id)) => state.store.complete_messages_for_codex_turn(turn_id).await,
                (None, None) => state
                    .store
                    .complete_message_if_active(&message.id)
                    .await
                    .map(u64::from),
                (Some(_), None) => state
                    .store
                    .complete_message_if_active(&message.id)
                    .await
                    .map(u64::from),
            };
            match completion {
                Ok(_) => {}
                Err(_) => {
                    return fail_app_server_notification(
                        state,
                        method,
                        &notification,
                        &params,
                        thread_id.as_deref(),
                        turn_id.as_deref(),
                        item_id.as_deref(),
                        notification_key.as_deref(),
                        queue_failed_assistant,
                    )
                    .await;
                }
            }
            let run = match state.store.automation_run_for_message(&message.id).await {
                Ok(run) => run,
                Err(_) => {
                    return fail_app_server_notification(
                        state,
                        method,
                        &notification,
                        &params,
                        thread_id.as_deref(),
                        turn_id.as_deref(),
                        item_id.as_deref(),
                        notification_key.as_deref(),
                        queue_failed_assistant,
                    )
                    .await;
                }
            };
            if let Some(run) = run {
                let finished_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
                // Only a turn the provider reports completed finished the run.
                let failed = turn_completion_delivery_state(&params) != DeliveryState::Completed;
                if state
                    .store
                    .finish_automation_run(
                        &run.id,
                        if failed { "failed" } else { "completed" },
                        &finished_at,
                        failed.then_some(
                            "The run stopped before completing. Open its conversation for details.",
                        ),
                        Some(&message.id),
                    )
                    .await
                    .is_err()
                {
                    return fail_app_server_notification(
                        state,
                        method,
                        &notification,
                        &params,
                        thread_id.as_deref(),
                        turn_id.as_deref(),
                        item_id.as_deref(),
                        notification_key.as_deref(),
                        queue_failed_assistant,
                    )
                    .await;
                }
            }
        }
    }
    if method == "turn/completed" {
        if let (Some(message), Some(thread), Some(turn)) =
            (message.as_ref(), thread_id.as_deref(), turn_id.as_deref())
        {
            if let Some(reset) = usage_resume::usage_limit(&params) {
                usage_resume::note_limited_turn(state, message, thread, turn, reset).await;
            }
        }
        if let Some(message) = message.as_ref() {
            projects::release_to_desktop(state, &message.conversation_id);
        }
        let completed_assistant_messages =
            if let (Some(thread_id), Some(turn_id)) = (thread_id.as_deref(), turn_id.as_deref()) {
                let completed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
                match state
                    .store
                    .complete_assistant_messages_for_codex_thread_and_turn(
                        thread_id,
                        turn_id,
                        &completed_at,
                    )
                    .await
                {
                    Ok(messages) => messages,
                    Err(_) => {
                        return fail_app_server_notification(
                            state,
                            method,
                            &notification,
                            &params,
                            Some(thread_id),
                            Some(turn_id),
                            item_id.as_deref(),
                            notification_key.as_deref(),
                            queue_failed_assistant,
                        )
                        .await;
                    }
                }
            } else if let Some(turn_id) = turn_id.as_deref() {
                let completed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
                match state
                    .store
                    .complete_assistant_messages_for_codex_turn(turn_id, &completed_at)
                    .await
                {
                    Ok(messages) => messages,
                    Err(_) => {
                        return fail_app_server_notification(
                            state,
                            method,
                            &notification,
                            &params,
                            thread_id.as_deref(),
                            Some(turn_id),
                            item_id.as_deref(),
                            notification_key.as_deref(),
                            queue_failed_assistant,
                        )
                        .await;
                    }
                }
            } else {
                Vec::new()
            };
        // Some App Server versions finish a turn without item/completed. The
        // stored assistant rows are the source of truth for the missing
        // terminal events in that case.
        for assistant_message in completed_assistant_messages {
            if publish_event_with_context(
                state,
                WonderEvent::AssistantCompleted {
                    text: assistant_message.text,
                },
                EventContext {
                    request_id: request_id(&notification, &params),
                    conversation_id: Some(assistant_message.conversation_id.clone()),
                    message_id: message.as_ref().map(|message| message.id.clone()),
                    thread_id: Some(assistant_message.codex_thread_id),
                    turn_id: Some(assistant_message.codex_turn_id),
                    item_id: Some(assistant_message.item_id),
                    ..EventContext::default()
                },
            )
            .await
            .is_err()
            {
                return fail_app_server_notification(
                    state,
                    method,
                    &notification,
                    &params,
                    thread_id.as_deref(),
                    turn_id.as_deref(),
                    item_id.as_deref(),
                    notification_key.as_deref(),
                    queue_failed_assistant,
                )
                .await;
            }
        }
    }
    if method == "serverRequest/resolved" {
        if let Some(server_request_id) = server_request_id.as_deref() {
            if state
                .store
                .resolve_approval_by_server_request_id(
                    server_request_id,
                    "app_server_resolved",
                    &now_ms().to_string(),
                )
                .await
                .is_err()
            {
                return fail_app_server_notification(
                    state,
                    method,
                    &notification,
                    &params,
                    thread_id.as_deref(),
                    turn_id.as_deref(),
                    item_id.as_deref(),
                    notification_key.as_deref(),
                    queue_failed_assistant,
                )
                .await;
            }
        }
    }
    let assistant_completed = method == "item/completed"
        && matches!(
            project_app_server_notification(method, &params, None),
            Some(WonderEvent::AssistantCompleted { .. })
        );
    if assistant_completed {
        if let Some(conversation_id) = conversation_id.as_deref() {
            // Project threads have no Project inbox entry; mark the thread unread.
            if projects::is_project(state, conversation_id).await {
                let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
                if state
                    .store
                    .update_project_conversation(
                        conversation_id,
                        wonder_store::ProjectConversationPatch {
                            unread: Some(true),
                            ..Default::default()
                        },
                        &now,
                    )
                    .await
                    .is_err()
                    || state
                        .store
                        .touch_project_conversation(conversation_id, &now)
                        .await
                        .is_err()
                {
                    return fail_app_server_notification(
                        state,
                        method,
                        &notification,
                        &params,
                        thread_id.as_deref(),
                        turn_id.as_deref(),
                        item_id.as_deref(),
                        notification_key.as_deref(),
                        queue_failed_assistant,
                    )
                    .await;
                }
            }
        }
    }
    if let (Some(conversation_id), Some(thread_id), Some(turn_id), Some(item_id)) = (
        conversation_id.as_deref(),
        thread_id.as_deref(),
        turn_id.as_deref(),
        item_id.as_deref(),
    ) {
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        match method {
            "item/agentMessage/delta" => {
                if message.as_ref().is_some_and(|message| {
                    matches!(message.state.as_str(), "completed" | "interrupted")
                }) {
                    event = None;
                } else if let Some(delta) = params.get("delta").and_then(extract_text) {
                    // The text and its delta event commit together (adjacent
                    // sequences), so a client can apply the event directly.
                    let mut envelope = event.take().map(|event| {
                        event_envelope(
                            state,
                            event,
                            EventContext {
                                request_id: request_id(&notification, &params),
                                conversation_id: Some(conversation_id.to_owned()),
                                message_id: message.as_ref().map(|message| message.id.clone()),
                                thread_id: Some(thread_id.to_owned()),
                                turn_id: Some(turn_id.to_owned()),
                                item_id: Some(item_id.to_owned()),
                                approval_id: server_request_id.clone(),
                            },
                        )
                    });
                    let deltas = batch
                        .as_ref()
                        .filter(|b| !b.deltas.is_empty())
                        .map(|b| {
                            b.deltas
                                .iter()
                                .map(|(text, key)| (text.as_str(), key.as_str()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_else(|| vec![(delta.as_str(), assistant_delta_key.as_str())]);
                    let assistant = match state
                        .store
                        .upsert_assistant_deltas_with_event(
                            conversation_id,
                            thread_id,
                            turn_id,
                            item_id,
                            &deltas,
                            &now,
                            envelope.as_mut(),
                        )
                        .await
                    {
                        Ok((assistant, appended)) => {
                            if let Some(batch) = batch.as_mut() {
                                batch.consumed = true;
                            }
                            if let (true, Some(envelope)) = (appended, envelope) {
                                let _ = state.events.send(envelope);
                            }
                            assistant
                        }
                        Err(_) => {
                            return fail_app_server_notification(
                                state,
                                method,
                                &notification,
                                &params,
                                Some(thread_id),
                                Some(turn_id),
                                Some(item_id),
                                notification_key.as_deref(),
                                queue_failed_assistant,
                            )
                            .await;
                        }
                    };
                    if assistant.state != "completed" {
                        // Every delta used to rewrite the message's delivery
                        // state, journaling a change per token; write it once.
                        if let Some(message) = message.as_ref().filter(|message| {
                            message.state != "streaming"
                                || message.codex_thread_id.as_deref() != Some(thread_id)
                                || message.codex_turn_id.as_deref() != Some(turn_id)
                        }) {
                            if state
                                .store
                                .update_message_delivery(
                                    &message.id,
                                    "streaming",
                                    Some(thread_id),
                                    Some(turn_id),
                                )
                                .await
                                .is_err()
                            {
                                return fail_app_server_notification(
                                    state,
                                    method,
                                    &notification,
                                    &params,
                                    Some(thread_id),
                                    Some(turn_id),
                                    Some(item_id),
                                    notification_key.as_deref(),
                                    queue_failed_assistant,
                                )
                                .await;
                            }
                            if message.state != "streaming"
                                && publish_message_state_checked(
                                    state,
                                    message,
                                    DeliveryState::Streaming,
                                    Some(thread_id),
                                    Some(turn_id),
                                )
                                .await
                                .is_err()
                            {
                                return fail_app_server_notification(
                                    state,
                                    method,
                                    &notification,
                                    &params,
                                    Some(thread_id),
                                    Some(turn_id),
                                    Some(item_id),
                                    notification_key.as_deref(),
                                    queue_failed_assistant,
                                )
                                .await;
                            }
                        }
                    }
                }
            }
            "item/completed" if assistant_completed => {
                let text = params
                    .get("item")
                    .and_then(extract_text)
                    .unwrap_or_default();
                match state
                    .store
                    .complete_assistant_message(
                        conversation_id,
                        thread_id,
                        turn_id,
                        item_id,
                        &text,
                        &now,
                    )
                    .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) => event = None,
                    Err(_) => {
                        return fail_app_server_notification(
                            state,
                            method,
                            &notification,
                            &params,
                            Some(thread_id),
                            Some(turn_id),
                            Some(item_id),
                            notification_key.as_deref(),
                            queue_failed_assistant,
                        )
                        .await;
                    }
                }
            }
            _ => {}
        }
    }
    if method == "item/completed" {
        if let (Some(conversation_id), Some(thread), Some(turn), Some(item)) = (
            conversation_id.as_deref(),
            thread_id.as_deref(),
            turn_id.as_deref(),
            params.get("item"),
        ) {
            if let (Some(id), Some(questions)) = (
                item.get("id").and_then(serde_json::Value::as_str),
                questions::async_questions(item),
            ) {
                if state
                    .store
                    .save_async_question(
                        conversation_id,
                        thread,
                        turn,
                        id,
                        &questions.to_string(),
                        now_ms().saturating_add(questions::OPTIONAL_QUESTION_TTL_MS) as i64,
                    )
                    .await
                    .is_err()
                {
                    if let Some(key) = notification_key.as_deref() {
                        let _ = state.store.release_app_server_notification(key).await;
                    }
                    return false;
                }
            }
        }
    }
    if let Some(thread) = thread_id.as_deref() {
        proposed_file_changes::observe(thread, method, &params);
    }
    if let Some(server_request_id) = server_request_id.as_deref() {
        if is_approval_request {
            let mut stored_params = params.clone();
            // Codex's request names its file change item; the change shown is
            // that item's proposal, kept with this request (and its nonce).
            if method == "item/fileChange/requestApproval" && stored_params.get("changes").is_none()
            {
                if let Some(changes) = thread_id
                    .as_deref()
                    .zip(item_id.as_deref())
                    .and_then(|(thread, item)| proposed_file_changes::get(thread, item))
                {
                    stored_params["changes"] = changes.into();
                }
            }
            if stored_params.get("supportsDenyMessage").is_some() {
                let supported = method == "item/commandExecution/requestApproval"
                    && stored_params["kind"] == "command"
                    && stored_params["supportsDenyMessage"] == true
                    && stored_params["_wonderRuntimeId"]
                        .as_str()
                        .and_then(|id| state.ingestion.runtime_route(id))
                        .is_some_and(|route| {
                            route.family.provider().capabilities.deny_with_instruction
                        });
                stored_params["supportsDenyMessage"] = serde_json::json!(supported);
            }
            if method == "item/tool/requestUserInput" {
                stored_params["_wonderSupportsQuestionReply"] = serde_json::json!(stored_params
                    ["_wonderRuntimeId"]
                    .as_str()
                    .and_then(|id| state.ingestion.runtime_route(id))
                    .is_some_and(|route| route
                        .family
                        .provider()
                        .capabilities
                        .structured_questions));
                if let Some(deadline) = questions::optional_deadline(&params, now_ms()) {
                    stored_params["_wonderQuestionDeadlineMs"] = serde_json::json!(deadline);
                }
            }
            if state
                .store
                .insert_pending_approval(
                    server_request_id,
                    method,
                    &serde_json::to_string(&stored_params).unwrap_or_else(|_| "{}".into()),
                    &now_ms().to_string(),
                )
                .await
                .is_err()
            {
                return fail_app_server_notification(
                    state,
                    method,
                    &notification,
                    &params,
                    thread_id.as_deref(),
                    turn_id.as_deref(),
                    item_id.as_deref(),
                    notification_key.as_deref(),
                    queue_failed_assistant,
                )
                .await;
            }
        }
    }
    if method == "turn/completed" {
        computer_runtime::turn_ended(state, &params);
    }
    // The turn's saved diff is published as an item just before its terminal
    // state so history lifts it onto the turn as `editedFiles`.
    let turn_diff_item = (method == "turn/completed")
        .then(|| thread_id.as_deref().zip(turn_id.as_deref()))
        .flatten()
        .and_then(|(thread, turn)| {
            turn_diff::take(thread, turn).map(|diff| turn_diff::item(turn, &diff))
        });
    let task_item = (method == "turn/plan/updated")
        .then(|| {
            turn_id
                .as_deref()
                .and_then(|turn| task_checklist::notification_item(turn, &params))
        })
        .flatten();
    // Preserve the complete App Server item in the event stream as a
    // loss-minimized projection. Older clients still see a normal activity
    // event; newer clients consume its thread_item_upsert category.
    if let (Some(turn_id), Some(item)) = (
        turn_id.as_deref(),
        task_item.as_ref().or(turn_diff_item.as_ref()).or_else(|| {
            params.get("item").filter(|_| {
                matches!(
                    method,
                    "item/started"
                        | "item/completed"
                        | "item/commandExecution/started"
                        | "item/commandExecution/completed"
                        | "item/fileChange/started"
                        | "item/fileChange/completed"
                )
            })
        }),
    ) {
        let workspace = match conversation_id.as_deref() {
            Some(conversation) => match state
                .store
                .conversation_execution_directory(conversation)
                .await
            {
                Ok(value) => value,
                Err(_) => return false,
            },
            None => None,
        };
        if let Some(detail) = thread_item_upsert_detail_with_authority(
            turn_id,
            item,
            if task_item.is_some() {
                "completed"
            } else {
                app_server_item_lifecycle_state(method, item)
            },
            2,
            workspace.as_deref(),
        ) {
            if publish_event_with_context(
                state,
                WonderEvent::Activity {
                    category: "thread_item_upsert".into(),
                    state: "updated".into(),
                    detail: Some(detail),
                },
                EventContext {
                    request_id: request_id(&notification, &params),
                    conversation_id: conversation_id.clone(),
                    message_id: message.as_ref().map(|message| message.id.clone()),
                    thread_id: thread_id.clone(),
                    turn_id: Some(turn_id.to_owned()),
                    item_id: item_id.clone(),
                    approval_id: server_request_id.clone(),
                },
            )
            .await
            .is_err()
            {
                return false;
            }
        }
    }
    if let Some(event) = event {
        if publish_event_with_context(
            state,
            event,
            EventContext {
                request_id: request_id(&notification, &params),
                conversation_id: conversation_id.clone(),
                message_id: message.as_ref().map(|message| message.id.clone()),
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                item_id: item_id.clone(),
                approval_id: server_request_id,
            },
        )
        .await
        .is_err()
        {
            return fail_app_server_notification(
                state,
                method,
                &notification,
                &params,
                thread_id.as_deref(),
                turn_id.as_deref(),
                item_id.as_deref(),
                notification_key.as_deref(),
                queue_failed_assistant,
            )
            .await;
        }
    }
    true
}

#[cfg(test)]
async fn drain_pending_app_server_notifications(state: &AppState, thread_id: &str, turn_id: &str) {
    let mut dispatch = Some(state.dispatch_lock.lock().await);
    drain_pending_with_dispatch(state, thread_id, turn_id, &mut dispatch).await;
}

async fn drain_pending_with_dispatch<'a>(
    state: &'a AppState,
    thread_id: &str,
    turn_id: &str,
    dispatch: &mut Option<tokio::sync::MutexGuard<'a, ()>>,
) {
    // Acquire lane before lifecycle lock, including receipt-triggered replay.
    // The durable head may already be preparing and need that lock to finish.
    let relock = dispatch.is_some();
    drop(dispatch.take());
    let _lane = state.ingestion.projection_lane(thread_id).await;
    if relock {
        *dispatch = Some(state.dispatch_lock.lock().await);
    }
    drain_pending_locked(state, thread_id, turn_id, dispatch).await;
}

async fn drain_pending_locked<'a>(
    state: &'a AppState,
    thread_id: &str,
    turn_id: &str,
    dispatch: &mut Option<tokio::sync::MutexGuard<'a, ()>>,
) -> bool {
    let pending = match state
        .store
        .pending_app_server_notifications_for_thread_and_turn(thread_id, turn_id)
        .await
    {
        Ok(pending) => pending,
        Err(_) => return false,
    };
    for pending in pending {
        let Ok(notification) =
            serde_json::from_str::<serde_json::Value>(&pending.notification_json)
        else {
            continue;
        };
        if apply_app_server_notification_locked(state, notification, false, true, None, dispatch)
            .await
        {
            let _ = state
                .store
                .delete_pending_app_server_notification(&pending.id)
                .await;
        } else {
            return false;
        }
    }
    true
}

/// Claims a turn for the goal's chat unless a Cupola message owns it or one
/// is still being submitted there (its receipt decides). True when claimed.
async fn claim_goal_turn(
    state: &AppState,
    conversation: &str,
    thread: &str,
    turn: &str,
) -> Result<bool, ()> {
    if state
        .store
        .message_for_codex_thread_and_turn(thread, turn)
        .await
        .map_err(|_| ())?
        .is_some()
        || state
            .store
            .conversation_submitting(conversation)
            .await
            .map_err(|_| ())?
    {
        return Ok(false);
    }
    state
        .store
        .set_goal_turn(conversation, thread, turn)
        .await
        .map_err(|_| ())?;
    Ok(true)
}

/// Replays what arrived for a goal thread before its chat was known: turns
/// the runtime started on its own (no Cupola receipt) join the goal's chat.
/// A turn Cupola is still submitting is left to that dispatch's own drain.
/// False keeps the rest for a later attempt.
pub(crate) async fn drain_goal_thread(state: &AppState, thread: &str) -> bool {
    let _lane = state.ingestion.projection_lane(thread).await;
    let mut dispatch = Some(state.dispatch_lock.lock().await);
    drain_goal_thread_with_dispatch(state, thread, &mut dispatch).await
}

async fn drain_goal_thread_with_dispatch<'a>(
    state: &'a AppState,
    thread: &str,
    dispatch: &mut Option<tokio::sync::MutexGuard<'a, ()>>,
) -> bool {
    let conversation = match state.store.goal_conversation_for_thread(thread).await {
        Ok(Some(conversation)) => conversation,
        Ok(None) => return true,
        Err(_) => return false,
    };
    let pending = match state
        .store
        .pending_app_server_notifications_for_thread(thread)
        .await
    {
        Ok(pending) if pending.is_empty() => return true,
        Ok(pending) => pending,
        Err(_) => return false,
    };
    let Ok(submitting) = state.store.conversation_submitting(&conversation).await else {
        return false;
    };
    for pending in pending {
        if matches!(
            pending.method.as_str(),
            "thread/goal/updated" | "thread/goal/cleared"
        ) {
            // The binding these carried is already written; the turn an
            // update names is the goal's unless a Cupola message owns it.
            if let Some(turn) = pending.turn_id.as_deref() {
                if claim_goal_turn(state, &conversation, thread, turn)
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            if state
                .store
                .delete_pending_app_server_notification(&pending.id)
                .await
                .is_err()
            {
                return false;
            }
            continue;
        }
        // Without a turn it cannot join the goal; it waits for its receipt.
        let Some(turn) = pending.turn_id.as_deref() else {
            continue;
        };
        let owned = match state
            .store
            .message_for_codex_thread_and_turn(thread, turn)
            .await
        {
            Ok(message) => message.is_some(),
            Err(_) => return false,
        };
        let claimed = match state.store.goal_conversation_for_turn(thread, turn).await {
            Ok(claimed) => claimed.is_some(),
            Err(_) => return false,
        };
        if !owned && !claimed {
            if submitting {
                continue;
            }
            if state
                .store
                .set_goal_turn(&conversation, thread, turn)
                .await
                .is_err()
            {
                return false;
            }
        }
        let Ok(notification) =
            serde_json::from_str::<serde_json::Value>(&pending.notification_json)
        else {
            continue;
        };
        if !Box::pin(apply_app_server_notification_locked(
            state,
            notification,
            false,
            true,
            None,
            dispatch,
        ))
        .await
        {
            return false;
        }
        if state
            .store
            .delete_pending_app_server_notification(&pending.id)
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

fn artifact_mime_type(name: &str, bytes: &[u8]) -> Option<&'static str> {
    let extension = FsPath::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("pdf")
            if bytes.starts_with(b"%PDF-") && bytes.windows(5).any(|window| window == b"%%EOF") =>
        {
            Some("application/pdf")
        }
        Some("md" | "markdown" | "mdx") if std::str::from_utf8(bytes).is_ok() => {
            Some("text/markdown")
        }
        Some("html" | "htm" | "xhtml") if std::str::from_utf8(bytes).is_ok() => Some("text/html"),
        Some("txt" | "log") if std::str::from_utf8(bytes).is_ok() => Some("text/plain"),
        Some("rs") if std::str::from_utf8(bytes).is_ok() => Some("text/x-rust"),
        Some("ts" | "tsx") if std::str::from_utf8(bytes).is_ok() => Some("text/typescript"),
        Some("js" | "jsx" | "mjs" | "cjs") if std::str::from_utf8(bytes).is_ok() => {
            Some("text/javascript")
        }
        Some("json" | "jsonc" | "json5") if std::str::from_utf8(bytes).is_ok() => {
            Some("application/json")
        }
        Some("py") if std::str::from_utf8(bytes).is_ok() => Some("text/x-python"),
        Some("css" | "scss" | "less") if std::str::from_utf8(bytes).is_ok() => Some("text/css"),
        Some("xml") if std::str::from_utf8(bytes).is_ok() => Some("application/xml"),
        Some("yaml" | "yml") if std::str::from_utf8(bytes).is_ok() => Some("application/yaml"),
        Some("toml" | "ini") if std::str::from_utf8(bytes).is_ok() => Some("text/plain"),
        Some("sh" | "bash") if std::str::from_utf8(bytes).is_ok() => Some("text/x-shellscript"),
        Some("sql") if std::str::from_utf8(bytes).is_ok() => Some("application/sql"),
        Some("png") if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => Some("image/png"),
        Some("jpg" | "jpeg") if bytes.starts_with(b"\xff\xd8\xff") => Some("image/jpeg"),
        Some("gif") if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") => {
            Some("image/gif")
        }
        Some("webp")
            if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) =>
        {
            Some("image/webp")
        }
        _ => None,
    }
}

fn sanitized_relative_path(path: &str, workspace: &str) -> Option<String> {
    let path = FsPath::new(path);
    let relative = if path.is_absolute() {
        path.strip_prefix(workspace).ok()?
    } else {
        path
    };
    let mut components = Vec::new();
    for component in relative.components() {
        let component = component.as_os_str().to_str()?;
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        components.push(component);
    }
    (!components.is_empty()).then(|| components.join("/"))
}

async fn publish_message_state(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    delivery_state: DeliveryState,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
) {
    let _ = publish_event_with_context(
        state,
        WonderEvent::MessageState {
            state: delivery_state,
        },
        EventContext {
            conversation_id: Some(message.conversation_id.clone()),
            message_id: Some(message.id.clone()),
            thread_id: thread_id
                .map(str::to_owned)
                .or_else(|| message.codex_thread_id.clone()),
            turn_id: turn_id
                .map(str::to_owned)
                .or_else(|| message.codex_turn_id.clone()),
            ..EventContext::default()
        },
    )
    .await;
}

async fn publish_message_state_checked(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    delivery_state: DeliveryState,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    publish_event_with_context(
        state,
        WonderEvent::MessageState {
            state: delivery_state,
        },
        EventContext {
            conversation_id: Some(message.conversation_id.clone()),
            message_id: Some(message.id.clone()),
            thread_id: thread_id
                .map(str::to_owned)
                .or_else(|| message.codex_thread_id.clone()),
            turn_id: turn_id
                .map(str::to_owned)
                .or_else(|| message.codex_turn_id.clone()),
            ..EventContext::default()
        },
    )
    .await
    .map(|_| ())
}

fn string_field(params: &serde_json::Value, name: &str) -> Option<String> {
    params
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn steer_response_turn_id(response: &wonder_app_server::RpcResponse) -> Option<String> {
    response
        .result
        .as_ref()
        .and_then(|result| result.get("turnId"))
        .and_then(serde_json::Value::as_str)
        .filter(|turn_id| !turn_id.is_empty())
        .map(str::to_owned)
}

/// App Server item payloads have changed shape across compatible releases:
/// some expose `text`, while others put assistant output in a content/parts
/// array. Keep the protocol boundary tolerant and return only display text.
fn extract_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return (!text.is_empty()).then(|| text.to_owned());
    }
    if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) {
        return (!text.is_empty()).then(|| text.to_owned());
    }
    for key in ["content", "parts", "message", "output", "delta"] {
        if let Some(values) = value.get(key).and_then(serde_json::Value::as_array) {
            let text = values
                .iter()
                .filter_map(extract_text)
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() {
                return Some(text);
            }
        } else if let Some(nested) = value.get(key).and_then(extract_text) {
            return Some(nested);
        }
    }
    None
}

fn request_id(notification: &serde_json::Value, params: &serde_json::Value) -> Option<String> {
    params
        .get("requestId")
        .and_then(value_id)
        .or_else(|| notification.get("id").and_then(value_id))
        .map(|id| {
            match notification
                .get("_wonderRuntimeId")
                .or_else(|| params.get("_wonderRuntimeId"))
                .and_then(serde_json::Value::as_str)
            {
                Some(runtime) => format!("{runtime}:{id}"),
                None => id,
            }
        })
}

fn app_server_notification_key(
    method: &str,
    notification: &serde_json::Value,
    params: &serde_json::Value,
) -> Option<String> {
    let explicit_id = ["eventId", "notificationId", "deltaId"]
        .iter()
        .find_map(|key| params.get(*key).and_then(value_id))
        .or_else(|| {
            ["eventId", "notificationId"]
                .iter()
                .find_map(|key| notification.get(*key).and_then(value_id))
        });
    let item_id = params.get("itemId").and_then(value_id).or_else(|| {
        params
            .get("item")
            .and_then(|item| item.get("id"))
            .and_then(value_id)
    });
    let sequence = ["sequence", "index", "deltaIndex", "offset"]
        .iter()
        .find_map(|key| params.get(*key).and_then(value_id));
    let identity = explicit_id
        .or_else(|| match method {
            "item/agentMessage/delta" => item_id
                .zip(sequence)
                .map(|(item, sequence)| format!("item:{item}:sequence:{sequence}")),
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => item_id
                .zip(sequence)
                .map(|(item, sequence)| format!("item:{item}:sequence:{sequence}")),
            "item/fileChange/patchUpdated" => item_id.map(|item| {
                let changes = params.get("changes").cloned().unwrap_or_default();
                let changes_hash = hex::encode(Sha256::digest(
                    serde_json::to_vec(&changes).unwrap_or_default(),
                ));
                format!("item:{item}:changes:{changes_hash}")
            }),
            "item/completed" => item_id.map(|item| format!("item:{item}:completed")),
            "thread/compacted" => params
                .get("turnId")
                .and_then(value_id)
                .map(|turn| format!("turn:{turn}:compacted")),
            "turn/completed" => params
                .get("turnId")
                .and_then(value_id)
                .or_else(|| {
                    params
                        .get("turn")
                        .and_then(|turn| turn.get("id"))
                        .and_then(value_id)
                })
                .map(|turn| format!("turn:{turn}:completed")),
            "serverRequest/resolved"
            | "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
            | "mcpServer/elicitation/request"
            | "item/tool/call" => {
                request_id(notification, params).map(|request| format!("request:{request}"))
            }
            _ => None,
        })
        .or_else(|| {
            notification
                .get("_wonderInboxId")
                .and_then(value_id)
                .map(|id| format!("inbox:{id}"))
        })?;
    let identity = serde_json::json!({ "method": method, "identity": identity, "thread": notification_thread_id(params), "turn": notification_turn_id(params) });
    Some(hex::encode(Sha256::digest(
        serde_json::to_vec(&identity).unwrap_or_default(),
    )))
}

fn value_id(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|id| id.to_string()))
}

fn project_app_server_notification(
    method: &str,
    params: &serde_json::Value,
    server_request_id: Option<&str>,
) -> Option<WonderEvent> {
    match method {
        "item/agentMessage/delta" => params
            .get("delta")
            .and_then(extract_text)
            .map(|text| WonderEvent::AssistantDelta { text }),
        "item/commandExecution/outputDelta" => Some(WonderEvent::Activity {
            category: "command".into(),
            state: "working".into(),
            detail: Some("Output updated".into()),
        }),
        "item/fileChange/outputDelta" => Some(WonderEvent::Activity {
            category: "file change".into(),
            state: "working".into(),
            detail: Some("Output updated".into()),
        }),
        "item/started" => params
            .get("item")
            .and_then(|item| project_typed_item_activity(item, "running")),
        "item/completed" => {
            let item = params.get("item")?;
            if item
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|item_type| item_type == "agentMessage")
            {
                // A valid agent completion may have no visible text (for
                // example, a turn that only produced an artifact). Preserve
                // the terminal event so the client can leave streaming state.
                Some(WonderEvent::AssistantCompleted {
                    text: extract_text(item).unwrap_or_default(),
                })
            } else {
                project_typed_item_activity(item, "completed")
            }
        }
        "turn/completed" => Some(WonderEvent::MessageState {
            state: turn_completion_delivery_state(params),
        }),
        "thread/compacted" => Some(WonderEvent::Activity {
            category: "thread/compacted".into(),
            state: "completed".into(),
            detail: None,
        }),
        "item/commandExecution/started" => Some(WonderEvent::Activity {
            category: "command".into(),
            state: "started".into(),
            detail: None,
        }),
        "item/commandExecution/completed" => Some(WonderEvent::Activity {
            category: "command".into(),
            state: "completed".into(),
            detail: None,
        }),
        "item/fileChange/started" => Some(WonderEvent::Activity {
            category: "file change".into(),
            state: "started".into(),
            detail: None,
        }),
        "item/fileChange/completed" => Some(WonderEvent::Activity {
            category: "file change".into(),
            state: "completed".into(),
            detail: None,
        }),
        "item/mcpToolCall/progress" => Some(WonderEvent::Activity {
            category: "MCP tool".into(),
            state: "working".into(),
            detail: None,
        }),
        "skills/changed" => Some(WonderEvent::Activity {
            category: "codex_skills".into(),
            state: "changed".into(),
            detail: None,
        }),
        "turn/plan/updated" => None,
        "item/plan/delta" => Some(WonderEvent::Activity {
            category: "plan".into(),
            state: "updated".into(),
            detail: None,
        }),
        "item/reasoning/summaryPartAdded"
        | "item/reasoning/summaryTextDelta"
        | "item/reasoning/textDelta" => Some(WonderEvent::Activity {
            category: "reasoning".into(),
            state: "working".into(),
            detail: None,
        }),
        "model/rerouted" => Some(WonderEvent::Activity {
            category: "model".into(),
            state: "rerouted".into(),
            detail: None,
        }),
        "serverRequest/resolved" => {
            server_request_id.map(|request_id| WonderEvent::ApprovalResolved {
                request_id: request_id.into(),
                decision: "app_server_resolved".into(),
            })
        }
        "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/permissions/requestApproval"
        | "item/tool/requestUserInput"
        | "mcpServer/elicitation/request"
        | "item/tool/call" => server_request_id.map(|request_id| WonderEvent::ApprovalOpened {
            request_id: request_id.into(),
        }),
        _ => Some(WonderEvent::Activity {
            category: "App Server".into(),
            state: "unhandled".into(),
            // Retain only the bounded method name. Never persist the raw
            // notification payload when a newer protocol method appears.
            detail: Some(method.chars().take(120).collect()),
        }),
    }
}

fn project_typed_item_activity(item: &serde_json::Value, state: &str) -> Option<WonderEvent> {
    let item_type = item.get("type").and_then(serde_json::Value::as_str)?;
    let (category, detail) = match item_type {
        "commandExecution" => (
            "command",
            item.get("command")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        ),
        "fileChange" => (
            "file change",
            item.get("changes")
                .and_then(serde_json::Value::as_array)
                .map(|changes| {
                    changes
                        .iter()
                        .filter_map(|change| change.get("path").and_then(serde_json::Value::as_str))
                        .take(4)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|detail| !detail.is_empty()),
        ),
        "mcpToolCall" | "dynamicToolCall" => (
            "MCP tool",
            item.get("tool")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        ),
        "plan" => ("plan", item.get("text").and_then(extract_text)),
        "reasoning" => ("reasoning", None),
        _ => return None,
    };
    Some(WonderEvent::Activity {
        category: category.into(),
        state: state.into(),
        detail,
    })
}

fn thread_item_upsert_detail(
    turn_id: &str,
    item: &serde_json::Value,
    state: &str,
    workspace: Option<&str>,
) -> Option<String> {
    let authority = if item.get("status").is_some() || item.get("state").is_some() {
        1
    } else {
        0
    };
    thread_item_upsert_detail_with_authority(turn_id, item, state, authority, workspace)
}

fn app_server_item_lifecycle_state(method: &str, item: &serde_json::Value) -> &'static str {
    if !method.ends_with("completed") {
        return "started";
    }
    item.get("status")
        .or_else(|| item.get("state"))
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("type").and_then(serde_json::Value::as_str))
        })
        .map(history::thread_item_state)
        .filter(|state| matches!(*state, "failed" | "interrupted"))
        .unwrap_or("completed")
}

fn thread_item_upsert_detail_with_authority(
    turn_id: &str,
    item: &serde_json::Value,
    state: &str,
    lifecycle_authority: u8,
    workspace: Option<&str>,
) -> Option<String> {
    let item_id = item.get("id").and_then(serde_json::Value::as_str)?;
    let sanitized = history::sanitize_typed_item_in_state(item, workspace, state);
    let mut detail = serde_json::json!({
        "turnId": turn_id,
        "state": thread_item_state(state),
        "lifecycleAuthority": lifecycle_authority,
        "itemId": item_id,
    });
    if lifecycle_authority > 0 {
        // A live upsert also carries the row a snapshot would show for this
        // event alone (its type and display text), so a client can apply it
        // to the open conversation without reproducing the host projection.
        let raw_type = item
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        detail["itemType"] = if history::known_thread_item_type(raw_type) {
            raw_type
        } else {
            "unknown"
        }
        .into();
        if let Some(text) = history::typed_item_display_text(&sanitized) {
            detail["text"] = text.into();
        }
    }
    detail["item"] = sanitized;
    Some(detail.to_string())
}

fn turn_completion_delivery_state(params: &serde_json::Value) -> DeliveryState {
    let status = params
        .get("turn")
        .and_then(|turn| turn.get("status"))
        .and_then(|status| {
            status
                .as_str()
                .or_else(|| status.get("type").and_then(serde_json::Value::as_str))
        })
        .or_else(|| params.get("status").and_then(serde_json::Value::as_str));
    // Codex's TurnStatus is completed|interrupted|failed|inProgress and both
    // bridges send one of the first three. A missing or newer status is not
    // evidence of a finished reply: the turn ended with an outcome Wonder
    // cannot read, shown as unknown and announced as needing a look.
    match status {
        Some("completed") => DeliveryState::Completed,
        Some("failed" | "error") => DeliveryState::Failed,
        Some("interrupted" | "aborted" | "cancelled") => DeliveryState::Interrupted,
        _ => DeliveryState::Uncertain,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RouteAuth {
    Public,
    PublicPairing,
    ProtectedApi,
    NotFound,
}

fn route_auth(path: &str) -> RouteAuth {
    if matches!(path, "/healthz" | "/readyz" | "/pair") {
        RouteAuth::Public
    } else if is_public_pairing_path(path)
        || path == "/api/v1/rpc/sealed"
        || path == "/api/v1/host/proof"
    {
        // Origin-checked only: a sealed RPC client proves its session inside
        // the channel, so no credential crosses a relay in the clear, and a
        // phone checks the host proof before it sends a session at all.
        RouteAuth::PublicPairing
    } else if path.starts_with("/api/") {
        RouteAuth::ProtectedApi
    } else {
        RouteAuth::NotFound
    }
}

async fn require_loopback_capability(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let auth = route_auth(request.uri().path());
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let authority = request
        .uri()
        .authority()
        .map(|authority| authority.as_str());
    // A relay's edge forwards the Origin it was sent, so it names the route too.
    let origin_host = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(|origin| origin.strip_prefix("https://"));
    // A call routed from inside a sealed channel was decrypted here; on the
    // network the relay address carries the sealed channel only.
    let sealed_call = request
        .extensions()
        .get::<rpc::server::SealedCall>()
        .is_some();
    if !sealed_call
        && relay::refuses_on_relay(
            &state.data_dir,
            &[host, authority, origin_host],
            request.uri().path(),
        )
    {
        let mut response = StatusCode::NOT_FOUND.into_response();
        apply_security_headers(&mut response);
        return response;
    }
    if auth == RouteAuth::NotFound {
        let mut response = StatusCode::NOT_FOUND.into_response();
        apply_security_headers(&mut response);
        return response;
    }
    if auth == RouteAuth::Public {
        let mut response = next.run(request).await;
        apply_security_headers(&mut response);
        return response;
    }
    if auth == RouteAuth::PublicPairing {
        let Some(origins) = request_trusted_origins(&state, RequestRoute::of(&request)).await
        else {
            return (StatusCode::SERVICE_UNAVAILABLE, "Cupola is still starting").into_response();
        };
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok());
        let Some(matched) = origins
            .into_iter()
            .find(|trusted| origin == Some(trusted.origin.as_str()))
        else {
            return (StatusCode::FORBIDDEN, "origin rejected").into_response();
        };
        request
            .extensions_mut()
            .insert(MatchedOrigin(matched.origin));
        let mut response = next.run(request).await;
        apply_security_headers(&mut response);
        return response;
    }
    let provided = request
        .headers()
        .get("x-wonder-loopback-capability")
        .and_then(|value| value.to_str().ok());
    if (request.uri().path().starts_with("/api/v1/pairing/offers")
        || request.uri().path().starts_with("/api/v1/pairing/pending"))
        && provided != Some(state.loopback_capability.as_str())
    {
        return (
            StatusCode::FORBIDDEN,
            "Confirm device connections on your computer.",
        )
            .into_response();
    }
    if provided == Some(state.loopback_capability.as_str()) {
        request.extensions_mut().insert(LocalOwnerAuthority);
    } else {
        let Some(origins) = request_trusted_origins(&state, RequestRoute::of(&request)).await
        else {
            return (StatusCode::SERVICE_UNAVAILABLE, "Cupola is still starting").into_response();
        };
        let Some(matched) = matching_trusted_origin(&request, origins) else {
            return (StatusCode::FORBIDDEN, "origin rejected").into_response();
        };
        request
            .extensions_mut()
            .insert(MatchedOrigin(matched.origin));
        let session = cookie_value(request.headers(), "__Host-wonder_session");
        let csrf = request
            .headers()
            .get("x-wonder-csrf")
            .and_then(|value| value.to_str().ok());
        let needs_csrf = request.method() != axum::http::Method::GET;
        if needs_csrf && csrf.is_none() {
            return (StatusCode::UNAUTHORIZED, "Cupola CSRF token required").into_response();
        }
        let Some(session) = session else {
            return (StatusCode::UNAUTHORIZED, "Cupola session required").into_response();
        };
        let authenticated_device = {
            let pairing = state.pairing.lock().await;
            match pairing.verify_session(&session, csrf, now_ms()) {
                Ok(device_id) => AuthenticatedDevice {
                    device_id,
                    session_binding: csrf.unwrap_or_default().to_owned(),
                },
                Err(_) => {
                    return (StatusCode::UNAUTHORIZED, "Cupola session required").into_response()
                }
            }
        };
        if state
            .store
            .touch_owner_device(&authenticated_device.device_id, &now_ms().to_string())
            .await
            .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "device state unavailable",
            )
                .into_response();
        }
        request.extensions_mut().insert(authenticated_device);
    }
    let path = request.uri().path();
    if request.method() == axum::http::Method::POST
        && (path.ends_with("/messages")
            || path.ends_with("/retry")
            || path.ends_with("/steer")
            || (path.starts_with("/api/v1/automations/") && path.ends_with("/run")))
    {
        let readiness = state.ingestion.readiness(&state.store).await;
        if !readiness.ready {
            return (StatusCode::SERVICE_UNAVAILABLE, readiness.detail).into_response();
        }
    }
    request.extensions_mut().insert(OwnerAuthority);
    let mut response = next.run(request).await;
    apply_security_headers(&mut response);
    response
}

fn apply_security_headers(response: &mut Response) {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert("content-security-policy", axum::http::HeaderValue::from_static("default-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; connect-src 'self'; img-src 'self' data: blob:"));
    response.headers_mut().insert(
        "x-frame-options",
        axum::http::HeaderValue::from_static("DENY"),
    );
    // Types come from the daemon (file previews from their extension); a
    // client must not reinterpret them from the bytes.
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
}

fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .map(str::trim)
                .find_map(|cookie| cookie.strip_prefix(&format!("{name}=")))
        })
        .map(str::to_owned)
}

fn is_public_pairing_path(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/pairing/claim"
            | "/api/v1/pairing/code"
            | "/api/v1/pairing/session"
            | "/api/v1/pairing/session/refresh-challenge"
    )
}

/// How a phone reaches this host. Each route has its own origin; a session
/// challenge names the one the phone actually used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OriginKind {
    Tailscale,
    Home,
    Relay,
    /// An owner-supplied `--origin` that is not a tailnet name.
    Custom,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct TrustedOrigin {
    kind: OriginKind,
    origin: String,
}

impl TrustedOrigin {
    fn primary(origin: &str) -> Self {
        let tailnet = origin
            .strip_prefix("https://")
            .map(|host_port| host_port.split(':').next().unwrap_or_default())
            .is_some_and(|host| host.ends_with(".ts.net"));
        Self {
            kind: if tailnet {
                OriginKind::Tailscale
            } else {
                OriginKind::Custom
            },
            origin: origin.to_owned(),
        }
    }
}

/// The trusted origin a remote request matched, so challenges echo its route.
#[derive(Clone, Debug)]
struct MatchedOrigin(String);

/// The primary origin: pairing offers, host status and MCP use this one.
async fn current_public_origin(state: &AppState) -> Option<String> {
    primary_origin(&current_trusted_origins(state).await)
}

/// The relay carries only sealed RPC, so it is never the primary origin.
fn primary_origin(trusted: &[TrustedOrigin]) -> Option<String> {
    trusted
        .iter()
        .find(|trusted| trusted.kind != OriginKind::Relay)
        .map(|trusted| trusted.origin.clone())
}

/// Every origin a paired phone may use, primary first.
async fn current_trusted_origins(state: &AppState) -> Vec<TrustedOrigin> {
    let mut origins = if let Some(path) = state.public_origin_file.as_deref() {
        match tokio::fs::read_to_string(path).await {
            Ok(contents) => trusted_origins_from_status_log(&contents),
            Err(_) => Vec::new(),
        }
    } else {
        let origin = state.public_origin.trim();
        if is_valid_public_origin(origin) {
            vec![TrustedOrigin::primary(origin)]
        } else {
            Vec::new()
        }
    };
    // The daemon's own relay tunnel, while it is on (`relay.json`).
    if let Some(origin) = relay::relay_origin(&state.data_dir) {
        if is_valid_public_origin(&origin) && !origins.iter().any(|known| known.origin == origin) {
            origins.push(TrustedOrigin {
                kind: OriginKind::Relay,
                origin,
            });
        }
    }
    origins
}

fn home_route_origin(request: &Request<axum::body::Body>) -> Option<String> {
    request
        .extensions()
        .get::<lan::HomeRoute>()
        .map(|home| home.origin.clone())
}

/// The origins a request may present: only its own route's. `None` while
/// no route is ready yet.
///
/// A request that came in over a home network listener may use only that
/// listener's origin. Every other request arrived on loopback, where
/// Tailscale Serve and the relay tunnel deliver: a home origin is never valid
/// there, and the relay origin is valid exactly when the request names the
/// relay address (the edge routes by Host, so a relay request always does).
/// Without this, a request over the relay could present the Tailscale or a
/// home origin and be served, and challenged, as that route.
async fn request_trusted_origins(
    state: &AppState,
    route: RequestRoute,
) -> Option<Vec<TrustedOrigin>> {
    if let Some(origin) = route.home {
        return Some(vec![TrustedOrigin {
            kind: OriginKind::Home,
            origin,
        }]);
    }
    let origins = current_trusted_origins(state).await;
    if origins.is_empty() {
        return None;
    }
    let mut relay_hosts: Vec<String> = origins
        .iter()
        .filter(|trusted| trusted.kind == OriginKind::Relay)
        .filter_map(|trusted| trusted.origin.strip_prefix("https://"))
        .map(relay::authority_host)
        .collect();
    relay_hosts.extend(relay::relay_host(&state.data_dir));
    let via_relay = route
        .authorities
        .iter()
        .any(|named| relay_hosts.contains(&relay::authority_host(named)));
    Some(
        origins
            .into_iter()
            .filter(|trusted| {
                trusted.kind != OriginKind::Home && (trusted.kind == OriginKind::Relay) == via_relay
            })
            .collect(),
    )
}

/// What a request says about the route it came in on, read before any await
/// (a request body is not `Sync`).
struct RequestRoute {
    /// The home network listener's origin, when it arrived on one.
    home: Option<String>,
    /// Its `Host` header and URI authority.
    authorities: Vec<String>,
}

impl RequestRoute {
    fn of(request: &Request<axum::body::Body>) -> Self {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok());
        let authority = request
            .uri()
            .authority()
            .map(|authority| authority.as_str());
        Self {
            home: home_route_origin(request),
            authorities: [host, authority]
                .into_iter()
                .flatten()
                .map(str::to_owned)
                .collect(),
        }
    }
}

/// Reads the newest status line: `origin` is the primary route and the
/// optional `routes` array adds `{kind, origin}` entries. Entries with an
/// unknown kind or an origin that is not a valid public origin are skipped.
fn trusted_origins_from_status_log(contents: &str) -> Vec<TrustedOrigin> {
    let Some(status) = contents
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    else {
        return Vec::new();
    };
    if status.get("state").and_then(serde_json::Value::as_str) != Some("ready") {
        return Vec::new();
    }
    let primary = status
        .get("origin")
        .and_then(serde_json::Value::as_str)
        .map(TrustedOrigin::primary);
    let routes = status
        .get("routes")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|route| serde_json::from_value::<TrustedOrigin>(route.clone()).ok());
    let mut origins: Vec<TrustedOrigin> = Vec::new();
    for trusted in primary.into_iter().chain(routes) {
        if is_valid_public_origin(&trusted.origin)
            && !origins.iter().any(|known| known.origin == trusted.origin)
        {
            origins.push(trusted);
        }
    }
    origins
}

fn is_valid_public_origin(origin: &str) -> bool {
    let Some(host_port) = origin.strip_prefix("https://") else {
        return false;
    };
    if host_port.is_empty()
        || host_port.contains('/')
        || host_port.contains('@')
        || host_port.chars().any(char::is_whitespace)
    {
        return false;
    }
    let host = host_port
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_matches(['[', ']']);
    !host.is_empty()
        && host != "localhost"
        && host != "127.0.0.1"
        && host != "::1"
        && host != "wonder.invalid"
}

fn matches_public_origin(request: &Request<axum::body::Body>, public_origin: &str) -> bool {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if origin == Some(public_origin) {
        return true;
    }
    if origin.is_some() || request.method() != axum::http::Method::GET {
        return false;
    }

    let public_host = public_origin
        .strip_prefix("https://")
        .or_else(|| public_origin.strip_prefix("http://"));
    let request_host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    public_host.is_some_and(|host| request_host == Some(host))
}

fn matching_trusted_origin(
    request: &Request<axum::body::Body>,
    origins: Vec<TrustedOrigin>,
) -> Option<TrustedOrigin> {
    origins
        .into_iter()
        .find(|trusted| matches_public_origin(request, &trusted.origin))
}

async fn readyz(State(state): State<AppState>) -> Response {
    let readiness = state.ingestion.readiness_report(&state.store).await;
    (
        if readiness.ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(readiness),
    )
        .into_response()
}

async fn pairing_link_opened_in_browser() -> &'static str {
    "This is a Cupola pairing link. Open Cupola on your iPhone or iPad, tap Add computer, and scan or paste it there.\n"
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

/// The computer's own name, read once. The name Wonder shows is
/// [`host_name::current`], which the owner can change.
pub(crate) fn system_host_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        #[cfg(target_os = "macos")]
        if let Ok(output) = std::process::Command::new("/usr/sbin/scutil")
            .args(["--get", "ComputerName"])
            .output()
        {
            if output.status.success() {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !name.is_empty() {
                    return name;
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        if let Some(name) = headless_host_name() {
            return name;
        }
        "Mac".to_owned()
    })
}

/// Experimental headless (Linux, Windows) host: name the computer after the operator's
/// choice or its hostname instead of a Mac-specific default.
#[cfg(not(target_os = "macos"))]
fn headless_host_name() -> Option<String> {
    if let Some(name) = std::env::var("WONDER_HOST_NAME")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
    {
        return Some(name);
    }
    Some(crate::platform::host_name().unwrap_or_else(|| HEADLESS_FALLBACK_NAME.to_owned()))
}

#[cfg(windows)]
const HEADLESS_FALLBACK_NAME: &str = "Windows computer";
#[cfg(not(any(windows, target_os = "macos")))]
const HEADLESS_FALLBACK_NAME: &str = "Linux computer";

async fn host_status(State(state): State<AppState>) -> impl IntoResponse {
    let trusted = current_trusted_origins(&state).await;
    let public_origin = primary_origin(&trusted);
    let routes = published_routes(state.home_routes.origins(), trusted);
    let execution = state.ingestion.readiness_report(&state.store).await;
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(HostStatus {
            host_name: host_name::current(&state).await,
            api_version: "v1",
            host_epoch: state.host_epoch.clone(),
            host_installation_id: state.host_installation_id.clone(),
            state: if execution.ready {
                host_readiness_state(public_origin.as_deref())
            } else {
                "degraded"
            },
            execution,
            codex_transport: "stdio-jsonl",
            started_at: state.started_at.clone(),
            public_origin,
            platform: std::env::consts::OS,
            features: [
                Some(projects::FEATURE),
                Some(computer_sessions::HOST_VIEW_FEATURE),
                cfg!(unix).then_some(voice::FEATURE),
                cfg!(unix).then_some(voice::LIMITS_FEATURE),
                Some(t3_code::FEATURE),
            ]
            .into_iter()
            .flatten()
            .collect(),
            routes,
            host_key_pin: state.home_routes.host_key_pin(),
        }),
    )
}

async fn runtime_capabilities(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    let app_server = match projects::codex_rpc(&state).await {
        Ok(rpc) => rpc,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    };
    let mut warnings = Vec::new();
    let apps = match app_server
        .request("app/list", serde_json::json!({ "limit": 100 }))
        .await
    {
        Ok(response) if response.error.is_none() => response
            .result
            .as_ref()
            .and_then(|result| result.get("data"))
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(runtime_app_summary)
                    .filter(|app| app.accessible)
                    .collect()
            })
            .unwrap_or_default(),
        _ => {
            warnings.push("Available Apps could not be loaded.".to_owned());
            Vec::new()
        }
    };

    let skills = match app_server
        .request(
            "skills/list",
            serde_json::json!({
                "cwds": [state.codex_home],
                "forceReload": false
            }),
        )
        .await
    {
        Ok(response) if response.error.is_none() => response
            .result
            .as_ref()
            .and_then(|result| result.get("data"))
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .flat_map(|entry| {
                        entry
                            .get("skills")
                            .and_then(serde_json::Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .filter_map(runtime_skill_summary)
                    .filter(|skill| skill.enabled)
                    .collect()
            })
            .unwrap_or_default(),
        _ => {
            warnings.push("Available Skills could not be loaded.".to_owned());
            Vec::new()
        }
    };

    let mcp_servers = match app_server
        .request(
            "mcpServerStatus/list",
            serde_json::json!({ "limit": 100, "detail": "toolsAndAuthOnly" }),
        )
        .await
    {
        Ok(response) if response.error.is_none() => response
            .result
            .as_ref()
            .and_then(|result| result.get("data"))
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(runtime_mcp_server_summary)
                    .collect()
            })
            .unwrap_or_default(),
        _ => {
            warnings.push("MCP connection status could not be loaded.".to_owned());
            Vec::new()
        }
    };

    Json(RuntimeCapabilitiesResponse {
        apps,
        skills,
        mcp_servers,
        warnings,
        refreshed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    })
    .into_response()
}

fn runtime_app_summary(value: &serde_json::Value) -> Option<RuntimeAppSummary> {
    Some(RuntimeAppSummary {
        id: value.get("id")?.as_str()?.to_owned(),
        name: value.get("name")?.as_str()?.to_owned(),
        description: value
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        category: value
            .get("distributionChannel")
            .or_else(|| value.get("category"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        enabled: value
            .get("isEnabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        accessible: value
            .get("isAccessible")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

fn runtime_skill_summary(value: &serde_json::Value) -> Option<RuntimeSkillSummary> {
    Some(RuntimeSkillSummary {
        name: value.get("name")?.as_str()?.to_owned(),
        description: value
            .get("shortDescription")
            .or_else(|| value.get("description"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Available Skill")
            .to_owned(),
        enabled: value
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        plugin_id: value
            .get("pluginId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

fn runtime_mcp_server_summary(value: &serde_json::Value) -> Option<RuntimeMcpServerSummary> {
    Some(RuntimeMcpServerSummary {
        name: value.get("name")?.as_str()?.to_owned(),
        plugin_id: value
            .get("pluginId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        auth_status: value
            .get("authStatus")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        runtime_status: value
            .get("runtimeStatus")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        tool_count: value
            .get("tools")
            .and_then(serde_json::Value::as_object)
            .map(serde_json::Map::len)
            .unwrap_or(0),
    })
}

fn host_readiness_state(public_origin: Option<&str>) -> &'static str {
    if public_origin.is_some() {
        "ready"
    } else {
        "degraded"
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SearchQuery {
    q: String,
    limit: Option<u32>,
    cursor: Option<String>,
    conversation_id: Option<String>,
}

fn search_result_deep_link(result: &wonder_store::StoredSearchResult) -> String {
    match result.kind.as_str() {
        "message" | "assistant_message" | "file" => result
            .conversation_id
            .as_deref()
            .map(|conversation_id| {
                format!(
                    "#/chats/{}?focus={}&kind={}",
                    conversation_id, result.id, result.kind
                )
            })
            .unwrap_or_else(|| "#/chats".into()),
        "conversation" => result
            .conversation_id
            .as_deref()
            .map(|conversation_id| format!("#/chats/{conversation_id}"))
            .unwrap_or_else(|| "#/chats".into()),
        _ => "#/chats".into(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateConversationRequest {
    mark_read: bool,
    host_epoch: String,
    read_through_sequence: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversationFileSummary {
    id: String,
    kind: String,
    name: String,
    mime_type: Option<String>,
    byte_size: Option<i64>,
    sha256: Option<String>,
    relative_path: Option<String>,
    state: String,
    additions: Option<i64>,
    deletions: Option<i64>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateConversationFileRequest {
    client_upload_id: Option<String>,
    name: String,
    mime_type: Option<String>,
    content_base64: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AutomationSummary {
    id: String,
    revision: i64,
    name: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
    prompt: String,
    rrule: String,
    timezone: String,
    status: String,
    notification_policy: String,
    scope_type: String,
    scope_id: String,
    next_run_at: Option<String>,
    last_run_at: Option<String>,
    last_attempt_at: Option<String>,
    last_success_at: Option<String>,
    /// The chat whose agent scheduled this; absent when the owner made it.
    #[serde(skip_serializing_if = "Option::is_none")]
    created_by_conversation_id: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AutomationRunSummary {
    id: String,
    automation_id: String,
    scheduled_for: String,
    status: String,
    started_at: String,
    finished_at: Option<String>,
    error: Option<String>,
    message_id: Option<String>,
    conversation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreateAutomationRequest {
    name: String,
    kind: String,
    conversation_id: Option<String>,
    prompt: String,
    rrule: String,
    timezone: String,
    status: Option<String>,
    notification_policy: Option<String>,
    scope_type: Option<String>,
    scope_id: Option<String>,
    client_request_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UpdateAutomationRequest {
    expected_revision: Option<i64>,
    name: Option<String>,
    prompt: Option<String>,
    kind: Option<String>,
    conversation_id: Option<String>,
    rrule: Option<String>,
    timezone: Option<String>,
    status: Option<String>,
    notification_policy: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewAutomationRequest {
    rrule: String,
    timezone: String,
}

async fn preview_automation(Json(request): Json<PreviewAutomationRequest>) -> Response {
    if request.rrule.len() > 512 || request.timezone.len() > 80 {
        return (StatusCode::BAD_REQUEST, "The schedule is too long.").into_response();
    }
    match next_automation_run(request.rrule.trim(), request.timezone.trim(), Utc::now()) {
        Ok(next) => Json(serde_json::json!({"nextRunAt":next,"timezone":request.timezone.trim()}))
            .into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error).into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RunAutomationRequest {
    client_request_id: Option<String>,
}

fn automation_request_id(prefix: &str, client_id: Option<&str>) -> Result<String, &'static str> {
    match client_id {
        Some(value) if value.trim().is_empty() || value.len() > 160 => {
            Err("The request identifier is invalid.")
        }
        Some(value) => Ok(format!(
            "{prefix}:{}",
            hex::encode(Sha256::digest(value.as_bytes()))
        )),
        None => Ok(uuid::Uuid::new_v4().to_string()),
    }
}

async fn automation_target_available(
    state: &AppState,
    target: &wonder_store::AutomationTarget,
    kind: &str,
    conversation_id: Option<&str>,
) -> Result<(), (StatusCode, &'static str)> {
    let wonder_store::AutomationTarget::ProjectThread { scope_id } = target;
    {
        if kind != "continuation" || conversation_id != Some(scope_id) {
            return Err((
                StatusCode::BAD_REQUEST,
                "Choose a Project thread to continue.",
            ));
        }
        let conversation = state
            .store
            .project_conversation(scope_id)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Project thread could not be loaded.",
                )
            })?
            .ok_or((StatusCode::BAD_REQUEST, "The Project thread was not found."))?;
        let project = state
            .store
            .project(&conversation.project_id)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Project could not be loaded.",
                )
            })?
            .ok_or((StatusCode::BAD_REQUEST, "The Project was not found."))?;
        let project = projects::thread_project(state, project, &conversation.cwd).await;
        if !project.is_included || project.root_for(&conversation.cwd).is_none() {
            return Err((
                StatusCode::CONFLICT,
                "Restore this Project and its folder before enabling automations.",
            ));
        }
        let binding = state.store.runtime_binding(scope_id).await.map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Project thread binding could not be loaded.",
            )
        })?;
        let verified = if let Some(binding) = binding {
            state
                .store
                .verified_project_runtime_owner(scope_id, &binding.thread_id)
                .await
                .unwrap_or(false)
        } else {
            false
        };
        if !verified {
            return Err((
                StatusCode::CONFLICT,
                "Open this Project thread on your computer before scheduling it.",
            ));
        }
    }
    Ok(())
}

fn automation_summary(automation: wonder_store::StoredAutomation) -> AutomationSummary {
    let scope_type = automation.target.scope_type().to_owned();
    let scope_id = automation.target.scope_id().to_owned();
    let created_by_conversation_id =
        thread_tools::schedule::creator(&automation.id).map(str::to_owned);
    AutomationSummary {
        id: automation.id,
        revision: automation.revision,
        name: automation.name,
        kind: automation.kind,
        conversation_id: automation.conversation_id,
        prompt: automation.prompt,
        rrule: automation.rrule,
        timezone: automation.timezone,
        status: automation.status,
        notification_policy: automation.notification_policy,

        scope_type,
        scope_id,
        next_run_at: automation.next_run_at,
        last_run_at: automation.last_run_at,
        last_attempt_at: automation.last_attempt_at,
        last_success_at: automation.last_success_at,
        created_by_conversation_id,
        created_at: automation.created_at,
        updated_at: automation.updated_at,
    }
}

fn automation_run_summary(run: wonder_store::StoredAutomationRun) -> AutomationRunSummary {
    AutomationRunSummary {
        id: run.id,
        automation_id: run.automation_id,
        scheduled_for: run.scheduled_for,
        status: run.status,
        started_at: run.started_at,
        finished_at: run.finished_at,
        error: run.error,
        message_id: run.message_id,
        conversation_id: run.conversation_id,
    }
}

async fn list_automations(State(state): State<AppState>) -> Response {
    match state.store.list_automations().await {
        Ok(automations) => Json(
            automations
                .into_iter()
                .map(automation_summary)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "automation listing failed",
        )
            .into_response(),
    }
}

async fn create_automation(
    State(state): State<AppState>,
    Json(request): Json<CreateAutomationRequest>,
) -> Response {
    match save_automation(&state, request, "automation").await {
        Ok((status, automation)) => (status, Json(automation_summary(automation))).into_response(),
        Err(error) => error.into_response(),
    }
}

pub(crate) type AutomationError = (StatusCode, std::borrow::Cow<'static, str>);

/// The one owner of a new automation, for the phone and for agents. The ID
/// comes from `id_prefix` and the client's request ID, so a retry returns
/// the automation it made (OK) instead of a second one (CREATED).
pub(crate) async fn save_automation(
    state: &AppState,
    request: CreateAutomationRequest,
    id_prefix: &str,
) -> Result<(StatusCode, wonder_store::StoredAutomation), AutomationError> {
    let fail = |status: StatusCode, message: &'static str| Err((status, message.into()));
    let name = request.name.trim();
    let kind = request.kind.trim();
    let prompt = request.prompt.trim();
    let timezone = request.timezone.trim();
    let rrule = request.rrule.trim();
    let status = request.status.as_deref().unwrap_or("active");
    let notification_policy = request.notification_policy.as_deref().unwrap_or("all_runs");
    if name.is_empty()
        || name.len() > 120
        || prompt.is_empty()
        || prompt.len() > 8_000
        || kind != "continuation"
        || timezone.is_empty()
        || timezone.len() > 80
        || rrule.is_empty()
        || rrule.len() > 512
        || !matches!(status, "active" | "paused")
        || !matches!(notification_policy, "all_runs" | "failed_runs_only")
        || (kind == "continuation" && request.conversation_id.is_none())
    {
        return fail(StatusCode::BAD_REQUEST, "invalid automation");
    }
    let scope_type = request
        .scope_type
        .as_deref()
        .unwrap_or("project_thread")
        .trim();
    let scope_id = request.scope_id.as_deref().map(str::trim).unwrap_or("");
    if scope_type != "project_thread" || scope_id.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "invalid automation target");
    }
    let target = wonder_store::AutomationTarget::ProjectThread {
        scope_id: scope_id.to_owned(),
    };
    let id = match automation_request_id(id_prefix, request.client_request_id.as_deref()) {
        Ok(id) => id,
        Err(error) => return fail(StatusCode::BAD_REQUEST, error),
    };
    let conversation_id = request.conversation_id.clone();
    // A retry may arrive after the first response was lost. The request ID
    // identifies one immutable create intent, even if the editor changed.
    let same_intent = |existing: &wonder_store::StoredAutomation| {
        existing.name == name
            && existing.kind == kind
            && existing.target == target
            && existing.conversation_id == conversation_id
            && existing.prompt == prompt
            && existing.rrule == rrule
            && existing.timezone == timezone
            && existing.status == status
            && existing.notification_policy == notification_policy
    };
    let changed_retry = || {
        fail(
            StatusCode::CONFLICT,
            "This request already created an automation with different details. Review the existing task before saving another.",
        )
    };
    match state.store.automation_by_id(&id).await {
        Ok(Some(existing)) => {
            return if same_intent(&existing) {
                Ok((StatusCode::OK, existing))
            } else {
                changed_retry()
            };
        }
        Err(_) => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Automation could not be loaded.",
            )
        }
        Ok(None) => {}
    }
    // Availability and model catalogs can change after the first insert. An
    // exact retry still returns that accepted create without starting work.
    if let Err(error) =
        automation_target_available(state, &target, kind, request.conversation_id.as_deref()).await
    {
        return fail(error.0, error.1);
    }
    let now_utc = Utc::now();
    let preview = match next_automation_run(rrule, timezone, now_utc) {
        Ok(next) => next,
        Err(error) => return Err((StatusCode::BAD_REQUEST, error.into())),
    };
    let next_run_at = if status == "active" { preview } else { None };
    let now = now_utc.to_rfc3339_opts(SecondsFormat::Millis, true);
    match state
        .store
        .insert_scoped_automation(
            &id,
            name,
            kind,
            target.scope_type(),
            target.scope_id(),
            conversation_id.as_deref(),
            prompt,
            rrule,
            timezone,
            status,
            notification_policy,
            next_run_at.as_deref(),
            &now,
        )
        .await
    {
        Ok(automation) if same_intent(&automation) => Ok((StatusCode::CREATED, automation)),
        Ok(_) => changed_retry(),
        Err(_) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "automation could not be saved",
        ),
    }
}

async fn update_automation(
    State(state): State<AppState>,
    Path(automation_id): Path<String>,
    Json(request): Json<UpdateAutomationRequest>,
) -> Response {
    match change_automation(&state, &automation_id, request).await {
        Ok(updated) => Json(automation_summary(updated)).into_response(),
        Err((StatusCode::NOT_FOUND, _)) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => error.into_response(),
    }
}

/// The one owner of automation edits, for the phone and for agents.
pub(crate) async fn change_automation(
    state: &AppState,
    automation_id: &str,
    request: UpdateAutomationRequest,
) -> Result<wonder_store::StoredAutomation, AutomationError> {
    let fail = |status: StatusCode, message: &'static str| Err((status, message.into()));
    let mut value = match state.store.automation_by_id(automation_id).await {
        Ok(Some(value)) => value,
        Ok(None) => return fail(StatusCode::NOT_FOUND, "That automation was not found."),
        Err(_) => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Automation could not be loaded.",
            )
        }
    };
    if request
        .expected_revision
        .is_some_and(|revision| revision != value.revision)
    {
        return fail(
            StatusCode::CONFLICT,
            "This automation changed. Refresh it before saving.",
        );
    }
    let reschedule =
        request.rrule.is_some() || request.timezone.is_some() || request.status.is_some();
    macro_rules! apply {
        ($field:ident) => {
            if let Some(new) = request.$field {
                value.$field = new.trim().to_owned();
            }
        };
    }
    apply!(name);
    apply!(prompt);
    apply!(kind);
    apply!(rrule);
    apply!(timezone);
    apply!(status);
    apply!(notification_policy);
    if let Some(id) = request.conversation_id {
        value.conversation_id = Some(id);
    }
    if value.name.is_empty()
        || value.name.len() > 120
        || value.prompt.is_empty()
        || value.prompt.len() > 8000
        || value.rrule.len() > 512
        || value.timezone.len() > 80
        || value.kind != "continuation"
        || !matches!(value.status.as_str(), "active" | "paused")
        || !matches!(
            value.notification_policy.as_str(),
            "all_runs" | "failed_runs_only"
        )
    {
        return fail(
            StatusCode::BAD_REQUEST,
            "Check the automation name, task, and schedule.",
        );
    }
    if let Err(error) = automation_target_available(
        state,
        &value.target,
        &value.kind,
        value.conversation_id.as_deref(),
    )
    .await
    {
        // Pausing remains possible when the target was archived.
        if value.status != "paused" || error.0 != StatusCode::CONFLICT {
            return fail(error.0, error.1);
        }
    }
    let now = Utc::now();
    let preview = match next_automation_run(&value.rrule, &value.timezone, now) {
        Ok(next) => next,
        Err(error) => return Err((StatusCode::BAD_REQUEST, error.into())),
    };
    if value.status == "paused" {
        value.next_run_at = None;
    } else if reschedule {
        value.next_run_at = preview;
    }
    value.updated_at = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    match state.store.update_automation(&value).await {
        Ok(true) => match state.store.automation_by_id(automation_id).await {
            Ok(Some(updated)) => Ok(updated),
            _ => fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "The automation was saved but could not be reloaded. Refresh to check it.",
            ),
        },
        Ok(false) => fail(
            StatusCode::CONFLICT,
            "The automation changed or its target is unavailable. Refresh before saving.",
        ),
        Err(_) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Automation could not be saved.",
        ),
    }
}

async fn list_automation_runs(
    State(state): State<AppState>,
    Path(automation_id): Path<String>,
) -> Response {
    match state.store.automation_by_id(&automation_id).await {
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "automation lookup failed",
        )
            .into_response(),
        Ok(Some(_)) => match state.store.list_automation_runs(&automation_id).await {
            Ok(runs) => Json(
                runs.into_iter()
                    .map(automation_run_summary)
                    .collect::<Vec<_>>(),
            )
            .into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "automation runs could not be loaded",
            )
                .into_response(),
        },
    }
}

async fn run_automation_now(
    State(state): State<AppState>,
    Path(automation_id): Path<String>,
    request: Option<Json<RunAutomationRequest>>,
) -> Response {
    let automation = match state.store.automation_by_id(&automation_id).await {
        Ok(Some(value)) => value,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Automation could not be loaded.",
            )
                .into_response()
        }
    };
    let request = request.map(|Json(value)| value).unwrap_or_default();
    let run_id = match automation_request_id(
        &format!("run:{automation_id}"),
        request.client_request_id.as_deref(),
    ) {
        Ok(id) => id,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    match state.store.automation_run_by_id(&run_id).await {
        Ok(Some(run)) => return Json(automation_run_summary(run)).into_response(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The run could not be loaded.",
            )
                .into_response()
        }
        Ok(None) => {}
    }
    if let Err(error) = automation_target_available(
        &state,
        &automation.target,
        &automation.kind,
        automation.conversation_id.as_deref(),
    )
    .await
    {
        return error.into_response();
    }
    if matches!(
        &automation.target,
        wonder_store::AutomationTarget::ProjectThread { .. }
    ) && !projects::ready(&state).await
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The selected agent is offline. Connect the computer it runs on, then try again.",
        )
            .into_response();
    }
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    match state.store.claim_scheduled_automation(&run_id,&automation,&now,&now,None,false).await {
        Ok(true)=>{
            let response = state.store.automation_run_by_id(&run_id).await;
            tokio::spawn(run_automation(state,automation,now,run_id));
            match response {
                Ok(Some(run))=>(StatusCode::ACCEPTED,Json(automation_run_summary(run))).into_response(),
                _=>(StatusCode::INTERNAL_SERVER_ERROR,"The run was accepted but could not be loaded. Retry with the same request identifier.").into_response(),
            }
        }
        Ok(false)=>match state.store.automation_run_by_id(&run_id).await {
            Ok(Some(run))=>Json(automation_run_summary(run)).into_response(),
            _=>(StatusCode::CONFLICT,"An automation run is already in progress, or the automation changed. Refresh and try again.").into_response(),
        },
        Err(_)=>(StatusCode::INTERNAL_SERVER_ERROR,"The automation run could not be accepted.").into_response(),
    }
}

async fn delete_automation(
    State(state): State<AppState>,
    Path(automation_id): Path<String>,
) -> Response {
    match remove_automation(&state, &automation_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err((StatusCode::NOT_FOUND, _)) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => error.into_response(),
    }
}

/// The one owner of deleting an automation, for the phone and for agents.
pub(crate) async fn remove_automation(
    state: &AppState,
    automation_id: &str,
) -> Result<(), AutomationError> {
    let fail = |status: StatusCode, message: &'static str| Err((status, message.into()));
    let runs = match state.store.list_automation_runs(automation_id).await {
        Ok(runs) => runs,
        Err(_) => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Automation runs could not be checked.",
            )
        }
    };
    if runs.iter().any(|run| run.status == "running") {
        return fail(StatusCode::CONFLICT,"Wait for this automation to finish, or stop its run in the conversation before deleting it.");
    }
    match state.store.delete_automation(automation_id).await {
        Ok(true) => Ok(()),
        Ok(false) => match state.store.automation_by_id(automation_id).await {
            Ok(Some(_)) => fail(
                StatusCode::CONFLICT,
                "The automation started running. Wait for it to finish before deleting it.",
            ),
            Ok(None) => fail(StatusCode::NOT_FOUND, "That automation was not found."),
            Err(_) => fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Automation could not be loaded.",
            ),
        },
        Err(_) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "automation could not be deleted",
        ),
    }
}

async fn search_endpoint(
    State(state): State<AppState>,
    Extension(_): Extension<OwnerAuthority>,
    Query(query): Query<SearchQuery>,
) -> Response {
    let query_text = query.q.trim();
    let scope = query.conversation_id.as_deref();
    if query_text.is_empty()
        || query_text.len() > 256
        || query.limit.is_some_and(|n| n == 0 || n > 100)
        || scope.is_some_and(|s| s.is_empty() || s.len() > 256)
    {
        return (
            StatusCode::BAD_REQUEST,
            "Use a 1-256 byte query and page limit of 1-100.",
        )
            .into_response();
    }
    // Version 2 carries one position per source (Wonder, runtime, index).
    type Cursor = (
        u8,
        String,
        String,
        Option<String>,
        native_search::MergedCursor,
    );
    let before = match query.cursor.as_deref() {
        None => native_search::MergedCursor::default(),
        Some(cursor) => {
            let decoded = (cursor.len() <= 16384)
                .then(|| {
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(cursor)
                        .ok()
                })
                .flatten()
                .and_then(|bytes| serde_json::from_slice::<Cursor>(&bytes).ok());
            match decoded {
                Some((2, epoch, q, conversation, before))
                    if epoch == state.host_epoch
                        && q == query_text
                        && conversation.as_deref() == scope =>
                {
                    before
                }
                _ => {
                    return (
                        StatusCode::BAD_REQUEST,
                        "Invalid search cursor. Search again.",
                    )
                        .into_response()
                }
            }
        }
    };
    match native_search::search(
        &state,
        query_text,
        scope,
        before,
        query.limit.unwrap_or(30),
        state.claude.is_some(),
    )
    .await
    {
        Ok(outcome) => {
            let next_cursor = outcome.next.map(|cursor| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                    serde_json::to_vec(&(2, &state.host_epoch, query_text, scope, cursor)).unwrap(),
                )
            });
            Json(serde_json::json!({
                "results": outcome.hits,
                "nextCursor": next_cursor,
                "partial": outcome.partial,
            }))
            .into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Search failed. Try again.",
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OccurrenceQuery {
    q: String,
}

/// Matches inside one Project conversation's native history, so a search
/// result opens at its message after the thread is attached.
async fn search_occurrences_endpoint(
    State(state): State<AppState>,
    Extension(_): Extension<OwnerAuthority>,
    Path(conversation_id): Path<String>,
    Query(query): Query<OccurrenceQuery>,
) -> Response {
    let query_text = query.q.trim();
    if query_text.is_empty() || query_text.len() > 256 {
        return (StatusCode::BAD_REQUEST, "Use a 1-256 byte query.").into_response();
    }
    let conversation = match state.store.project_conversation(&conversation_id).await {
        Ok(Some(conversation)) => conversation,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Search is temporarily unavailable. Try again.",
            )
                .into_response()
        }
    };
    match native_search::occurrences(&state, &conversation, query_text).await {
        Ok(Some(occurrences)) => Json(serde_json::json!({
            "supported": true,
            "occurrences": occurrences,
        }))
        .into_response(),
        Ok(None) => {
            Json(serde_json::json!({"supported": false, "occurrences": []})).into_response()
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "This chat could not be searched. Try again.",
        )
            .into_response(),
    }
}

fn deterministic_uuid(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChatNotificationsRequest {
    notification_level: wonder_store::ChatNotificationLevel,
}

async fn chat_notifications(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
) -> Response {
    if !projects::is_project(&state, &conversation).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state.store.chat_notification_level(&conversation).await {
        Ok(Some(level)) => Json(serde_json::json!({"notificationLevel": level})).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn set_chat_notifications(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
    Json(request): Json<ChatNotificationsRequest>,
) -> Response {
    if !projects::is_project(&state, &conversation).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state
        .store
        .set_chat_notification_level(&conversation, request.notification_level)
        .await
    {
        Ok(true) => {
            state.live_activity.notification_policy_changed();
            Json(serde_json::json!({"notificationLevel": request.notification_level}))
                .into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn update_conversation(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Json(request): Json<UpdateConversationRequest>,
) -> Response {
    if !request.mark_read || request.host_epoch.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "A visible history snapshot is required.",
        )
            .into_response();
    }
    if !projects::is_project(&state, &conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Reading acknowledges only the snapshot the phone actually displayed.
    match state
        .store
        .acknowledge_conversation_read(
            &conversation_id,
            &request.host_epoch,
            request.read_through_sequence,
            &state.started_at,
        )
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn delivery_state_from_name(value: &str) -> Option<DeliveryState> {
    Some(match value {
        "draft_on_device" => DeliveryState::DraftOnDevice,
        "submitting" => DeliveryState::Submitting,
        "accepted_by_wonder" => DeliveryState::AcceptedByWonder,
        "dispatching_to_codex" => DeliveryState::DispatchingToCodex,
        "accepted_by_codex" => DeliveryState::AcceptedByCodex,
        "streaming" => DeliveryState::Streaming,
        "completed" => DeliveryState::Completed,
        "interrupted" => DeliveryState::Interrupted,
        "failed" => DeliveryState::Failed,
        "uncertain" => DeliveryState::Uncertain,
        "safe_to_retry" => DeliveryState::SafeToRetry,
        _ => return None,
    })
}

fn message_receipt(message: wonder_store::StoredMessage) -> Option<ClientMessageReceipt> {
    Some(ClientMessageReceipt {
        client_message_id: message.client_message_id,
        wonder_message_id: message.id,
        body_sha256: message.body_sha256,
        conversation_id: message.conversation_id,
        delivery_state: delivery_state_from_name(&message.state)?,
        codex_thread_id: message.codex_thread_id,
        codex_turn_id: message.codex_turn_id,
        queued: None,
    })
}

fn conversation_file_summary(file: StoredConversationFile) -> ConversationFileSummary {
    ConversationFileSummary {
        id: file.id,
        kind: file.kind,
        name: file.name,
        mime_type: file.mime_type,
        byte_size: file.byte_size,
        sha256: file.sha256,
        relative_path: file.relative_path,
        state: file.state,
        additions: file.additions,
        deletions: file.deletions,
        created_at: file.created_at,
        updated_at: file.updated_at,
    }
}

fn valid_attachment_name(name: &str) -> bool {
    let trimmed = name.trim();
    name == trimmed
        && !trimmed.is_empty()
        && trimmed.len() <= 255
        && !trimmed.chars().any(char::is_control)
        && !trimmed.contains(['/', '\\'])
        && trimmed != "."
        && trimmed != ".."
        && !FsPath::new(trimmed).is_absolute()
}

fn valid_attachment_mime_type(mime_type: &str) -> bool {
    let mut parts = mime_type.split('/');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(subtype) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !major.is_empty()
        && !subtype.is_empty()
        && mime_type.len() <= 255
        && mime_type.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '/' | '!' | '#' | '$' | '&' | '^' | '_' | '.' | '+' | '-'
                )
        })
}

fn attachment_relative_path(file_id: &str) -> String {
    format!(".wonder/attachments/{file_id}")
}

async fn attachment_path(workspace: &str, file_id: &str, create_root: bool) -> Option<PathBuf> {
    locate_attachment(workspace, file_id, create_root)
        .await
        .ok()
}

/// Why a stored attachment's path could not be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachmentPathError {
    /// The workspace or its attachment storage no longer exists.
    Missing,
    /// The ID is malformed or the storage resolves through a symlink or
    /// outside the workspace.
    Unsafe,
    /// The storage could not be inspected or created.
    Io,
}

impl AttachmentPathError {
    fn from_io(error: &std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::NotFound {
            Self::Missing
        } else {
            Self::Io
        }
    }
}

/// The contained path of an attachment, telling a removed workspace or
/// attachment directory apart from an unsafe one.
pub(crate) async fn locate_attachment(
    workspace: &str,
    file_id: &str,
    create_root: bool,
) -> Result<PathBuf, AttachmentPathError> {
    use AttachmentPathError::*;
    let uuid = uuid::Uuid::parse_str(file_id).map_err(|_| Unsafe)?;
    let root = crate::canonical_path::canonicalize_async(workspace)
        .await
        .map_err(|error| AttachmentPathError::from_io(&error))?;
    let mut attachments = root.clone();
    for component in [".wonder", "attachments"] {
        attachments.push(component);
        if create_root {
            match tokio::fs::create_dir(&attachments).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(Io),
            }
        }
        let metadata = tokio::fs::symlink_metadata(&attachments)
            .await
            .map_err(|error| AttachmentPathError::from_io(&error))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Unsafe);
        }
    }
    let canonical_attachments = crate::canonical_path::canonicalize_async(&attachments)
        .await
        .map_err(|error| AttachmentPathError::from_io(&error))?;
    if !canonical_attachments.starts_with(&root) || !canonical_attachments.is_dir() {
        return Err(Unsafe);
    }
    let candidate = canonical_attachments.join(uuid.to_string());
    if let Ok(metadata) = tokio::fs::symlink_metadata(&candidate).await {
        if metadata.file_type().is_symlink() {
            return Err(Unsafe);
        }
        let canonical = crate::canonical_path::canonicalize_async(&candidate)
            .await
            .map_err(|error| AttachmentPathError::from_io(&error))?;
        if !canonical.starts_with(&canonical_attachments) {
            return Err(Unsafe);
        }
    }
    Ok(candidate)
}

async fn upload_conversation_file(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Json(request): Json<CreateConversationFileRequest>,
) -> Response {
    if let Some(response) = subagents::reject_user_mutation(&state, &conversation_id).await {
        return response;
    }
    let workspace = if projects::is_project(&state, &conversation_id).await {
        let Some(workspace) = projects::media_workspace(&state, &conversation_id).await else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        workspace
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let name = request.name.trim();
    if !valid_attachment_name(&request.name) {
        return (StatusCode::BAD_REQUEST, "invalid attachment name").into_response();
    }
    let mime_type = request
        .mime_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream")
        .to_owned();
    if !valid_attachment_mime_type(&mime_type) {
        return (StatusCode::BAD_REQUEST, "invalid attachment MIME type").into_response();
    }
    // Decoding, hashing and checking a picture's bytes against its declared
    // type take a blocking thread, once; the digest travels from here.
    let declared = mime_type.clone();
    let decoded = tokio::task::spawn_blocking(move || {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(request.content_base64)
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "attachment content is not valid base64",
                )
            })?;
        if bytes.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "attachment is empty"));
        }
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "attachment is too large"));
        }
        if !uploads::content_matches(&declared, &bytes) {
            return Err((
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "This file's contents don't match its file type. Export it again, then attach it.",
            ));
        }
        let sha256 = hex::encode(Sha256::digest(&bytes));
        Ok((bytes, sha256))
    })
    .await;
    let (bytes, sha256) = match decoded {
        Ok(Ok(decoded)) => decoded,
        Ok(Err(refusal)) => return refusal.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let mime_type = mime_type.as_str();
    let file_id = match request.client_upload_id {
        Some(id) => {
            if uuid::Uuid::parse_str(&id).is_err() {
                return (StatusCode::BAD_REQUEST, "invalid upload identity").into_response();
            }
            deterministic_uuid(&format!("native-upload:{conversation_id}:{id}"))
        }
        None => uuid::Uuid::new_v4().to_string(),
    };
    let _identity = attachments::identity_guard(&file_id).await;
    match state.store.list_conversation_files(&conversation_id).await {
        Ok(files) => {
            if let Some(existing) = files.into_iter().find(|file| file.id == file_id) {
                if existing.sha256.as_deref() != Some(&sha256)
                    || existing.name != name
                    || existing.mime_type.as_deref() != Some(mime_type)
                {
                    return (
                        StatusCode::CONFLICT,
                        "upload identity already has different content",
                    )
                        .into_response();
                }
                let Some(path) = attachment_path(&workspace, &existing.id, false).await else {
                    return (
                        StatusCode::CONFLICT,
                        "The saved upload is unavailable. Attach the file again.",
                    )
                        .into_response();
                };
                if let Err(error) = attachments::verified_bytes(&path, &existing).await {
                    return (StatusCode::CONFLICT, error).into_response();
                }
                return Json(conversation_file_summary(existing)).into_response();
            }
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let relative_path = attachment_relative_path(&file_id);
    let Some(file_path) = attachment_path(&workspace, &file_id, true).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "attachment path is outside the Project media storage",
        )
            .into_response();
    };
    if let Err(error) = attachments::write_immutable(&file_path, &bytes).await {
        return (StatusCode::CONFLICT, error).into_response();
    }
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    if state
        .store
        .upsert_conversation_file(
            &file_id,
            &conversation_id,
            "attachment",
            name,
            Some(mime_type),
            Some(bytes.len() as i64),
            Some(&sha256),
            Some(&relative_path),
            "available",
            None,
            None,
            None,
            &now,
        )
        .await
        .is_err()
    {
        // Retain complete immutable bytes so this upload identity can recover
        // after a metadata failure; a concurrent retry may already reference it.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "attachment metadata could not be saved",
        )
            .into_response();
    }
    match state.store.list_conversation_files(&conversation_id).await {
        Ok(files) => files
            .into_iter()
            .find(|file| file.id == file_id)
            .map(|file| Json(conversation_file_summary(file)).into_response())
            .unwrap_or_else(|| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "attachment could not be loaded",
        )
            .into_response(),
    }
}

/// Removing a sketch or file from a draft forgets the copy the phone already
/// uploaded. Only the owner's own uploads that no message carries go; a sent
/// message keeps its files.
async fn remove_unsent_upload(
    State(state): State<AppState>,
    Path((conversation_id, file_id)): Path<(String, String)>,
) -> Response {
    if let Some(response) = subagents::reject_user_mutation(&state, &conversation_id).await {
        return response;
    }
    if !projects::is_project(&state, &conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let _identity = attachments::identity_guard(&file_id).await;
    let file = match state
        .store
        .remove_unsent_upload(&conversation_id, &file_id)
        .await
    {
        Ok(wonder_store::UploadRemoval::Removed(file)) => file,
        Ok(wonder_store::UploadRemoval::Missing) => return StatusCode::NOT_FOUND.into_response(),
        Ok(wonder_store::UploadRemoval::Referenced) => {
            return (
                StatusCode::CONFLICT,
                "This file was already sent, so it stays with that message.",
            )
                .into_response()
        }
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "The file could not be removed. Try again.",
            )
                .into_response()
        }
    };
    // The record is gone, so no send can pick the bytes up any more. A file
    // left behind by a failed removal is unreachable and harmless. The
    // identity guard keeps a retried upload from recreating the record first.
    #[cfg(test)]
    attachments::removal_pause(&file.id).await;
    if file.relative_path.as_deref() == Some(attachment_relative_path(&file.id).as_str()) {
        if let Some(workspace) = projects::media_workspace(&state, &conversation_id).await {
            if let Some(path) = attachment_path(&workspace, &file.id, false).await {
                let _ = tokio::fs::remove_file(path).await;
            }
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn conversation_files(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
) -> Response {
    if !projects::is_project(&state, &conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state.store.list_conversation_files(&conversation_id).await {
        Ok(files) => Json(
            files
                .into_iter()
                .map(conversation_file_summary)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "files could not be loaded",
        )
            .into_response(),
    }
}

const PROJECT_OUTPUTS_PAGE: i64 = 30;
const PROJECT_OUTPUTS_MAX_PAGE: i64 = 50;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectOutputsQuery {
    /// `published` (default): what agents produced. `uploaded`: what the
    /// owner attached. `all`: both.
    #[serde(default)]
    kind: Option<String>,
    /// `all` (default), `pages`, `images` or `documents`.
    #[serde(default, rename = "type")]
    file_type: Option<String>,
    /// Part of the file name.
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectOutputSummary {
    id: String,
    name: String,
    mime_type: Option<String>,
    byte_size: Option<i64>,
    sha256: Option<String>,
    /// `gone` when the record remains but its bytes are no longer on this
    /// Mac as recorded; the index never stands in for them.
    availability: &'static str,
    /// `agent` or `owner`.
    origin: &'static str,
    conversation_id: String,
    conversation_title: String,
    replaces: Option<String>,
    pinned: bool,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectOutputsResponse {
    files: Vec<ProjectOutputSummary>,
    next_cursor: Option<String>,
}

fn project_output_cursor(cursor: &wonder_store::ProjectOutputCursor) -> String {
    let text =
        serde_json::to_vec(&(cursor.pinned, &cursor.updated_at, &cursor.id)).unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)
}

fn parse_project_output_cursor(cursor: &str) -> Option<wonder_store::ProjectOutputCursor> {
    if cursor.len() > 1024 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()?;
    let (pinned, updated_at, id): (bool, String, String) = serde_json::from_slice(&bytes).ok()?;
    Some(wonder_store::ProjectOutputCursor {
        pinned,
        updated_at,
        id,
    })
}

/// Whether an output's bytes are still where its record says, at its
/// recorded size. A cheap check of one path per listed file: no crawl and
/// no hashing (a download verifies the SHA-256).
async fn project_output_available(
    state: &AppState,
    workspaces: &mut HashMap<String, Option<String>>,
    file: &StoredConversationFile,
) -> bool {
    if file.state != "available" {
        return false;
    }
    let size_matches = |len: u64| file.byte_size.is_none_or(|size| size == len as i64);
    match file.relative_path.as_deref() {
        Some(relative_path) => {
            if file.kind != "attachment" || relative_path != attachment_relative_path(&file.id) {
                return false;
            }
            let workspace = match workspaces.get(&file.conversation_id) {
                Some(workspace) => workspace.clone(),
                None => {
                    let workspace = projects::media_workspace(state, &file.conversation_id).await;
                    workspaces.insert(file.conversation_id.clone(), workspace.clone());
                    workspace
                }
            };
            let Some(workspace) = workspace else {
                return false;
            };
            let Some(path) = attachment_path(&workspace, &file.id, false).await else {
                return false;
            };
            tokio::fs::metadata(path)
                .await
                .is_ok_and(|metadata| metadata.is_file() && size_matches(metadata.len()))
        }
        None => match state.store.media_source(&file.id).await {
            Ok(Some(source)) => match source.location {
                wonder_store::MediaSourceLocation::File { path } => tokio::fs::metadata(path)
                    .await
                    .is_ok_and(|metadata| metadata.is_file() && size_matches(metadata.len())),
                wonder_store::MediaSourceLocation::Transcript {
                    path,
                    offset,
                    length,
                } => tokio::fs::metadata(path).await.is_ok_and(|metadata| {
                    metadata.is_file() && offset.saturating_add(length) <= metadata.len()
                }),
            },
            _ => false,
        },
    }
}

/// A Project's Outputs: the files its threads' agents produced, across
/// every thread, pinned first and then newest. Each opens through its
/// thread's file route, which verifies the bytes.
async fn project_outputs(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(query): Query<ProjectOutputsQuery>,
) -> Response {
    let origin = match query.kind.as_deref().unwrap_or("published") {
        "published" => wonder_store::ProjectOutputOrigin::Agent,
        "uploaded" => wonder_store::ProjectOutputOrigin::Owner,
        "all" => wonder_store::ProjectOutputOrigin::Any,
        _ => return (StatusCode::BAD_REQUEST, "Unknown output kind.").into_response(),
    };
    let file_type = match query.file_type.as_deref().unwrap_or("all") {
        "all" => wonder_store::ProjectOutputType::All,
        "pages" => wonder_store::ProjectOutputType::Pages,
        "images" => wonder_store::ProjectOutputType::Images,
        "documents" => wonder_store::ProjectOutputType::Documents,
        _ => return (StatusCode::BAD_REQUEST, "Unknown output type.").into_response(),
    };
    let name = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    if name.is_some_and(|q| q.chars().count() > 256) {
        return (StatusCode::BAD_REQUEST, "The search is too long.").into_response();
    }
    let limit = query.limit.unwrap_or(PROJECT_OUTPUTS_PAGE);
    if !(1..=PROJECT_OUTPUTS_MAX_PAGE).contains(&limit) {
        return (StatusCode::BAD_REQUEST, "The page size is out of range.").into_response();
    }
    let after = match query.cursor.as_deref() {
        None => None,
        Some(cursor) => match parse_project_output_cursor(cursor) {
            Some(cursor) => Some(cursor),
            None => {
                return (StatusCode::BAD_REQUEST, "That page is no longer valid.").into_response()
            }
        },
    };
    match state.store.project(&project_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "outputs could not be loaded",
            )
                .into_response()
        }
    }
    let mut outputs = match state
        .store
        .project_outputs(
            &project_id,
            &wonder_store::ProjectOutputQuery {
                origin,
                file_type,
                name,
                after,
                limit: limit + 1,
            },
        )
        .await
    {
        Ok(outputs) => outputs,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "outputs could not be loaded",
            )
                .into_response()
        }
    };
    let more = outputs.len() as i64 > limit;
    outputs.truncate(limit as usize);
    let next_cursor = more.then(|| outputs.last()).flatten().map(|last| {
        project_output_cursor(&wonder_store::ProjectOutputCursor {
            pinned: last.pinned,
            updated_at: last.file.updated_at.clone(),
            id: last.file.id.clone(),
        })
    });
    let mut workspaces = HashMap::new();
    let mut files = Vec::with_capacity(outputs.len());
    for output in outputs {
        let available = project_output_available(&state, &mut workspaces, &output.file).await;
        let file = output.file;
        let owner = file
            .source_id
            .as_deref()
            .is_none_or(|source| source.starts_with("user-media:"));
        files.push(ProjectOutputSummary {
            id: file.id,
            name: file.name,
            mime_type: file.mime_type,
            byte_size: file.byte_size,
            sha256: file.sha256,
            availability: if available { "available" } else { "gone" },
            origin: if owner { "owner" } else { "agent" },
            conversation_id: file.conversation_id,
            conversation_title: output.conversation_title,
            replaces: output.replaces_file_id,
            pinned: output.pinned,
            created_at: file.created_at,
            updated_at: file.updated_at,
        });
    }
    Json(ProjectOutputsResponse { files, next_cursor }).into_response()
}

async fn set_project_output_pin(
    state: &AppState,
    project_id: &str,
    file_id: &str,
    pinned: bool,
) -> Response {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    match state
        .store
        .set_project_output_pinned(project_id, file_id, pinned, &now)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "The pin could not be saved.",
        )
            .into_response(),
    }
}

async fn pin_project_output(
    State(state): State<AppState>,
    Path((project_id, file_id)): Path<(String, String)>,
) -> Response {
    set_project_output_pin(&state, &project_id, &file_id, true).await
}

async fn unpin_project_output(
    State(state): State<AppState>,
    Path((project_id, file_id)): Path<(String, String)>,
) -> Response {
    set_project_output_pin(&state, &project_id, &file_id, false).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConversationFileQuery {
    /// The longest edge a phone shows: a raster picture larger than this
    /// comes back as a smaller JPEG (see `media_refs::preview_edge`).
    #[serde(default)]
    max_pixels: Option<u32>,
}

async fn conversation_file(
    State(state): State<AppState>,
    Path((conversation_id, file_id)): Path<(String, String)>,
    Query(query): Query<ConversationFileQuery>,
) -> Response {
    if !projects::is_project(&state, &conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file = match state
        .store
        .conversation_files_by_ids(&conversation_id, std::slice::from_ref(&file_id))
        .await
    {
        Ok(files) => files.into_iter().find(|file| file.id == file_id),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "file could not be loaded",
            )
                .into_response()
        }
    };
    let Some(file) = file else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !matches!(file.kind.as_str(), "attachment" | "artifact") || file.state != "available" {
        return StatusCode::NOT_FOUND.into_response();
    }
    enum Source {
        Attachment(PathBuf),
        /// Where the picture lives, and this chat's media (its retained preview).
        Reference(wonder_store::MediaSourceLocation, Option<PathBuf>),
    }
    let source = match file.relative_path.as_deref() {
        Some(relative_path) => {
            if file.kind != "attachment" || relative_path != attachment_relative_path(&file.id) {
                return StatusCode::NOT_FOUND.into_response();
            }
            let workspace = projects::media_workspace(&state, &conversation_id).await;
            let Some(workspace) = workspace else {
                return StatusCode::NOT_FOUND.into_response();
            };
            match locate_attachment(&workspace, &file.id, false).await {
                Ok(path) => Source::Attachment(path),
                Err(AttachmentPathError::Missing) => return media_refs::gone_response(),
                Err(AttachmentPathError::Unsafe) => return StatusCode::FORBIDDEN.into_response(),
                Err(AttachmentPathError::Io) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "file could not be loaded",
                    )
                        .into_response()
                }
            }
        }
        // A picture kept by reference: read where it lives on this Mac,
        // only if it is still exactly the original.
        None => {
            let source = match state.store.media_source(&file.id).await {
                Ok(Some(source)) => source,
                Ok(None) => return StatusCode::NOT_FOUND.into_response(),
                Err(_) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "file could not be loaded",
                    )
                        .into_response()
                }
            };
            let media = projects::media_workspace(&state, &conversation_id)
                .await
                .map(PathBuf::from);
            Source::Reference(source.location, media)
        }
    };
    let content_type = file
        .mime_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    let safe_name = file.name.replace(['"', '\\', '\r', '\n'], "_");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{safe_name}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    let edge = query
        .max_pixels
        .and_then(media_refs::preview_edge)
        .filter(|_| media_refs::resizable(content_type) && file.sha256.is_some());
    // Same-image requests wait before consuming a global slot. Keep both
    // guards with the blocking work, including original reads and hashing.
    let guards = if let Some(edge) = edge {
        Some(
            media_refs::preview_guard(&state.data_dir, file.sha256.as_deref().unwrap(), edge).await,
        )
    } else {
        None
    };
    let data_dir = state.data_dir.clone();
    let expected_size = file.byte_size;
    let expected_sha = file.sha256.clone();
    let generation = media_refs::generation(&state.data_dir);
    let file_id = file.id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _guards = guards;
        let bytes = match source {
            // Bounded, no-follow and hash-checked: a replaced file can't
            // hang this read or come back as the attachment.
            Source::Attachment(path) => {
                let (Some(size), Some(sha)) = (expected_size, expected_sha.as_deref()) else {
                    return Err(StatusCode::INTERNAL_SERVER_ERROR);
                };
                attachments::read_verified(&path, size, sha).map_err(|failure| match failure {
                    attachments::ReadFailure::Missing => StatusCode::GONE,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                })?
            }
            // read_source verifies size/hash once; carry that verification
            // into preview(), which keys the cache by the verified hash.
            Source::Reference(location, media) => {
                match media_refs::read_source(&location, expected_size, expected_sha.as_deref()) {
                    Some(bytes) => bytes,
                    // The original changed or is gone: history keeps its
                    // retained preview. Only the original's exact bytes
                    // are ever served as the file itself.
                    None => {
                        let kept = media
                            .and_then(|media| media_refs::retained(&media, &file_id))
                            .ok_or(StatusCode::GONE)?;
                        if Some(kept.sha256.as_str()) != expected_sha.as_deref() {
                            let edge = edge.ok_or(StatusCode::GONE)?;
                            let preview = match media_refs::shrink(&kept.bytes, edge) {
                                Some(bytes) => media_refs::Preview {
                                    sha256: hex::encode(Sha256::digest(&bytes)),
                                    bytes,
                                },
                                None => kept,
                            };
                            return Ok((Vec::new(), Some(preview), true));
                        }
                        kept.bytes
                    }
                }
            }
        };
        let preview = edge.and_then(|edge| {
            media_refs::preview(
                media_refs::cache_dir(&data_dir).as_deref(),
                expected_sha.as_deref().unwrap(),
                edge,
                &bytes,
                media_refs::PREVIEW_CACHE_BYTES,
                generation,
            )
        });
        Ok((bytes, preview, false))
    })
    .await;
    let (bytes, preview, original_gone) = match result {
        Ok(Ok(result)) => result,
        Ok(Err(StatusCode::GONE)) => return media_refs::gone_response(),
        Ok(Err(StatusCode::NOT_FOUND)) => return StatusCode::NOT_FOUND.into_response(),
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "file integrity check failed",
            )
                .into_response()
        }
    };
    if let Some(preview) = preview {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
        if let Ok(value) = HeaderValue::from_str(&preview.sha256) {
            headers.insert(media_refs::PREVIEW_SHA256_HEADER, value);
        }
        if original_gone {
            headers.insert(
                media_refs::ORIGINAL_GONE_HEADER,
                HeaderValue::from_static("1"),
            );
        }
        return (headers, Body::from(preview.bytes)).into_response();
    }
    original_response(headers, content_type, bytes)
}

fn original_response(mut headers: HeaderMap, content_type: &str, bytes: Vec<u8>) -> Response {
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    (headers, Body::from(bytes)).into_response()
}

/// Cancels the thread's scheduled resume after a usage limit.
async fn cancel_usage_resume(
    State(state): State<AppState>,
    Extension(_): Extension<OwnerAuthority>,
    Path(conversation_id): Path<String>,
) -> Response {
    match state.store.cancel_usage_resume(&conversation_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            "This thread has no scheduled resume.",
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The resume could not be cancelled. Try again.",
        )
            .into_response(),
    }
}

async fn send_message(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    local_authority: Option<Extension<LocalOwnerAuthority>>,
    Json(request): Json<SendMessageRequest>,
) -> Response {
    if request.device_id == "wonder-desktop" {
        if local_authority.is_none() {
            return StatusCode::FORBIDDEN.into_response();
        }
        if state
            .store
            .ensure_local_desktop(&now_ms().to_string())
            .await
            .is_err()
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Desktop identity could not be saved",
            )
                .into_response();
        }
    }
    send_message_inner(
        State(state),
        Path(conversation_id),
        authenticated_device,
        Json(request),
    )
    .await
}

/// New content may never enter a retained provider home, including through
/// Guide/steer. Checking before acceptance also preserves the saved transcript.
async fn legacy_history_refusal(state: &AppState, conversation_id: &str) -> Option<Response> {
    match state.store.project_conversation(conversation_id).await {
        Ok(Some(conversation))
            if !providers::is_default_store(
                state,
                conversation.family,
                &conversation.provider_store,
            ) =>
        {
            Some(projects::error(
                StatusCode::CONFLICT,
                providers::other_history(conversation.family),
            ))
        }
        Err(_) => Some(projects::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "This chat could not be checked. Try again.",
        )),
        _ => None,
    }
}

async fn send_message_inner(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    Json(request): Json<SendMessageRequest>,
) -> Response {
    let _conversation = state.projects.conversation_dispatch(&conversation_id).await;
    if let Some(response) = subagents::reject_user_mutation(&state, &conversation_id).await {
        return response;
    }
    if request.body.trim().is_empty() && request.attachment_ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "Write a message or attach a file to send.",
        )
            .into_response();
    }
    if request.body.len() > 64 * 1024 {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            "This message is too long to send. Keep it under 64 KB.",
        )
            .into_response();
    }
    if !valid_attachment_ids(&request.attachment_ids) {
        return (StatusCode::BAD_REQUEST, "invalid attachmentIds").into_response();
    }
    if uuid::Uuid::parse_str(&request.client_message_id).is_err() {
        return (StatusCode::BAD_REQUEST, "clientMessageId must be a UUID").into_response();
    }
    if let Some(Extension(AuthenticatedDevice { device_id, .. })) = authenticated_device {
        if request.device_id != device_id {
            return (StatusCode::FORBIDDEN, "device identity mismatch").into_response();
        }
    }
    if !projects::is_project(&state, &conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(response) = legacy_history_refusal(&state, &conversation_id).await {
        return response;
    }
    if let Err(error) = codex_skills::validate_acceptance(
        &state,
        &conversation_id,
        &request.device_id,
        &request.client_message_id,
        request.project_model.as_ref().map(|m| m.family),
        &request.body,
        &request.codex_skills,
    )
    .await
    {
        return crate::codex_skills::rejection_response(error);
    }
    let body_sha256 = hex::encode(Sha256::digest(request.body.as_bytes()));
    {
        if let Err(response) = provider_switch::stage_message_model(
            &state,
            &conversation_id,
            &request.device_id,
            &request.client_message_id,
            request.project_model.as_ref(),
        )
        .await
        {
            return response;
        }
    }
    if !request.attachment_ids.is_empty() {
        if let Err((status, detail)) = artifact_annotations::validate_first_acceptance(
            &state,
            &conversation_id,
            &request.device_id,
            &request.client_message_id,
            &request.attachment_ids,
        )
        .await
        {
            return (status, detail).into_response();
        }
    }
    let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let insert = match state
        .store
        .insert_message_with_skills(
            &request.device_id,
            &request.client_message_id,
            &request.body,
            &body_sha256,
            &conversation_id,
            &request.attachment_ids,
            &created_at,
            None,
            &request.codex_skills,
        )
        .await
    {
        Ok(insert) => insert,
        Err(sqlx::Error::Protocol(message)) if message == "annotation_already_sent" => {
            return (
                StatusCode::CONFLICT,
                "This annotation was already sent. Add a new annotation to send another note.",
            )
                .into_response();
        }
        Err(error) if wonder_store::is_rewind_pending(&error) => return rewind::recovering(),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    };
    let stored = match insert {
        MessageInsert::Inserted(message) => {
            // The owner continued the thread; a scheduled resume would repeat it.
            let _ = state.store.cancel_usage_resume(&conversation_id).await;
            message
        }
        MessageInsert::Existing(message) => message,
        MessageInsert::Conflict => {
            return (
                StatusCode::CONFLICT,
                [(header::CACHE_CONTROL, "no-store")],
                Json(serde_json::json!({ "error": "idempotency_conflict" })),
            )
                .into_response()
        }
    };
    if request.take_over && stored.state == "accepted_by_wonder" {
        // T3 Code restarts its own turn with the next waiting message when
        // it can carry it; nothing on the Mac is stopped. Otherwise the app
        // holding the chat is ended first.
        if projects::t3_code_takes_next(&state, &conversation_id).await {
            projects::request_t3_restart(&state, &conversation_id);
        } else {
            projects::request_take_over(&state, &conversation_id, &stored.id);
        }
    }
    let Some(receipt) = queue::receipt(&state, stored.clone()).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "stored message has an unknown delivery state",
        )
            .into_response();
    };
    state.dispatch_wake.notify_one();
    (
        StatusCode::ACCEPTED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(receipt),
    )
        .into_response()
}

async fn steer_turn(
    State(state): State<AppState>,
    Path((conversation_id, turn_id)): Path<(String, String)>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    Json(request): Json<SteerTurnRequest>,
) -> Response {
    if let Some(response) = subagents::reject_user_mutation(&state, &conversation_id).await {
        return response;
    }
    if (request.body.trim().is_empty() && request.attachment_ids.is_empty())
        || request.body.len() > 65536
        || !valid_attachment_ids(&request.attachment_ids)
        || uuid::Uuid::parse_str(&request.client_message_id).is_err()
        || request.expected_turn_id != turn_id
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if authenticated_device.is_some_and(|Extension(device)| device.device_id != request.device_id) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(response) = legacy_history_refusal(&state, &conversation_id).await {
        return response;
    }
    if let Err(error) = codex_skills::validate_acceptance(
        &state,
        &conversation_id,
        &request.device_id,
        &request.client_message_id,
        None,
        &request.body,
        &request.codex_skills,
    )
    .await
    {
        return crate::codex_skills::rejection_response(error);
    }
    let hash = hex::encode(Sha256::digest(request.body.as_bytes()));
    // A turn running in T3 Code: the message goes into its thread there as
    // a steer of that turn (`projects::request_t3_steer`), not into a Cupola
    // turn.
    let cupola_turn = matches!(state.store.message_for_codex_turn(&turn_id).await,
        Ok(Some(m)) if m.conversation_id == conversation_id);
    if !cupola_turn
        && projects::t3_code_thread(&state, &conversation_id)
            .await
            .is_some()
    {
        return steer_through_t3_code(&state, &conversation_id, &turn_id, &request, &hash).await;
    }
    let mut ended = false;
    match state
        .store
        .message_by_device_and_client_message_id(&request.device_id, &request.client_message_id)
        .await
        .ok()
        .flatten()
    {
        // A replay of a Guide that already moved to the queue gets its receipt.
        Some(existing)
            if existing.conversation_id == conversation_id
                && existing.body_sha256 == hash
                && state
                    .store
                    .is_durable_dispatch(&existing.id)
                    .await
                    .unwrap_or(false) =>
        {
            return match queue::receipt(&state, existing).await {
                Some(receipt) => (StatusCode::ACCEPTED, Json(receipt)).into_response(),
                None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            };
        }
        Some(_) => {}
        None => {
            let active = matches!(state.store.message_for_codex_turn(&turn_id).await,
                Ok(Some(m)) if m.conversation_id == conversation_id
                    && matches!(m.state.as_str(), "accepted_by_codex" | "streaming"));
            // The reply ended before Send now arrived: the message is queued
            // and sent next instead (see `queue_undeliverable_guide`).
            ended = !active;
        }
    }
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    match state
        .store
        .insert_message_with_skills(
            &request.device_id,
            &request.client_message_id,
            &request.body,
            &hash,
            &conversation_id,
            &request.attachment_ids,
            &now,
            Some(&turn_id),
            &request.codex_skills,
        )
        .await
    {
        Ok(MessageInsert::Inserted(message)) if ended => {
            queue_undeliverable_guide(&state, &message).await
        }
        Ok(MessageInsert::Inserted(message) | MessageInsert::Existing(message)) => {
            // Steer now rather than on the dispatch loop's next tick: the
            // phone shows "Sending…" until the agent takes the message.
            state.dispatch_wake.notify_one();
            match message_receipt(message) {
                Some(receipt) => (StatusCode::ACCEPTED, Json(receipt)).into_response(),
                None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        Ok(MessageInsert::Conflict) => StatusCode::CONFLICT.into_response(),
        Err(error) if wonder_store::is_rewind_pending(&error) => rewind::recovering(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Send now on a thread whose turn runs in T3 Code: the message waits like
/// any other, marked to steer, and dispatch sends it into the T3 thread.
async fn steer_through_t3_code(
    state: &AppState,
    conversation_id: &str,
    turn_id: &str,
    request: &SteerTurnRequest,
    hash: &str,
) -> Response {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    match state
        .store
        .insert_message_with_skills(
            &request.device_id,
            &request.client_message_id,
            &request.body,
            hash,
            conversation_id,
            &request.attachment_ids,
            &now,
            None,
            &request.codex_skills,
        )
        .await
    {
        Ok(MessageInsert::Inserted(message) | MessageInsert::Existing(message)) => {
            if message.state == "accepted_by_wonder" {
                projects::request_t3_steer(state, &message.id, turn_id);
            }
            state.dispatch_wake.notify_one();
            match queue::receipt(state, message).await {
                Some(receipt) => (StatusCode::ACCEPTED, Json(receipt)).into_response(),
                None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        Ok(MessageInsert::Conflict) => StatusCode::CONFLICT.into_response(),
        Err(error) if wonder_store::is_rewind_pending(&error) => rewind::recovering(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn dispatch_guide(
    state: AppState,
    steered_message: wonder_store::StoredMessage,
    turn_id: String,
) -> Response {
    let conversation_id = steered_message.conversation_id.clone();
    let _conversation = state.projects.conversation_dispatch(&conversation_id).await;
    let mut dispatch_guard = Some(
        state
            .update_admission
            .dispatch_guard(&state.dispatch_lock)
            .await,
    );
    let target = async {
        if state
            .store
            .subagent_ownership_for_conversation(&conversation_id)
            .await
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Err(
                "Agent tasks are read-only. Continue in the parent conversation.".to_owned(),
            );
        }
        let active_message = match state.store.message_for_codex_turn(&turn_id).await {
            Ok(Some(m))
                if m.conversation_id == conversation_id
                    && matches!(m.state.as_str(), "accepted_by_codex" | "streaming") =>
            {
                m
            }
            _ => return Err("This work is no longer active".to_owned()),
        };
        let thread_id = active_message
            .codex_thread_id
            .ok_or("This work has no runtime thread")?;
        if !state
            .store
            .verified_project_runtime_owner(&conversation_id, &thread_id)
            .await
            .map_err(|e| e.to_string())?
        {
            return Err("This work no longer belongs to the Project".into());
        }
        let family = claude::conversation_family(&state, &conversation_id).await?;
        // Without Send now the message waits for the reply to end.
        if !family.provider().capabilities.steer {
            return Err(format!(
                "{} can't take a message during a reply",
                family.provider().display_name
            ));
        }
        let rpc = claude::for_thread(&state, &thread_id).await?;
        Ok::<_, String>((thread_id, rpc, family))
    }
    .await;
    let (active_thread_id, runtime, family) = match target {
        Ok(target) => target,
        // The reply it was sent into ended first: it waits in the queue.
        Err(_) => return queue_undeliverable_guide(&state, &steered_message).await,
    };
    let attachments = match state
        .store
        .attachments_for_message(&steered_message.id)
        .await
    {
        Ok(attachments) => attachments,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "message attachments could not be loaded",
            )
                .into_response()
        }
    };
    let workspace = if projects::is_project(&state, &conversation_id).await {
        match projects::media_workspace(&state, &conversation_id).await {
            Some(workspace) => workspace,
            None => return StatusCode::NOT_FOUND.into_response(),
        }
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let source = state
        .store
        .message_source(&steered_message.id)
        .await
        .ok()
        .flatten();
    // Native slash actions need their own receipt, so Guide queues them.
    if family == AgentFamily::Codex
        && !matches!(
            composer_commands::codex_request(
                &steered_message.body,
                &active_thread_id,
                !attachments.is_empty()
            ),
            Ok(None)
        )
    {
        return queue_undeliverable_guide(&state, &steered_message).await;
    }
    let referenced_body = match composer_mentions::provider_text(&state, &steered_message).await {
        Ok(body) => body,
        // A reference that cannot be resolved is for the sender to fix; a
        // queued copy would only fail the same way later.
        Err(detail) => {
            let _ = state
                .store
                .update_message_delivery(&steered_message.id, "failed", None, None)
                .await;
            publish_message_state(&state, &steered_message, DeliveryState::Failed, None, None)
                .await;
            return (StatusCode::UNPROCESSABLE_ENTITY, detail).into_response();
        }
    };
    let body = thread_tools::provider_text(source.as_ref(), &referenced_body);
    let attachments = match agent_images::for_agent(&workspace, attachments).await {
        Ok(attachments) => attachments,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error).into_response(),
    };
    let mut input = match turn_input(&body, &workspace, &attachments).await {
        Ok(input) => input,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error).into_response(),
    };
    match codex_skills::message_input(&state, &steered_message, Some(family)).await {
        Ok(skills) => input.extend(skills),
        // No native input was accepted. Preserve the typed selection through
        // the existing Guide-to-queue owner; dispatch validates it again.
        Err(_) => return queue_undeliverable_guide(&state, &steered_message).await,
    }
    if !state
        .store
        .claim_guide(&steered_message.id, &active_thread_id)
        .await
        .unwrap_or(false)
    {
        return StatusCode::CONFLICT.into_response();
    }
    let response = runtime
        .request(
            "turn/steer",
            steer_turn_params_with_attachments(
                &active_thread_id,
                &turn_id,
                &steered_message.client_message_id,
                input,
            ),
        )
        .await;
    let response = match response {
        Ok(response) if response.error.is_none() => response,
        Ok(response) => {
            // The Claude bridge refuses before it accepts anything, so its
            // refusal is final; the message did not reach Claude.
            if response.error.as_ref().is_some_and(|error| {
                projects::input_rejection(family, "turn/steer", error)
                    == Some(projects::InputRejection::Wait)
            }) {
                return queue_undeliverable_guide(&state, &steered_message).await;
            }
            let _ = state
                .store
                .update_message_delivery(&steered_message.id, "uncertain", None, None)
                .await;
            return (StatusCode::BAD_GATEWAY, "App Server rejected steer").into_response();
        }
        Err(_) => {
            let _ = state
                .store
                .update_message_delivery(&steered_message.id, "uncertain", None, None)
                .await;
            drop(dispatch_guard);
            match restart_and_reconcile(&state, &steered_message).await {
                Some(ReconcileResult::Accepted { thread_id, turn_id }) => {
                    if !accept_reconciled_guide(&state, &steered_message, &thread_id, &turn_id)
                        .await
                    {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            "App Server steer remains uncertain",
                        )
                            .into_response();
                    }
                    return (
                        StatusCode::ACCEPTED,
                        [(header::CACHE_CONTROL, "no-store")],
                        Json(ClientMessageReceipt {
                            client_message_id: steered_message.client_message_id,
                            wonder_message_id: steered_message.id,
                            body_sha256: steered_message.body_sha256,
                            conversation_id: steered_message.conversation_id,
                            delivery_state: DeliveryState::AcceptedByCodex,
                            codex_thread_id: Some(thread_id),
                            codex_turn_id: Some(turn_id),
                            queued: None,
                        }),
                    )
                        .into_response();
                }
                None => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "App Server steer remains uncertain",
                    )
                        .into_response();
                }
            }
        }
    };
    let Some(next_turn_id) = steer_response_turn_id(&response) else {
        let _ = state
            .store
            .update_message_delivery(&steered_message.id, "uncertain", None, None)
            .await;
        return (
            StatusCode::BAD_GATEWAY,
            "App Server steer returned no turn id",
        )
            .into_response();
    };
    let thread_id = Some(active_thread_id);
    // Project recovery leaves a running message alone only while the runtime
    // that accepted it is still live; without this a Guide read as uncertain.
    if projects::is_project(&state, &conversation_id).await {
        projects::record_generation(&state, &steered_message.id, runtime.health().id());
    }
    let _ = state
        .store
        .update_message_delivery(
            &steered_message.id,
            "accepted_by_codex",
            thread_id.as_deref(),
            Some(&next_turn_id),
        )
        .await;
    publish_message_state(
        &state,
        &steered_message,
        DeliveryState::AcceptedByCodex,
        thread_id.as_deref(),
        Some(&next_turn_id),
    )
    .await;
    // Claude answers Guide as its own turn (Codex joins the running one):
    // events for it that arrived before this receipt was stored apply now.
    if next_turn_id != turn_id {
        if let Some(thread) = thread_id.as_deref() {
            drain_pending_with_dispatch(&state, thread, &next_turn_id, &mut dispatch_guard).await;
        }
    }
    (
        StatusCode::ACCEPTED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(ClientMessageReceipt {
            client_message_id: steered_message.client_message_id,
            wonder_message_id: steered_message.id,
            body_sha256: steered_message.body_sha256,
            conversation_id: steered_message.conversation_id,
            delivery_state: DeliveryState::AcceptedByCodex,
            codex_thread_id: thread_id,
            codex_turn_id: Some(next_turn_id),
            queued: None,
        }),
    )
        .into_response()
}

/// A Guide that cannot reach the reply it was sent into (the reply ended, or
/// the runtime refused it before reading it) is not a failure: it waits at
/// the end of the conversation's queue and is sent like any queued message.
/// An agent task's Guide, which has no queue of its own, still fails.
async fn queue_undeliverable_guide(
    state: &AppState,
    guide: &wonder_store::StoredMessage,
) -> Response {
    if state.store.guide_to_queue(&guide.id).await.unwrap_or(false) {
        publish_message_state(state, guide, DeliveryState::AcceptedByWonder, None, None).await;
        state.dispatch_wake.notify_one();
        let receipt = match state.store.message_by_id(&guide.id).await {
            Ok(Some(message)) => queue::receipt(state, message).await,
            _ => None,
        };
        return match receipt {
            Some(receipt) => (StatusCode::ACCEPTED, Json(receipt)).into_response(),
            None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    let _ = state
        .store
        .update_message_delivery(&guide.id, "failed", None, None)
        .await;
    publish_message_state(state, guide, DeliveryState::Failed, None, None).await;
    StatusCode::CONFLICT.into_response()
}

#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InterruptTurnRequest {
    owner_action_id: Option<String>,
}

async fn interrupt_turn_http(
    state: State<AppState>,
    path: Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    // Empty bodies and {} are legacy single Stops, never fresh escalation IDs.
    let request: InterruptTurnRequest = if body.is_empty() {
        InterruptTurnRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(_) => return (StatusCode::BAD_REQUEST, "Invalid Stop action").into_response(),
        }
    };
    if let Some(id) = &request.owner_action_id {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return (StatusCode::BAD_REQUEST, "Invalid Stop action").into_response();
        }
    }
    interrupt_turn_with_action(state, path, request.owner_action_id).await
}

pub(crate) async fn interrupt_turn(
    state: State<AppState>,
    path: Path<(String, String)>,
) -> Response {
    // Internal stops (including update recovery) are not additional owner taps.
    interrupt_turn_with_action(state, path, None).await
}

async fn interrupt_turn_with_action(
    State(state): State<AppState>,
    Path((conversation_id, turn_id)): Path<(String, String)>,
    owner_action_id: Option<String>,
) -> Response {
    // A turn running in T3 Code (not one Cupola started): T3 Code stops it.
    let cupola_turn = matches!(state.store.message_for_codex_turn(&turn_id).await,
        Ok(Some(message)) if message.conversation_id == conversation_id);
    if !cupola_turn {
        if let Some(response) =
            projects::interrupt_through_t3_code(&state, &conversation_id, &turn_id).await
        {
            return response;
        }
    }
    let _conversation = state.projects.conversation_dispatch(&conversation_id).await;
    let _dispatch = state.dispatch_lock.lock().await;
    if let Some(response) = subagents::reject_user_mutation(&state, &conversation_id).await {
        return response;
    }
    match subagents::runtime_for_conversation(&state, &conversation_id).await {
        Ok(Some(child)) => {
            if state
                .store
                .cancel_update_handoff_tree(&conversation_id, &child.ownership.thread_id, &turn_id)
                .await
                .is_err()
            {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            match child_turn_status(&child, &turn_id).await {
                Ok(Some(status))
                    if matches!(status.as_str(), "interrupted" | "completed" | "failed") =>
                {
                    return StatusCode::NO_CONTENT.into_response()
                }
                Ok(Some(status))
                    if status == "inProgress"
                        && child
                            .thread
                            .pointer("/status/type")
                            .and_then(serde_json::Value::as_str)
                            == Some("active") => {}
                _ => {
                    return (
                        StatusCode::CONFLICT,
                        "This subagent's turn is no longer active",
                    )
                        .into_response()
                }
            }
            let response = child
                .rpc
                .request(
                    "turn/interrupt",
                    serde_json::json!({
                        "threadId": child.ownership.thread_id, "turnId": turn_id
                    }),
                )
                .await;
            match response {
                Ok(response) if response.error.is_none() => {}
                _ => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "The subagent could not be stopped. Refresh its conversation.",
                    )
                        .into_response()
                }
            }
            // There may be no user receipt: spawned work belongs to the child
            // through verified runtime metadata, not a fabricated user message.
            if let Ok(Some(message)) = state.store.message_for_codex_turn(&turn_id).await {
                if message.conversation_id == conversation_id
                    && message.codex_thread_id.as_deref()
                        == Some(child.ownership.thread_id.as_str())
                    && state
                        .store
                        .interrupt_message_if_active(&message.id)
                        .await
                        .unwrap_or(false)
                {
                    publish_message_state(
                        &state,
                        &message,
                        DeliveryState::Interrupted,
                        Some(&child.ownership.thread_id),
                        Some(&turn_id),
                    )
                    .await;
                }
            }
            return StatusCode::NO_CONTENT.into_response();
        }
        Ok(None) => {}
        Err(_) => {
            return (
                StatusCode::CONFLICT,
                "This subagent is unavailable. Reopen its parent conversation.",
            )
                .into_response()
        }
    }
    let message = match state.store.message_for_codex_turn(&turn_id).await {
        Ok(Some(message)) if message.conversation_id == conversation_id => message,
        Ok(Some(_)) => return StatusCode::NOT_FOUND.into_response(),
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "turn lookup failed").into_response(),
    };
    let Some(thread_id) = message.codex_thread_id.as_deref() else {
        return (StatusCode::CONFLICT, "turn has no Codex thread").into_response();
    };
    if state
        .store
        .cancel_update_handoff_tree(&conversation_id, thread_id, &turn_id)
        .await
        .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    // A turn that already finished (any way) has nothing left to stop.
    if matches!(
        message.state.as_str(),
        "interrupted" | "completed" | "failed"
    ) {
        return StatusCode::NO_CONTENT.into_response();
    }
    if !matches!(message.state.as_str(), "accepted_by_codex" | "streaming") {
        return (StatusCode::CONFLICT, "turn is no longer active").into_response();
    }
    if let Some(response) =
        projects::stop_desktop_relayed(&state, &message, thread_id, &turn_id).await
    {
        return response;
    }
    let family = match claude::conversation_family(&state, &conversation_id).await {
        Ok(family) => family,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    };
    let acp = matches!(family, AgentFamily::OpenCode | AgentFamily::Cursor);
    let runtime = match claude::for_thread(&state, thread_id).await {
        Ok(runtime) => runtime,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    };
    // Saved before the provider is asked: a host restart between the provider
    // accepting the interrupt and its turn/completed must still read the turn
    // as the owner's Stop. ACP stops are confirmed in the response itself.
    let own_stop = if acp {
        false
    } else {
        match projects::request_stop(&state, thread_id, &turn_id).await {
            Ok(own) => own,
            Err(_) => {
                return (StatusCode::SERVICE_UNAVAILABLE, projects::STOP_NOT_SAVED).into_response()
            }
        }
    };
    let mut params = serde_json::json!({ "threadId": thread_id, "turnId": turn_id });
    // This extension is owned by the Claude bridge, not Codex or ACP's schema.
    if family == AgentFamily::Claude {
        if let Some(id) = owner_action_id {
            params["ownerActionId"] = serde_json::json!(id);
        }
    }
    let response = runtime.request("turn/interrupt", params).await;
    let Ok(response) = response else {
        if own_stop {
            projects::clear_stop(&state, thread_id, &turn_id).await;
        }
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "App Server interrupt failed",
        )
            .into_response();
    };
    if acp {
        return match claude::confirmed_acp_stop(&response, &turn_id) {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(error) => (StatusCode::BAD_GATEWAY, error).into_response(),
        };
    }
    if response.error.is_some() {
        if own_stop {
            projects::clear_stop(&state, thread_id, &turn_id).await;
        }
        return (StatusCode::BAD_GATEWAY, "App Server rejected interrupt").into_response();
    }
    // Codex and the Claude bridge acknowledge turn/interrupt before the turn
    // ends; its own turn/completed (normally interrupted) settles the message.
    // Until then the turn keeps running, and a provider that never confirms is
    // settled by recovery from native history, never assumed stopped here.
    match state.store.message_by_id(&message.id).await {
        Ok(Some(current))
            if !matches!(current.state.as_str(), "accepted_by_codex" | "streaming") =>
        {
            StatusCode::NO_CONTENT.into_response()
        }
        _ => (
            StatusCode::ACCEPTED,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({ "stopConfirmed": false })),
        )
            .into_response(),
    }
}

async fn retry_message(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    let Some(Extension(AuthenticatedDevice { device_id, .. })) = authenticated_device else {
        return (StatusCode::UNAUTHORIZED, "paired device required").into_response();
    };
    let message = match state.store.message_by_id(&message_id).await {
        Ok(Some(message)) => message,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "message lookup failed").into_response()
        }
    };
    if let Some(response) = subagents::reject_user_mutation(&state, &message.conversation_id).await
    {
        return response;
    }
    if message.device_id != device_id {
        return (StatusCode::FORBIDDEN, "device identity mismatch").into_response();
    }
    if !matches!(message.state.as_str(), "safe_to_retry" | "failed") {
        return (
            StatusCode::CONFLICT,
            Json(
                serde_json::json!({ "error": "message_not_safe_to_retry", "state": message.state }),
            ),
        )
            .into_response();
    }
    if !state
        .store
        .is_durable_dispatch(&message.id)
        .await
        .unwrap_or(false)
    {
        return (
            StatusCode::CONFLICT,
            "Send a new message for this kind of work",
        )
            .into_response();
    }
    match state.store.requeue_message_for_retry(&message.id).await {
        Ok(true) => {
            state.dispatch_wake.notify_one();
            publish_message_state(
                &state,
                &message,
                DeliveryState::AcceptedByWonder,
                None,
                None,
            )
            .await;
            (
                StatusCode::ACCEPTED,
                [(header::CACHE_CONTROL, "no-store")],
                Json(ClientMessageReceipt {
                    client_message_id: message.client_message_id,
                    wonder_message_id: message.id,
                    body_sha256: message.body_sha256,
                    conversation_id: message.conversation_id,
                    delivery_state: DeliveryState::AcceptedByWonder,
                    codex_thread_id: message.codex_thread_id,
                    codex_turn_id: message.codex_turn_id,
                    queued: None,
                }),
            )
                .into_response()
        }
        Ok(false) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "message_not_safe_to_retry" })),
        )
            .into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "message retry failed").into_response(),
    }
}

pub fn spawn_automation_scheduler(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            automation_scheduler_tick(&state).await;
        }
    })
}

async fn automation_scheduler_tick(state: &AppState) {
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return;
    };
    let now = Utc::now();
    let now_string = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    if state
        .store
        .reconcile_automation_runs(&now_string)
        .await
        .is_err()
    {
        return;
    }
    // Accepted work can outlive a failed materialization at any point in this
    // daemon lifetime. Linked messages already belong to durable dispatch.
    let Ok(runs) = state.store.recoverable_automation_runs().await else {
        return;
    };
    for (run, automation) in runs {
        let ready = if matches!(
            &automation.target,
            wonder_store::AutomationTarget::ProjectThread { .. }
        ) {
            projects::ready(state).await
        } else {
            state.ingestion.readiness(&state.store).await.ready
        };
        if !ready {
            continue;
        }
        tokio::spawn(run_automation(
            state.clone(),
            automation,
            run.scheduled_for,
            run.id,
        ));
    }
    let Ok(due) = state.store.list_due_automations(&now_string).await else {
        return;
    };
    for automation in due {
        let Some(scheduled_for) = automation.next_run_at.clone() else {
            continue;
        };
        let ready = if matches!(
            &automation.target,
            wonder_store::AutomationTarget::ProjectThread { .. }
        ) {
            projects::ready(state).await
        } else {
            state.ingestion.readiness(&state.store).await.ready
        };
        if !ready {
            continue;
        }
        if automation_target_available(
            state,
            &automation.target,
            &automation.kind,
            automation.conversation_id.as_deref(),
        )
        .await
        .is_err()
        {
            continue;
        }
        // One catch-up at most. While a run is active its due timestamp
        // stays pending; after completion advance directly beyond now.
        let Ok(next) = next_automation_run(&automation.rrule, &automation.timezone, now) else {
            continue;
        };
        let run_id = uuid::Uuid::new_v4().to_string();
        if !matches!(
            state
                .store
                .claim_scheduled_automation(
                    &run_id,
                    &automation,
                    &scheduled_for,
                    &now_string,
                    next.as_deref(),
                    true
                )
                .await,
            Ok(true)
        ) {
            continue;
        }
        tokio::spawn(run_automation(
            state.clone(),
            automation,
            scheduled_for,
            run_id,
        ));
    }
}

async fn run_automation(
    state: AppState,
    automation: wonder_store::StoredAutomation,
    scheduled_for: String,
    run_id: String,
) {
    // Scheduler recovery can race a newly accepted run-now task. Serialize the
    // unmaterialized phase with admission, and recheck under that same barrier
    // before target validation can fail or another executor can dispatch it.
    let Some(admission) = state.update_admission.claim_guard().await else {
        return;
    };
    let Ok(Some(run)) = state.store.automation_run_by_id(&run_id).await else {
        return;
    };
    if run.status != "running" || run.message_id.is_some() {
        return;
    }
    // An agent's schedule never runs with more access than the chat that
    // made it, even if its destination chat was widened since.
    let available = match automation_target_available(
        &state,
        &automation.target,
        &automation.kind,
        automation.conversation_id.as_deref(),
    )
    .await
    {
        Ok(()) => thread_tools::schedule::run_authority(&state, &automation).await,
        Err((_, error)) => Err(error),
    };
    if let Err(error) = available {
        let _ = state
            .store
            .finish_automation_run(
                &run_id,
                "failed",
                &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                Some(error),
                None,
            )
            .await;
        return;
    }
    let conversation_id = match &automation.target {
        wonder_store::AutomationTarget::ProjectThread { scope_id } => scope_id.clone(),
    };
    let Some(device_id) = state
        .store
        .automation_message_device_id()
        .await
        .ok()
        .flatten()
    else {
        let _ = state
            .store
            .finish_automation_run(
                &run_id,
                "failed",
                &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                Some("This automation needs a paired phone or iPad. Pair one again with cupola pair on your computer."),
                None,
            )
            .await;
        return;
    };
    // The message, durable queue entry and run link commit together. A crash
    // can only leave accepted work or no materialized work, never an orphan.
    if state
        .store
        .materialize_automation_message(
            &run_id,
            &device_id,
            &automation,
            &conversation_id,
            &scheduled_for,
        )
        .await
        .is_err()
    {
        return; // The next scheduler tick retries the durable accepted run.
    }
    // The shared queue owns held-thread checks, ordering and dispatch admission,
    // just as it does for owner sends. Linked runs must never dispatch directly.
    drop(admission);
    state.dispatch_wake.notify_one();
}

fn inherited_child_resume_params(thread_id: &str) -> serde_json::Value {
    // Omission is intentional: parent instructions, model and permissions
    // must never replace the configuration of an existing child thread.
    serde_json::json!({"threadId": thread_id, "excludeTurns": true})
}

async fn child_turn_status(
    child: &subagents::ChildRuntime,
    turn_id: &str,
) -> Result<Option<String>, String> {
    let response = child
        .rpc
        .request(
            "thread/turns/list",
            serde_json::json!({
                "threadId": child.ownership.thread_id, "limit": 100,
                "sortDirection": "desc", "itemsView": "notLoaded"
            }),
        )
        .await
        .map_err(|error| error.to_string())?;
    if response.error.is_some() {
        return Err("The subagent's active work could not be checked".into());
    }
    Ok(response
        .result
        .as_ref()
        .and_then(|value| value.get("data"))
        .and_then(serde_json::Value::as_array)
        .and_then(|turns| {
            turns
                .iter()
                .find(|turn| turn.get("id").and_then(serde_json::Value::as_str) == Some(turn_id))
        })
        .and_then(|turn| turn.get("status").and_then(serde_json::Value::as_str))
        .map(str::to_owned))
}

enum ReconcileResult {
    Accepted { thread_id: String, turn_id: String },
}

async fn accept_reconciled_guide(
    state: &AppState,
    message: &wonder_store::StoredMessage,
    thread: &str,
    turn: &str,
) -> bool {
    let Some(_admission) = state.update_admission.claim_guard().await else {
        return false;
    };
    let _dispatch = state.dispatch_lock.lock().await;
    let Ok(Some(current)) = state.store.message_by_id(&message.id).await else {
        return false;
    };
    if current.state != "uncertain"
        || current.conversation_id != message.conversation_id
        || current.client_message_id != message.client_message_id
        || current.body_sha256 != message.body_sha256
        || current.codex_thread_id.as_deref() != Some(thread)
        || current.codex_turn_id.is_some()
    {
        return false;
    }
    if state
        .store
        .update_message_delivery(&message.id, "accepted_by_codex", Some(thread), Some(turn))
        .await
        .is_err()
    {
        return false;
    }
    ingestion::request_conversation_reconcile(state, &current.conversation_id).await;
    publish_message_state(
        state,
        &current,
        DeliveryState::AcceptedByCodex,
        Some(thread),
        Some(turn),
    )
    .await;
    true
}

async fn restart_and_reconcile(
    state: &AppState,
    message: &wonder_store::StoredMessage,
) -> Option<ReconcileResult> {
    let turn_id = dispatch::find_accepted_turn(state, message).await.ok()??;
    Some(ReconcileResult::Accepted {
        thread_id: message.codex_thread_id.clone()?,
        turn_id,
    })
}

async fn rediscover_runtime(
    state: &AppState,
    app_server: &mut AppServerClient,
) -> Result<(), String> {
    // Usage is optional and may fail independently (for example, a revoked
    // sign-in on a fresh host). The usage endpoint owns that error; it must
    // not discard a healthy process or hide the models needed to start setup.
    let response = app_server
        .request("account/read", serde_json::json!({}))
        .await
        .map_err(|e| e.to_string())?;
    if response.error.is_some() || response.result.is_none() {
        return Err("account/read is unavailable".into());
    }
    let mut catalog = RuntimeCatalog::default();
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    loop {
        let mut params = serde_json::json!({"limit": 100});
        if let Some(value) = cursor.as_deref() {
            params["cursor"] = serde_json::json!(value);
        }
        let response = app_server
            .request("model/list", params)
            .await
            .map_err(|e| e.to_string())?;
        if response.error.is_some() {
            return Err("Model discovery failed".into());
        }
        let result = response.result.ok_or("model/list returned no result")?;
        catalog.apply_models_page(AgentFamily::Codex, &result);
        cursor = result
            .get("nextCursor")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let Some(value) = cursor.as_ref() else {
            break;
        };
        if !seen.insert(value.clone()) {
            return Err("Repeated model cursor".into());
        }
    }
    let mut current = state.runtime_catalog.write().await;
    catalog.models.extend(
        current
            .models
            .iter()
            .filter(|m| m.agent_family != AgentFamily::Codex)
            .cloned(),
    );
    catalog.preferences = current.preferences.clone();
    catalog.owner_hidden = current.owner_hidden.clone();
    catalog.resolve_defaults();
    *current = catalog;
    Ok(())
}

fn find_client_message(value: &serde_json::Value, client_message_id: &str) -> Option<String> {
    let entries = value.get("data")?.as_array()?;
    entries.iter().find_map(|entry| {
        let item = entry.get("item")?;
        if item.get("type").and_then(serde_json::Value::as_str) != Some("userMessage")
            || item.get("clientId").and_then(serde_json::Value::as_str) != Some(client_message_id)
        {
            return None;
        }
        entry
            .get("turnId")
            .and_then(serde_json::Value::as_str)
            .or_else(|| item.get("turnId").and_then(serde_json::Value::as_str))
            .map(str::to_owned)
    })
}

/// The largest message a client may send on `/api/v1/events`.
const MAX_EVENT_CLIENT_MESSAGE_BYTES: usize = 64 * 1024;
/// The largest body the unauthenticated pairing routes read.
const MAX_PUBLIC_PAIRING_BODY_BYTES: usize = 16 * 1024;

async fn events_socket(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let local_capability = headers
        .get("x-wonder-loopback-capability")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == state.loopback_capability);
    let session = cookie_value(&headers, "__Host-wonder_session");
    let compact = history::wants_compact_view(&headers);
    // Clients send only a subscription and acknowledgements.
    ws.max_message_size(MAX_EVENT_CLIENT_MESSAGE_BYTES)
        .on_upgrade(move |socket| stream_events(socket, state, session, local_capability, compact))
}

async fn event_challenge(
    State(state): State<AppState>,
    device: Option<Extension<AuthenticatedDevice>>,
    origin: Option<Extension<MatchedOrigin>>,
) -> Response {
    // The owner signs in with the loopback capability, never a challenge.
    let (
        Some(Extension(AuthenticatedDevice { device_id, .. })),
        Some(Extension(MatchedOrigin(origin))),
    ) = (device, origin)
    else {
        return (
            StatusCode::FORBIDDEN,
            "Only a paired device signs in with a challenge.",
        )
            .into_response();
    };
    match state.pairing.lock().await.issue_fresh_challenge(
        &device_id,
        &origin,
        &state.host_installation_id,
        now_ms(),
    ) {
        Ok(challenge) => Json(challenge).into_response(),
        Err(_) => (StatusCode::UNAUTHORIZED, "paired device required").into_response(),
    }
}

pub fn capability_from_environment() -> Result<String, String> {
    std::env::var("WONDER_LOOPBACK_CAPABILITY")
        .map_err(|_| "WONDER_LOOPBACK_CAPABILITY is required".into())
}

/// Protocol challenges stay private in memory for signature verification.
/// The host-facing enrollment state keeps only a comparison MAC. No API
/// exposes the expected text, even for an interactive terminal.
#[derive(Default)]
pub struct HostPairingState {
    protocol: PairingState,
    verifications: std::collections::HashMap<String, EnrollmentVerification>,
    /// Recent host proofs per route origin (see [`HostPairingState::allow_host_proof`]).
    host_proofs: std::collections::HashMap<String, Vec<u64>>,
}

const HOST_PROOF_WINDOW_MS: u64 = 60 * 1000;
const HOST_PROOF_LIMIT: usize = 60;

struct EnrollmentVerification {
    expected: ring::hmac::Tag,
    attempts: u32,
}

impl HostPairingState {
    /// Bounds unauthenticated host proofs, counted per route origin so a flood
    /// on one network cannot stop phones checking the host on another.
    fn allow_host_proof(&mut self, origin: &str, now_ms: u64) -> bool {
        self.host_proofs.retain(|_, proofs| {
            proofs.retain(|at| now_ms.saturating_sub(*at) < HOST_PROOF_WINDOW_MS);
            !proofs.is_empty()
        });
        let proofs = self.host_proofs.entry(origin.to_owned()).or_default();
        if proofs.len() >= HOST_PROOF_LIMIT {
            return false;
        }
        proofs.push(now_ms);
        true
    }
}

impl From<PairingState> for HostPairingState {
    fn from(protocol: PairingState) -> Self {
        Self {
            protocol,
            ..Self::default()
        }
    }
}

impl std::ops::Deref for HostPairingState {
    type Target = PairingState;
    fn deref(&self) -> &Self::Target {
        &self.protocol
    }
}

impl std::ops::DerefMut for HostPairingState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.protocol
    }
}

fn enrollment_code(pending: &wonder_api::pairing_protocol::PendingEnrollment) -> String {
    use wonder_api::pairing::{canonical_challenge_transcript, ChallengeTranscript};
    let c = &pending.challenge;
    let transcript = canonical_challenge_transcript(&ChallengeTranscript {
        device_id: &c.device_id,
        challenge_id: &c.challenge_id,
        nonce: &c.nonce,
        origin: &c.origin,
        host_installation_id: &c.host_installation_id,
        issued_at_ms: c.issued_at_ms,
        expires_at_ms: c.expires_at_ms,
    });
    hex::encode_upper(Sha256::digest(transcript))[..12].into()
}

/// The `error` of a confirmation refused only because the typed text differs
/// from the phone's; the phone stays pending, so the person may type it again.
pub const PAIRING_CODE_MISMATCH: &str = "verification_mismatch";

/// The phone shows 12 uppercase hex digits; people type them in any case,
/// grouped with spaces or hyphens. Anything else can never match.
fn normalized_enrollment_code(typed: &str) -> Option<String> {
    let code: String = typed
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == 12 && code.bytes().all(|b| b.is_ascii_hexdigit())).then_some(code)
}

fn enrollment_mac_key() -> ring::hmac::Key {
    ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"Cupola pairing verification")
}

impl HostPairingState {
    fn verification(
        &mut self,
        pending: &wonder_api::pairing_protocol::PendingEnrollment,
    ) -> &mut EnrollmentVerification {
        self.verifications
            .entry(pending.device_id.clone())
            .or_insert_with(|| {
                let code = enrollment_code(pending);
                EnrollmentVerification {
                    expected: ring::hmac::sign(&enrollment_mac_key(), code.as_bytes()),
                    attempts: 0,
                }
            })
    }

    fn prune_verifications(&mut self, now: u64) {
        let pending = self.pending_enrollments(now);
        self.verifications
            .retain(|id, _| pending.iter().any(|p| &p.device_id == id));
    }
}

async fn list_pending_enrollments(
    State(state): State<AppState>,
    Extension(_local): Extension<LocalOwnerAuthority>,
) -> Response {
    let mut pairing = state.pairing.lock().await;
    pairing.prune_verifications(now_ms());
    let mut result = Vec::new();
    for pending in pairing.pending_enrollments(now_ms()) {
        // Never return the challenge, nonce, transcript digest or comparison
        // MAC: the phone's displayed text can be reconstructed from them.
        result.push(serde_json::json!({
            "deviceId": pending.device_id, "label": pending.label,
            "offerId": pending.challenge.offer_id,
            "hostInstallationId": pending.challenge.host_installation_id,
            "expiresAtMs": pending.challenge.expires_at_ms,
        }));
    }
    Json(result).into_response()
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EnrollmentConfirmation {
    code: Option<String>,
    #[cfg(feature = "dev-pairing")]
    #[serde(default)]
    dev_approve: bool,
}

fn pairing_request<T: serde::de::DeserializeOwned + Default>(body: &[u8]) -> Result<T, Response> {
    if body.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice::<Option<T>>(body)
        .map(Option::unwrap_or_default)
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())
}

async fn confirm_enrollment(
    State(state): State<AppState>,
    Extension(_local): Extension<LocalOwnerAuthority>,
    Path(device_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    // Keep confirmation, persistence and session issuance serialized.
    let mut pairing = state.pairing.lock().await;
    let confirmed_at_ms = now_ms();
    let pending = match pairing.pending_enrollment(&device_id, confirmed_at_ms) {
        Ok(pending) => pending,
        Err(error) => return pairing_error_response(error),
    };
    let request: EnrollmentConfirmation = match pairing_request(&body) {
        Ok(r) => r,
        Err(error) => return error,
    };
    #[cfg(feature = "dev-pairing")]
    let dev_approve = request.dev_approve;
    #[cfg(not(feature = "dev-pairing"))]
    let dev_approve = false;
    if !dev_approve {
        let Some(code) = request.code.filter(|code| !code.is_empty()) else {
            return (StatusCode::BAD_REQUEST, "approval requires --code <TEXT>: ask the person to read the verification text from their phone").into_response();
        };
        let verification = pairing.verification(&pending);
        if verification.attempts >= 20 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "too many verification attempts; deny this phone and pair again",
            )
                .into_response();
        }
        let matches = normalized_enrollment_code(&code).is_some_and(|code| {
            ring::hmac::verify(
                &enrollment_mac_key(),
                code.as_bytes(),
                verification.expected.as_ref(),
            )
            .is_ok()
        });
        if !matches {
            verification.attempts += 1;
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": PAIRING_CODE_MISMATCH,
                    "detail": "verification text does not match; ask the person to read the text from their phone again, or use --deny",
                })),
            )
                .into_response();
        }
    }
    let key = match serde_json::to_string(&pending.public_key) {
        Ok(key) => key,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if state
        .store
        .upsert_owner_device_with_expiration(
            &device_id,
            &pending.label,
            &key,
            pending.session_expires_at_ms,
            &now_ms().to_string(),
        )
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save this phone. Try confirming again.",
        )
            .into_response();
    }
    // The same phone pairing again replaces its earlier row, not adds a second.
    match state
        .store
        .replace_owner_devices_with_key(&device_id, &key, &now_ms().to_string())
        .await
    {
        Ok(replaced) => {
            for old in replaced {
                pairing.revoke_device(&old);
                let _ = state.revocations.send(old);
            }
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save this phone. Try confirming again.",
            )
                .into_response()
        }
    }
    // The model only labels the device list; a failed write never blocks pairing.
    if let Some(model) = &pending.model {
        let _ = state.store.set_owner_device_model(&device_id, model).await;
    }
    // The owner made the decision before expiry, while holding the pairing lock.
    match pairing.confirm_enrollment(&device_id, confirmed_at_ms) {
        Ok(()) => {
            pairing.verifications.remove(&device_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => pairing_error_response(error),
    }
}

async fn reject_enrollment(
    State(state): State<AppState>,
    Extension(_local): Extension<LocalOwnerAuthority>,
    Path(device_id): Path<String>,
) -> Response {
    let mut pairing = state.pairing.lock().await;
    if let Err(error) = pairing.pending_enrollment(&device_id, now_ms()) {
        return pairing_error_response(error);
    }
    pairing.revoke_device(&device_id);
    pairing.verifications.remove(&device_id);
    pairing.prune_verifications(now_ms());
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct HostProofQuery {
    nonce: String,
}

/// `GET /api/v1/host/proof?nonce=<32 hex digits>`: the host key's signature
/// over the origin this request matched and the phone's nonce. A phone checks
/// it against the pin from its pairing link before it trusts a route, so the
/// route needs no session; the route's own origin check still applies.
async fn host_proof(
    State(state): State<AppState>,
    Extension(MatchedOrigin(origin)): Extension<MatchedOrigin>,
    Query(query): Query<HostProofQuery>,
) -> Response {
    if !is_host_proof_nonce(&query.nonce) {
        return (StatusCode::BAD_REQUEST, "nonce must be 32 hex digits").into_response();
    }
    if !state
        .pairing
        .lock()
        .await
        .allow_host_proof(&origin, now_ms())
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            "too many host proof requests",
        )
            .into_response();
    }
    let proof = match lan::host_key(&state.data_dir).await {
        Ok(host_key) => host_key.prove(&origin, &query.nonce),
        Err(error) => {
            let _ = state.logger.record(
                "error",
                "host_key_unavailable",
                serde_json::json!({ "error": error.to_string() }),
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "This computer's key could not be read",
            )
                .into_response();
        }
    };
    match proof {
        Ok(proof) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(proof),
        )
            .into_response(),
        // The nonce was checked above, so only a misconfigured origin gets here.
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The sealed RPC handshake signs the same `wonder-host-proof-v1` transcript
/// with a 43-character base64url nonce derived from its key exchange. This
/// route signs a nonce anyone chooses, so it accepts only nonces that can
/// never be one of those; otherwise a relay could have it sign a sealed
/// welcome for keys of its own.
fn is_host_proof_nonce(nonce: &str) -> bool {
    nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn create_pairing_offer(
    State(state): State<AppState>,
    Extension(_local): Extension<LocalOwnerAuthority>,
    body: axum::body::Bytes,
) -> Response {
    // No body (or `null`) for the main route; `{"route": "home"}` for a link on
    // this computer's home network address.
    let request = if body.is_empty() {
        rpc::PairingOfferParams::default()
    } else {
        match serde_json::from_slice::<Option<rpc::PairingOfferParams>>(&body) {
            Ok(request) => request.unwrap_or_default(),
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        }
    };
    // The phone pins this key at pairing and checks it on every route later.
    let host_key = match lan::host_key(&state.data_dir).await {
        Ok(host_key) => host_key,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("This computer's key could not be read: {error}"),
            )
                .into_response()
        }
    };
    let public_origin = if request.route == Some(rpc::PairingOfferRoute::Home) {
        // The first home address that answers; the phone must be on that network.
        let Some(origin) = state.home_routes.origins().into_iter().next() else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Home network connections are not running on this computer. Turn them on with \
                 `cupola lan on` and restart the host.",
            )
                .into_response();
        };
        Some(origin)
    } else {
        current_public_origin(&state).await
    };
    let mut pairing = state.pairing.lock().await;
    pairing.prune_verifications(now_ms());
    pairing_offer_response(
        public_origin,
        &state.host_installation_id,
        &host_key.pin(),
        &mut pairing,
    )
}

fn pairing_offer_response(
    public_origin: Option<String>,
    host_installation_id: &str,
    host_key_pin: &str,
    pairing: &mut PairingState,
) -> Response {
    let Some(public_origin) = public_origin else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "This computer's connection address isn't ready yet. Check `cupola status`.",
        )
            .into_response();
    };
    let offer = pairing.create_offer_with_host_key(
        &public_origin,
        host_installation_id,
        Some(host_key_pin),
        now_ms(),
    );
    (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(offer),
    )
        .into_response()
}

async fn cancel_pairing_offer(
    State(state): State<AppState>,
    Extension(_local): Extension<LocalOwnerAuthority>,
    Path(offer_id): Path<String>,
) -> impl IntoResponse {
    let mut pairing = state.pairing.lock().await;
    let removed = pairing.cancel_offer(&offer_id);
    pairing.prune_verifications(now_ms());
    if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimRequest {
    offer_id: String,
    secret: String,
    public_key: DevicePublicKeyJwk,
    label: String,
    /// Hardware model ("iPhone17,2"); older phones omit it.
    #[serde(default)]
    model: Option<String>,
    #[serde(default = "default_session_expiration")]
    session_expiration: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingCodeRequest {
    human_code: String,
    public_key: DevicePublicKeyJwk,
    label: String,
    /// Hardware model ("iPhone17,2"); older phones omit it.
    #[serde(default)]
    model: Option<String>,
    #[serde(default = "default_session_expiration")]
    session_expiration: String,
}

fn default_session_expiration() -> String {
    "1d".into()
}

async fn claim_pairing_offer(
    State(state): State<AppState>,
    Json(request): Json<ClaimRequest>,
) -> Response {
    if !valid_device_label(&request.label) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let session_expires_at_ms =
        match session_expiration_deadline(&request.session_expiration, now_ms()) {
            Ok(deadline) => deadline,
            Err(error) => return pairing_error_response(error),
        };
    let mut pairing = state.pairing.lock().await;
    let result = pairing.claim_offer_with_session_expiration(
        &request.offer_id,
        &request.secret,
        request.public_key,
        &request.label,
        session_expires_at_ms,
        now_ms(),
    );
    if let (Ok(claimed), Some(model)) = (&result, device_model(request.model.as_deref())) {
        pairing.set_pending_model(&claimed.device_id, model);
    }
    drop(pairing);
    match result {
        Ok(claimed) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(claimed),
        )
            .into_response(),
        Err(error) => pairing_error_response(error),
    }
}

async fn claim_pairing_code(
    State(state): State<AppState>,
    Json(request): Json<PairingCodeRequest>,
) -> Response {
    if !valid_device_label(&request.label) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !state
        .pairing
        .lock()
        .await
        .allow_human_code_attempt(now_ms())
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            "too many pairing code attempts",
        )
            .into_response();
    }
    let session_expires_at_ms =
        match session_expiration_deadline(&request.session_expiration, now_ms()) {
            Ok(deadline) => deadline,
            Err(error) => return pairing_error_response(error),
        };
    let mut pairing = state.pairing.lock().await;
    let result = pairing.claim_offer_by_human_code(
        &request.human_code,
        request.public_key,
        &request.label,
        session_expires_at_ms,
        now_ms(),
    );
    if let (Ok(claimed), Some(model)) = (&result, device_model(request.model.as_deref())) {
        pairing.set_pending_model(&claimed.device_id, model);
    }
    drop(pairing);
    match result {
        Ok(claimed) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(claimed),
        )
            .into_response(),
        Err(error) => pairing_error_response(error),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionRefreshChallengeRequest {
    device_id: String,
}

async fn create_session_refresh_challenge(
    State(state): State<AppState>,
    Extension(MatchedOrigin(origin)): Extension<MatchedOrigin>,
    Json(request): Json<SessionRefreshChallengeRequest>,
) -> Response {
    match state.pairing.lock().await.issue_fresh_challenge(
        &request.device_id,
        &origin,
        &state.host_installation_id,
        now_ms(),
    ) {
        Ok(challenge) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(challenge),
        )
            .into_response(),
        Err(error) => {
            if let Some(reason) = pairing_refusal_reason(&error) {
                log_device_auth_refusal(
                    "session refresh",
                    &format!("device {}", id_prefix(&request.device_id)),
                    reason,
                );
            }
            pairing_error_response(error)
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionRequest {
    challenge_id: String,
    signature: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse {
    session_token: String,
    device_id: String,
    csrf_token: String,
    host_installation_id: String,
    expires_at_ms: Option<u64>,
}

async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<SessionRequest>,
) -> Response {
    let mut pairing = state.pairing.lock().await;
    let result = pairing.verify_challenge(&request.challenge_id, &request.signature, now_ms());
    if let Some(reason) = result.as_ref().err().and_then(pairing_refusal_reason) {
        log_device_auth_refusal(
            "session sign-in",
            &format!("challenge {}", id_prefix(&request.challenge_id)),
            reason,
        );
    }
    match result {
        Ok(session) => {
            if state
                .store
                .insert_session(
                    &session_token_hash(&session.session_token),
                    &session.device_id,
                    &csrf_token_hash(&session.csrf_token),
                    session.expires_at_ms,
                    &now_ms().to_string(),
                )
                .await
                .is_err()
            {
                pairing.rollback_session(&request.challenge_id, &session);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "session persistence failed",
                )
                    .into_response();
            }
            let cookie = if session.expires_at_ms == NEVER_EXPIRES_AT_MS {
                format!(
                    "__Host-wonder_session={}; Path=/; Max-Age=2147483647; Secure; HttpOnly; SameSite=Strict",
                    session.session_token
                )
            } else {
                let max_age = session
                    .expires_at_ms
                    .saturating_sub(now_ms())
                    .saturating_div(1000);
                format!(
                    "__Host-wonder_session={}; Path=/; Max-Age={}; Secure; HttpOnly; SameSite=Strict",
                    session.session_token, max_age
                )
            };
            (
                StatusCode::OK,
                [
                    (header::SET_COOKIE, cookie),
                    (header::CACHE_CONTROL, "no-store".into()),
                ],
                Json(SessionResponse {
                    session_token: session.session_token,
                    device_id: session.device_id,
                    csrf_token: session.csrf_token,
                    host_installation_id: session.host_installation_id,
                    expires_at_ms: (session.expires_at_ms != NEVER_EXPIRES_AT_MS)
                        .then_some(session.expires_at_ms),
                }),
            )
                .into_response()
        }
        Err(error) => pairing_error_response(error),
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSummary {
    id: String,
    created_at: String,
    label: String,
    role: &'static str,
    last_seen_at: Option<String>,
    revoked_at: Option<String>,
    session_expires_at_ms: Option<u64>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameDeviceRequest {
    label: String,
    #[serde(flatten)]
    action: SignedActionFields,
}

/// A reported hardware model worth keeping: short and plain ("iPhone17,2").
/// Anything else is dropped rather than refusing the pairing.
fn device_model(model: Option<&str>) -> Option<String> {
    let model = model?.trim();
    (!model.is_empty()
        && model.len() <= 64
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ',' | '.' | '-' | '_' | ' ')))
    .then(|| model.to_owned())
}

fn valid_device_label(label: &str) -> bool {
    let trimmed = label.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= 160 && !trimmed.chars().any(char::is_control)
}

async fn list_devices(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    match state.store.list_owner_devices().await {
        Ok(devices) => Json(
            devices
                .into_iter()
                .map(|device| DeviceSummary {
                    created_at: device.created_at,
                    id: device.id,
                    label: device.label,
                    role: "owner",
                    last_seen_at: device.last_seen_at,
                    revoked_at: device.revoked_at,
                    session_expires_at_ms: device.session_expires_at_ms,
                    model: device.model,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "device listing failed").into_response(),
    }
}

async fn rename_device(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(device_id): Path<String>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    local_authority: Option<Extension<LocalOwnerAuthority>>,
    Json(request): Json<RenameDeviceRequest>,
) -> Response {
    if authenticated_device.is_none() && local_authority.is_none() {
        return (StatusCode::UNAUTHORIZED, "paired device required").into_response();
    }
    let label = request.label.trim();
    if !valid_device_label(label) {
        return (
            StatusCode::BAD_REQUEST,
            "device label must be 1-160 characters and contain no control characters",
        )
            .into_response();
    }
    if let Some(Extension(device)) = authenticated_device {
        let target = format!("/api/v1/devices/{device_id}");
        let body_sha256 = action_body_sha256(&serde_json::json!([label]));
        if let Err(message) = verify_signed_action(
            &state,
            &device,
            "device.rename",
            &target,
            &body_sha256,
            &request.action,
            "active",
        )
        .await
        {
            return (StatusCode::UNAUTHORIZED, message).into_response();
        }
    }
    match state.store.rename_owner_device(&device_id, label).await {
        Ok(true) => match state.store.list_owner_devices().await {
            Ok(devices) => devices
                .into_iter()
                .find(|device| device.id == device_id)
                .map(|device| {
                    Json(DeviceSummary {
                        created_at: device.created_at,
                        id: device.id,
                        label: device.label,
                        role: "owner",
                        last_seen_at: device.last_seen_at,
                        revoked_at: device.revoked_at,
                        session_expires_at_ms: device.session_expires_at_ms,
                        model: device.model,
                    })
                    .into_response()
                })
                .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response()),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "device listing failed").into_response(),
        },
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "device rename failed").into_response(),
    }
}

async fn forget_device(
    State(state): State<AppState>,
    local_authority: Option<Extension<LocalOwnerAuthority>>,
    Path(device_id): Path<String>,
) -> Response {
    if local_authority.is_none() {
        return (StatusCode::FORBIDDEN, "local owner required").into_response();
    }
    match state.store.forget_revoked_device(&device_id).await {
        Ok(true) => {
            state.pairing.lock().await.forget_device(&device_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "revoked device not found").into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "device history could not be removed",
        )
            .into_response(),
    }
}

async fn revoke_device(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(device_id): Path<String>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    local_authority: Option<Extension<LocalOwnerAuthority>>,
    Json(request): Json<Option<SignedActionFields>>,
) -> Response {
    if authenticated_device.is_none() && local_authority.is_none() {
        return (StatusCode::UNAUTHORIZED, "paired device required").into_response();
    }
    if let Some(Extension(device)) = authenticated_device {
        let Some(request) = request.as_ref() else {
            return (
                StatusCode::UNAUTHORIZED,
                "signed action fields are required",
            )
                .into_response();
        };
        let target = format!("/api/v1/devices/{device_id}/revoke");
        if let Err(message) = verify_signed_action(
            &state,
            &device,
            "device.revoke",
            &target,
            &action_body_sha256(&serde_json::json!([])),
            request,
            "active",
        )
        .await
        {
            return (StatusCode::UNAUTHORIZED, message).into_response();
        }
        // A paired device may disconnect only itself; disconnecting another
        // device is the owner's call on this computer.
        if local_authority.is_none() && device.device_id != device_id {
            log_device_auth_refusal(
                "device revoke",
                &format!("device {}", id_prefix(&device.device_id)),
                &format!("tried to revoke another device {}", id_prefix(&device_id)),
            );
            return (
                StatusCode::FORBIDDEN,
                "a paired device can disconnect only itself; disconnect other devices on your computer",
            )
                .into_response();
        }
    }
    let mut pairing = state.pairing.lock().await;
    let revoked = match state
        .store
        .revoke_owner_device(&device_id, &now_ms().to_string())
        .await
    {
        Ok(revoked) => revoked,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "device revocation failed",
            )
                .into_response()
        }
    };
    if revoked {
        pairing.revoke_device(&device_id);
        let _ = state.revocations.send(device_id);
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn relay_status(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    let data_dir = state.data_dir.clone();
    match tokio::task::spawn_blocking(move || relay::view(&data_dir)).await {
        Ok(view) => ([(header::CACHE_CONTROL, "no-store")], Json(view)).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RelaySettingsRequest {
    allowed: bool,
}

/// The owner's choice, on this computer only: a phone cannot allow the relay.
async fn relay_settings(
    State(state): State<AppState>,
    local_authority: Option<Extension<LocalOwnerAuthority>>,
    Json(request): Json<RelaySettingsRequest>,
) -> Response {
    if local_authority.is_none() {
        return (
            StatusCode::FORBIDDEN,
            "turn the relay on or off on the computer",
        )
            .into_response();
    }
    match state.relay.set_allowed(request.allowed).await {
        Ok(view) => Json(view).into_response(),
        Err(message) => (StatusCode::SERVICE_UNAVAILABLE, message).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayEnableRequest {
    grant: String,
    label: String,
    #[serde(flatten)]
    action: SignedActionFields,
}

/// A signed-in phone hands over the Worker's one-time grant for this
/// computer's host key, signed with its device key (`relay.enable`).
async fn relay_enable(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    local_authority: Option<Extension<LocalOwnerAuthority>>,
    Json(request): Json<RelayEnableRequest>,
) -> Response {
    if authenticated_device.is_none() && local_authority.is_none() {
        return (StatusCode::UNAUTHORIZED, "paired device required").into_response();
    }
    if let Some(Extension(device)) = authenticated_device {
        let body = serde_json::json!({"grant": request.grant, "label": request.label});
        if let Err(message) = verify_signed_action(
            &state,
            &device,
            "relay.enable",
            "/api/v1/relay/enable",
            &action_body_sha256(&body),
            &request.action,
            "allowed",
        )
        .await
        {
            return (StatusCode::UNAUTHORIZED, message).into_response();
        }
    }
    match state.relay.enable(&request.grant, &request.label).await {
        Ok(view) => Json(view).into_response(),
        Err(relay::EnableError::NotAllowed) => {
            (StatusCode::FORBIDDEN, relay::NOT_ALLOWED_MESSAGE).into_response()
        }
        Err(relay::EnableError::Invalid) => (
            StatusCode::BAD_REQUEST,
            "not a relay grant for this computer",
        )
            .into_response(),
    }
}

async fn reset_workspace(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    local_authority: Option<Extension<LocalOwnerAuthority>>,
    Json(request): Json<ResetWorkspaceRequest>,
) -> Response {
    if authenticated_device.is_none() && local_authority.is_none() {
        return (StatusCode::UNAUTHORIZED, "paired device required").into_response();
    }

    let keep_device_id = if let Some(Extension(device)) = authenticated_device {
        if let Some(requested) = request.keep_device_id.as_deref() {
            if requested != device.device_id {
                return (
                    StatusCode::BAD_REQUEST,
                    "a paired session can preserve only its own device",
                )
                    .into_response();
            }
        }
        let target = "/api/v1/workspace/reset";
        if let Err(message) = verify_signed_action(
            &state,
            &device,
            "workspace.reset",
            target,
            &action_body_sha256(&serde_json::json!([device.device_id.clone()])),
            &request.action,
            "active",
        )
        .await
        {
            return (StatusCode::UNAUTHORIZED, message).into_response();
        }
        device.device_id
    } else {
        let Some(keep_device_id) = request.keep_device_id.as_deref() else {
            return (
                StatusCode::BAD_REQUEST,
                "keepDeviceId is required for local reset",
            )
                .into_response();
        };
        keep_device_id.to_owned()
    };

    // The chats' pictures and files go with their records.
    let store = state.store.clone();
    let keep = keep_device_id.clone();
    let reset = media_refs::reset_media(state.data_dir.clone(), async move {
        store.reset_workspace(&keep, &now_ms().to_string()).await
    })
    .await;
    let reset = match reset {
        Ok(reset) => reset,
        Err(sqlx::Error::RowNotFound) => {
            return (StatusCode::NOT_FOUND, "preserved device was not found").into_response()
        }
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "workspace reset failed").into_response()
        }
    };

    for device_id in &reset.revoked_device_ids {
        state.pairing.lock().await.revoke_device(device_id);
        let _ = state.revocations.send(device_id.clone());
    }

    Json(ResetWorkspaceSummary {
        preserved_device_id: keep_device_id,
        revoked_device_count: reset.revoked_device_ids.len(),
    })
    .into_response()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalSummary {
    approval_id: String,
    conversation_id: String,
    method: String,
    params: serde_json::Value,
    action_nonce: String,
    resolution_idempotency_key: Option<String>,
}

async fn list_approvals(State(state): State<AppState>) -> Response {
    let approvals = match state.store.list_pending_approvals().await {
        Ok(approvals) => approvals,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "approval listing failed").into_response()
        }
    };
    let mut summaries = Vec::with_capacity(approvals.len());
    for approval in approvals {
        let conversation_id = match state
            .store
            .approval_push_conversation(&approval.thread_id, &approval.turn_id)
            .await
        {
            Ok(Some(conversation_id)) => conversation_id,
            Ok(None) => continue,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "approval routing could not be loaded",
                )
                    .into_response()
            }
        };
        summaries.push(ApprovalSummary {
            approval_id: approval.approval_id,
            conversation_id,
            method: approval.method.clone(),
            params: redact_approval_params(
                &approval.method,
                &serde_json::from_str(&approval.params_json)
                    .unwrap_or_else(|_| serde_json::json!({})),
            ),
            action_nonce: approval.action_nonce,
            resolution_idempotency_key: approval.resolution_idempotency_key,
        });
    }
    Json(summaries).into_response()
}

/// A proposed diff shown with an edit approval. One over either cap is left
/// out and marked `truncated`, never cut short, so the owner never approves
/// part of what they saw.
const PROPOSED_DIFF_BYTES: usize = 64 * 1024;
const PROPOSED_DIFFS_BYTES: usize = 200 * 1024;

/// Each proposed file change as `{path, kind, diff | truncated}`. Codex names
/// the kind as `{"type": "update", ...}`; the phone reads the word. A change
/// without a path is not shown.
fn proposed_changes(changes: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut total = 0;
    changes
        .iter()
        .filter_map(|change| {
            let path = change.get("path").filter(|path| path.is_string())?;
            let mut safe = serde_json::Map::from_iter([("path".into(), path.clone())]);
            if let Some(kind) = change.get("kind").and_then(|kind| {
                kind.as_str()
                    .or_else(|| kind.get("type").and_then(serde_json::Value::as_str))
            }) {
                safe.insert("kind".into(), kind.into());
            }
            match change.get("diff").and_then(serde_json::Value::as_str) {
                Some(diff)
                    if !diff.is_empty()
                        && diff.len() <= PROPOSED_DIFF_BYTES
                        && total + diff.len() <= PROPOSED_DIFFS_BYTES =>
                {
                    total += diff.len();
                    safe.insert("diff".into(), diff.into());
                }
                Some(diff) if !diff.is_empty() => {
                    safe.insert("truncated".into(), true.into());
                }
                _ if change["truncated"] == true => {
                    safe.insert("truncated".into(), true.into());
                }
                _ => {}
            }
            Some(serde_json::Value::Object(safe))
        })
        .collect()
}

/// Codex sends a file change item (and its patch updates) before asking to
/// apply it, and the request names only the item. The latest proposal per
/// item is held here, bounded, for that request. A completed item is the
/// applied change, never a proposal.
mod proposed_file_changes {
    use std::collections::VecDeque;
    use std::sync::{LazyLock, Mutex};

    const MAX_ITEMS: usize = 32;

    type Proposal = (String, String, Vec<serde_json::Value>);
    static RECENT: LazyLock<Mutex<VecDeque<Proposal>>> =
        LazyLock::new(|| Mutex::new(VecDeque::new()));

    pub(crate) fn observe(thread_id: &str, method: &str, params: &serde_json::Value) {
        let (item_id, changes) = match method {
            "item/started" | "item/fileChange/started" => {
                let item = &params["item"];
                if item["type"] != "fileChange" {
                    return;
                }
                (item["id"].as_str(), item["changes"].as_array())
            }
            "item/fileChange/patchUpdated" => {
                (params["itemId"].as_str(), params["changes"].as_array())
            }
            _ => return,
        };
        let (Some(item_id), Some(changes)) = (item_id, changes) else {
            return;
        };
        let changes = super::proposed_changes(changes);
        let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
        recent.retain(|(thread, item, _)| !(thread == thread_id && item == item_id));
        recent.push_back((thread_id.to_owned(), item_id.to_owned(), changes));
        while recent.len() > MAX_ITEMS {
            recent.pop_front();
        }
    }

    pub(crate) fn get(thread_id: &str, item_id: &str) -> Option<Vec<serde_json::Value>> {
        let recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
        recent
            .iter()
            .find(|(thread, item, _)| thread == thread_id && item == item_id)
            .map(|(_, _, changes)| changes.clone())
    }
}

fn redact_approval_params(method: &str, params: &serde_json::Value) -> serde_json::Value {
    let mut summary = serde_json::Map::new();
    if method == thread_tools::secret::METHOD {
        if let Some(request) = thread_tools::secret::summary(params) {
            summary.insert("secretRequest".into(), request);
        }
    }
    if method == "item/tool/requestUserInput" {
        if let Some(question) = wonder_store::notification_question(params) {
            summary.insert(
                "notificationQuestion".into(),
                serde_json::json!({"id": question.id, "body": question.body}),
            );
        }
    }
    for key in [
        "command",
        "cwd",
        "filePath",
        "grantRoot",
        "url",
        "reason",
        "itemId",
        "threadId",
        "turnId",
        "serverName",
        "callId",
        "tool",
        "namespace",
        "kind",
        "isBlocking",
        "supportsDenyMessage",
    ] {
        if let Some(value) = params.get(key).filter(|value| value.is_string()) {
            summary.insert(key.into(), value.clone());
        } else if let Some(value) = params.get(key).filter(|value| value.is_boolean()) {
            summary.insert(key.into(), value.clone());
        }
    }
    if let Some(deadline) = params
        .get("_wonderQuestionDeadlineMs")
        .and_then(serde_json::Value::as_u64)
    {
        summary.insert("expiresAtMs".into(), deadline.into());
    }
    if let Some(decisions) = params
        .get("availableDecisions")
        .and_then(|value| value.as_array())
    {
        summary.insert("availableDecisions".into(), decisions.clone().into());
    }
    if let Some(changes) = params.get("changes").and_then(|value| value.as_array()) {
        summary.insert("changes".into(), proposed_changes(changes).into());
    }
    if let Some(questions) = params.get("questions").and_then(|value| value.as_array()) {
        let safe_questions = questions
            .iter()
            .filter_map(|question| {
                let object = question.as_object()?;
                let mut safe = serde_json::Map::new();
                for key in ["id", "header", "question", "isSecret", "isOther"] {
                    if let Some(value) = object
                        .get(key)
                        .filter(|value| value.is_string() || value.is_boolean())
                    {
                        safe.insert(key.into(), value.clone());
                    }
                }
                if let Some(value) = object.get("multiSelect").filter(|value| value.is_boolean()) {
                    safe.insert("multiSelect".into(), value.clone());
                }
                if let Some(options) = object.get("options").and_then(|value| value.as_array()) {
                    let safe_options = options
                        .iter()
                        .filter_map(|option| {
                            let object = option.as_object()?;
                            let mut safe = serde_json::Map::new();
                            for key in ["label", "description"] {
                                if let Some(value) =
                                    object.get(key).filter(|value| value.is_string())
                                {
                                    safe.insert(key.into(), value.clone());
                                }
                            }
                            (!safe.is_empty()).then_some(serde_json::Value::Object(safe))
                        })
                        .collect::<Vec<_>>();
                    safe.insert("options".into(), safe_options.into());
                }
                (!safe.is_empty()).then_some(serde_json::Value::Object(safe))
            })
            .collect::<Vec<_>>();
        summary.insert("questions".into(), safe_questions.into());
    }
    if let Some(value) = params.get("permissions") {
        if let Some(value) = redact_permission_profile(value) {
            summary.insert("permissions".into(), value);
        }
    }
    if let Some(value) = params
        .get("additionalPermissions")
        .and_then(redact_permission_profile)
    {
        summary.insert("additionalPermissions".into(), value);
    }
    if let Some(context) = params
        .get("networkApprovalContext")
        .and_then(serde_json::Value::as_object)
    {
        let safe: serde_json::Map<String, serde_json::Value> = ["host", "protocol"]
            .into_iter()
            .filter_map(|key| {
                context
                    .get(key)
                    .filter(|value| value.is_string())
                    .map(|value| (key.into(), value.clone()))
            })
            .collect();
        summary.insert("networkApprovalContext".into(), safe.into());
    }
    if let Some(value) = params.get("scope").filter(|value| value.is_string()) {
        summary.insert("scope".into(), value.clone());
    }
    if let Some(value) = params
        .get("requestedSchema")
        .and_then(redact_elicitation_schema)
    {
        summary.insert("requestedSchema".into(), value);
    }
    if let Some(message) = params.get("message").filter(|value| value.is_string()) {
        summary.insert("message".into(), message.clone());
    }
    if let Some(mode) = params.get("mode").filter(|value| value.is_string()) {
        summary.insert("mode".into(), mode.clone());
    }
    if method == "mcpServer/elicitation/request"
        && params["serverName"] == "cua_repl"
        && params["_meta"]["codex_approval_kind"] == "mcp_tool_call"
    {
        // Preserve consent details without publishing provider correlation IDs,
        // raw tool arguments, or grant-persistence options the phone cannot set.
        let meta = &params["_meta"];
        let mut context = serde_json::Map::new();
        context.insert("isComputerUse".into(), true.into());
        for key in ["subtitle", "riskLevel"] {
            if let Some(value) = meta[key].as_str().filter(|v| v.len() <= 4096) {
                context.insert(key.into(), value.into());
            }
        }
        if let Some(fields) = meta["tool_params_display"].as_array() {
            let details: Vec<String> = fields
                .iter()
                .take(32)
                .filter_map(|field| {
                    let label = field["display_name"].as_str()?;
                    let value = field["value"].as_str()?;
                    (label.len() <= 256 && value.len() <= 4096).then(|| format!("{label}: {value}"))
                })
                .collect();
            context.insert("details".into(), serde_json::json!(details));
        }
        summary.insert("elicitationContext".into(), context.into());
    }
    summary.insert(
        "requestType".into(),
        serde_json::Value::String(method.into()),
    );
    serde_json::Value::Object(summary)
}

fn redact_permission_profile(value: &serde_json::Value) -> Option<serde_json::Value> {
    validate_permission_profile(value).ok()?;
    let object = value.as_object()?;
    let mut safe = serde_json::Map::new();
    for key in ["fileSystem", "network"] {
        if let Some(value) = object.get(key) {
            safe.insert(key.into(), redact_permission_branch(key, value));
        }
    }
    Some(serde_json::Value::Object(safe))
}

fn redact_permission_branch(kind: &str, value: &serde_json::Value) -> serde_json::Value {
    if value.is_null() {
        return serde_json::Value::Null;
    }
    let Some(object) = value.as_object() else {
        return serde_json::Value::Null;
    };
    let mut safe = serde_json::Map::new();
    if kind == "network" {
        if let Some(value) = object.get("enabled") {
            safe.insert("enabled".into(), value.clone());
        }
    } else {
        for key in ["entries", "globScanMaxDepth", "read", "write"] {
            if let Some(value) = object.get(key) {
                safe.insert(key.into(), value.clone());
            }
        }
    }
    serde_json::Value::Object(safe)
}

fn redact_elicitation_schema(value: &serde_json::Value) -> Option<serde_json::Value> {
    // A form's constraints are part of the consent request. Dropping fields or
    // validators changes what the phone is approving. Keep the bounded schema
    // intact; the phone declines schema shapes it cannot faithfully represent.
    (value.is_object() && value.to_string().len() <= 64 * 1024).then(|| value.clone())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalResolution {
    decision: serde_json::Value,
    action_nonce: String,
    expected_state: String,
    idempotency_key: String,
    issued_at_ms: u64,
    signature: String,
    #[serde(default)]
    response_json: Option<String>,
}

async fn resolve_approval(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    Json(request): Json<ApprovalResolution>,
) -> Response {
    let Some(Extension(device)) = authenticated_device else {
        return (StatusCode::UNAUTHORIZED, "paired device required").into_response();
    };
    let _approval_guard = state.approval_lock.lock().await;
    let Some(approval) = (match state.store.begin_approval_resolution(&approval_id).await {
        Ok(Some(approval)) => Some(approval),
        Ok(None) => state
            .store
            .resume_approval_resolution(&approval_id)
            .await
            .unwrap_or(None),
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "approval lookup failed").into_response()
        }
    }) else {
        // A T3 Code question the phone stopped offering says where it waits.
        if let Ok(Some(closed)) = state.store.approval(&approval_id).await {
            if closed.decision.as_deref() == Some(t3_code::questions::ON_COMPUTER) {
                return (StatusCode::CONFLICT, t3_code::questions::ANSWER_ON_COMPUTER)
                    .into_response();
            }
            if closed.decision.as_deref() == Some(chatgpt_approvals::ON_COMPUTER) {
                return (StatusCode::CONFLICT, chatgpt_approvals::ANSWER_ON_COMPUTER)
                    .into_response();
            }
            if closed.decision.as_deref() == Some(chatgpt_approvals::RESOLVED_THERE) {
                return (StatusCode::CONFLICT, chatgpt_approvals::GONE).into_response();
            }
        }
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "already_resolved" })),
        )
            .into_response();
    };
    let recovering_resolution = approval.state == "resolving" && approval.resolution_json.is_some();
    if approval.method == "item/tool/requestUserInput" && !recovering_resolution {
        let params: serde_json::Value =
            serde_json::from_str(&approval.params_json).unwrap_or_default();
        if params
            .get("isBlocking")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
            && params
                .get("_wonderQuestionDeadlineMs")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|deadline| now_ms() >= deadline)
        {
            let _ = state.store.reset_approval_resolution(&approval_id).await;
            return (
                StatusCode::CONFLICT,
                "This optional question expired without an answer.",
            )
                .into_response();
        }
    }

    if validate_stored_approval_correlation(&approval).is_err() {
        let _ = state.store.reset_approval_resolution(&approval_id).await;
        return (
            StatusCode::BAD_REQUEST,
            "approval correlation is incomplete",
        )
            .into_response();
    }
    if approval.action_nonce.is_empty()
        || request.action_nonce != approval.action_nonce
        || request.expected_state != "pending"
        || request.idempotency_key.is_empty()
        || request.issued_at_ms.abs_diff(now_ms()) > 60_000
    {
        let _ = state.store.reset_approval_resolution(&approval_id).await;
        return (
            StatusCode::PRECONDITION_FAILED,
            "stale approval precondition",
        )
            .into_response();
    }
    let canonical_body = canonical_approval_body(&request);
    let body_sha256 = hex::encode(Sha256::digest(canonical_body.as_bytes()));
    if recovering_resolution
        && (approval.resolution_idempotency_key.as_deref()
            != Some(request.idempotency_key.as_str())
            || approval.resolution_body_sha256.as_deref() != Some(body_sha256.as_str()))
    {
        return (
            StatusCode::CONFLICT,
            "approval resolution is already in progress",
        )
            .into_response();
    }
    // A secret Cupola asked for on an agent's behalf is saved where the agent
    // named; it never becomes a provider answer or a saved resolution.
    if approval.method == thread_tools::secret::METHOD
        && thread_tools::secret::is_request(&approval.server_request_id)
    {
        // The value has its own route (`save_requested_secret`): a reply
        // here is one a phone may keep to retry, so it can only cancel.
        if request.decision.as_str() != Some("cancel") || request.response_json.is_some() {
            let _ = state.store.reset_approval_resolution(&approval_id).await;
            return (
                StatusCode::BAD_REQUEST,
                "Update Cupola on this phone to save a secret.",
            )
                .into_response();
        }
        if !approval_signed(&state, &approval, &request, &device, &body_sha256).await {
            let _ = state.store.reset_approval_resolution(&approval_id).await;
            return (StatusCode::UNAUTHORIZED, "invalid action signature").into_response();
        }
        return thread_tools::secret::answer(&state, &approval, "cancel", None).await;
    }
    let approval_params: serde_json::Value =
        serde_json::from_str(&approval.params_json).unwrap_or_default();
    // A question T3 Code asks goes back through T3 Code (`t3_code::questions`),
    // not a provider runtime: only its owning Project conversation is checked.
    let in_t3_code =
        t3_code::questions::asked_in_t3_code(&approval.server_request_id, &approval_params)
            && approval.method == "item/tool/requestUserInput";
    // So does a request ChatGPT's desktop app asks (`chatgpt_approvals`).
    let in_chatgpt =
        chatgpt_approvals::asked_in_chatgpt(&approval.server_request_id, &approval_params);
    let (approval_runtime, runtime_family) = if in_t3_code || in_chatgpt {
        if state
            .store
            .approval_push_conversation(&approval.thread_id, &approval.turn_id)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            let _ = state.store.reset_approval_resolution(&approval_id).await;
            return (
                StatusCode::CONFLICT,
                if in_chatgpt {
                    "This approval no longer belongs to an available Project. Refresh the chat."
                } else {
                    "This question no longer belongs to an available Project. Refresh the chat."
                },
            )
                .into_response();
        }
        (None, None)
    } else {
        let Some(approval_client) = state.ingestion.approval_client(&approval_params) else {
            let _ = state.store.reset_approval_resolution(&approval_id).await;
            return (
                StatusCode::CONFLICT,
                "This approval expired when the provider restarted. Refresh the chat.",
            )
                .into_response();
        };
        let owner = state
            .store
            .approval_push_conversation(&approval.thread_id, &approval.turn_id)
            .await
            .ok()
            .flatten();
        let project = if let Some(owner) = owner {
            state
                .store
                .project_conversation(&owner)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let route = approval_params["_wonderRuntimeId"]
            .as_str()
            .and_then(|id| state.ingestion.runtime_route(id));
        let runtime_family = route.as_ref().map(|route| route.family);
        if !project
            .zip(route)
            .is_some_and(|(project, route)| project.family == route.family)
            || !approval_params
                .get("_wonderRequestId")
                .is_some_and(|id| id.is_string() || id.is_number())
        {
            let _ = state.store.reset_approval_resolution(&approval_id).await;
            return (
                StatusCode::CONFLICT,
                "This approval no longer belongs to an available Project. Refresh the chat.",
            )
                .into_response();
        }
        let approval_runtime = approval_client.lock().await.rpc();
        if approval_params
            .get("_wonderRuntimeId")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| id != approval_runtime.health().id())
            || !approval_runtime.health().is_alive()
        {
            let _ = state.store.reset_approval_resolution(&approval_id).await;
            return (
                StatusCode::CONFLICT,
                "This approval expired when the provider restarted. Refresh the chat.",
            )
                .into_response();
        }
        (Some(approval_runtime), runtime_family)
    };
    let response = match build_approval_response(
        &approval.method,
        &approval.params_json,
        &request,
        runtime_family,
    )
    .and_then(|response| {
        // Recovery must satisfy the same native capability checks as a new reply.
        if recovering_resolution {
            let stored: serde_json::Value =
                serde_json::from_str(approval.resolution_json.as_deref().unwrap_or("{}"))
                    .map_err(|_| "stored approval response is invalid")?;
            if stored != response {
                return Err("stored approval response does not match the signed reply");
            }
        }
        Ok(response)
    }) {
        Ok(response) => response,
        Err(message) => {
            let _ = state.store.reset_approval_resolution(&approval_id).await;
            return (StatusCode::BAD_REQUEST, message).into_response();
        }
    };
    if !approval_signed(&state, &approval, &request, &device, &body_sha256).await {
        let _ = state.store.reset_approval_resolution(&approval_id).await;
        return (StatusCode::UNAUTHORIZED, "invalid action signature").into_response();
    }
    let response_json = serde_json::to_string(&response).unwrap_or_else(|_| "null".into());
    let persisted = state
        .store
        .persist_approval_resolution_intent(
            &approval_id,
            &request.idempotency_key,
            &body_sha256,
            &response_json,
        )
        .await
        .unwrap_or(false);
    if !persisted {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "approval persistence failed",
        )
            .into_response();
    }
    let mut decision = decision_label(&request.decision);
    if let Some(approval_runtime) = approval_runtime {
        let server_id = approval_params["_wonderRequestId"].clone();
        let response = approval_runtime
            .respond_value(server_id, Some(response), None)
            .await;
        drop(approval_runtime);
        if response.is_err() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "App Server response failed",
            )
                .into_response();
        }
    } else if in_chatgpt {
        match chatgpt_approvals::answer(
            &state,
            &approval.server_request_id,
            &approval.method,
            &approval_params,
            &response,
        )
        .await
        {
            chatgpt_approvals::Answered::Sent => {}
            chatgpt_approvals::Answered::Gone => {
                decision = chatgpt_approvals::RESOLVED_THERE.into()
            }
            chatgpt_approvals::Answered::Failed {
                message,
                sent_nothing,
            } => {
                // Never reached ChatGPT: answerable anew. Otherwise the same
                // answer is sent again (ChatGPT ignores one it already took).
                if sent_nothing {
                    let _ = state.store.abandon_approval_resolution(&approval_id).await;
                }
                return (StatusCode::SERVICE_UNAVAILABLE, message).into_response();
            }
        }
    } else {
        match t3_code::questions::answer(&state, &approval_params, &response, recovering_resolution)
            .await
        {
            t3_code::questions::Answered::Sent => {}
            t3_code::questions::Answered::Gone => {
                decision = t3_code::questions::RESOLVED_THERE.into()
            }
            t3_code::questions::Answered::Failed {
                message,
                sent_nothing,
            } => {
                // Never reached T3 Code: answerable anew. Otherwise the same
                // answer is retried (T3 Code takes only one).
                if sent_nothing {
                    let _ = state.store.abandon_approval_resolution(&approval_id).await;
                }
                return (StatusCode::SERVICE_UNAVAILABLE, message).into_response();
            }
        }
    }
    if state
        .store
        .finish_approval_resolution(&approval_id, &decision, &now_ms().to_string())
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "approval persistence failed",
        )
            .into_response();
    }
    let _ = publish_event_with_context(
        &state,
        WonderEvent::ApprovalResolved {
            request_id: approval_id.clone(),
            decision: decision.clone(),
        },
        EventContext {
            request_id: Some(approval_id.clone()),
            approval_id: Some(approval_id),
            ..EventContext::default()
        },
    )
    .await;
    if decision == t3_code::questions::RESOLVED_THERE {
        return (StatusCode::CONFLICT, t3_code::questions::GONE).into_response();
    }
    if decision == chatgpt_approvals::RESOLVED_THERE {
        return (StatusCode::CONFLICT, chatgpt_approvals::GONE).into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The phone's answer to an agent's secret request, on its own signed route:
/// the value is never part of an approval reply, saved intent or log line.
/// No `Debug`, so the value cannot be printed by accident.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SecretAnswer {
    /// `save` (with `value`) or `cancel`.
    decision: String,
    #[serde(default)]
    value: Option<String>,
    action_nonce: String,
    expected_state: String,
    idempotency_key: String,
    issued_at_ms: u64,
    signature: String,
}

/// The signed body: decision, nonce, state, key and value, in this order.
fn canonical_secret_body(answer: &SecretAnswer) -> String {
    serde_json::to_string(&serde_json::json!([
        answer.decision,
        answer.action_nonce,
        answer.expected_state,
        answer.idempotency_key,
        answer.value.as_deref().unwrap_or_default(),
    ]))
    .expect("canonical secret body is serializable")
}

async fn save_requested_secret(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    authenticated_device: Option<Extension<AuthenticatedDevice>>,
    Json(answer): Json<SecretAnswer>,
) -> Response {
    let Some(Extension(device)) = authenticated_device else {
        return (StatusCode::UNAUTHORIZED, "paired device required").into_response();
    };
    let _approval_guard = state.approval_lock.lock().await;
    let approval = match state.store.begin_approval_resolution(&approval_id).await {
        Ok(Some(approval)) => approval,
        Ok(None) => match state.store.resume_approval_resolution(&approval_id).await {
            Ok(Some(approval)) => approval,
            _ => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({ "error": "already_resolved" })),
                )
                    .into_response()
            }
        },
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "approval lookup failed").into_response()
        }
    };
    if approval.method != thread_tools::secret::METHOD
        || !thread_tools::secret::is_request(&approval.server_request_id)
    {
        let _ = state.store.reset_approval_resolution(&approval_id).await;
        return (StatusCode::NOT_FOUND, "This is not a request for a secret.").into_response();
    }
    if approval.action_nonce.is_empty()
        || answer.action_nonce != approval.action_nonce
        || answer.expected_state != "pending"
        || answer.idempotency_key.is_empty()
        || answer.issued_at_ms.abs_diff(now_ms()) > 60_000
    {
        let _ = state.store.reset_approval_resolution(&approval_id).await;
        return (
            StatusCode::PRECONDITION_FAILED,
            "stale approval precondition",
        )
            .into_response();
    }
    let body_sha256 = hex::encode(Sha256::digest(canonical_secret_body(&answer).as_bytes()));
    let target = format!("/api/v1/secret-requests/{approval_id}");
    if !action_signed(
        &state,
        ("secret.save", &target),
        &approval,
        (
            answer.issued_at_ms,
            &answer.expected_state,
            &answer.signature,
        ),
        &device,
        &body_sha256,
    )
    .await
    {
        let _ = state.store.reset_approval_resolution(&approval_id).await;
        return (StatusCode::UNAUTHORIZED, "invalid action signature").into_response();
    }
    thread_tools::secret::answer(&state, &approval, &answer.decision, answer.value.as_deref()).await
}

async fn approval_signed(
    state: &AppState,
    approval: &wonder_store::StoredApproval,
    request: &ApprovalResolution,
    device: &AuthenticatedDevice,
    body_sha256: &str,
) -> bool {
    let action_path = format!("/api/v1/approvals/{}/resolve", approval.approval_id);
    action_signed(
        state,
        ("approval.resolve", &action_path),
        approval,
        (
            request.issued_at_ms,
            &request.expected_state,
            &request.signature,
        ),
        device,
        body_sha256,
    )
    .await
}

/// A paired device's signature over `action` on `target` for this request.
async fn action_signed(
    state: &AppState,
    (action, target): (&str, &str),
    approval: &wonder_store::StoredApproval,
    (issued_at_ms, expected_state, signature): (u64, &str, &str),
    device: &AuthenticatedDevice,
    body_sha256: &str,
) -> bool {
    let transcript = ActionTranscript {
        action,
        target,
        body_sha256,
        action_nonce: &approval.action_nonce,
        session_binding: &device.session_binding,
        device_id: &device.device_id,
        host_installation_id: &state.host_installation_id,
        issued_at_ms,
        expected_state,
    };
    state
        .pairing
        .lock()
        .await
        .verify_action_signature(&device.device_id, &transcript, signature)
        .is_ok()
}

fn validate_stored_approval_correlation(
    approval: &wonder_store::StoredApproval,
) -> Result<(), &'static str> {
    let requires_thread_turn = matches!(
        approval.method.as_str(),
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
            | "item/tool/call"
    );
    if requires_thread_turn && (approval.thread_id.is_empty() || approval.turn_id.is_empty()) {
        return Err("approval requires threadId and turnId");
    }
    if approval.method == "mcpServer/elicitation/request" && approval.thread_id.is_empty() {
        return Err("MCP elicitation requires threadId");
    }
    Ok(())
}

/// The detached action signature covers this stable unsigned body projection,
/// rather than serializer-dependent JSON key order or signature bytes.
fn canonical_approval_body(request: &ApprovalResolution) -> String {
    serde_json::to_string(&serde_json::json!([
        request.decision,
        request.action_nonce,
        request.expected_state,
        request.idempotency_key,
        request.response_json.as_deref().unwrap_or_default(),
    ]))
    .expect("canonical approval body is serializable")
}

fn decision_label(decision: &serde_json::Value) -> String {
    decision
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| serde_json::to_string(decision).unwrap_or_else(|_| "invalid".into()))
}

fn validate_command_decision(
    decision: &serde_json::Value,
    advertised: Option<&[serde_json::Value]>,
    runtime_family: Option<AgentFamily>,
) -> Result<(), &'static str> {
    let valid = match decision {
        serde_json::Value::String(value) => matches!(
            value.as_str(),
            "accept" | "acceptForSession" | "decline" | "cancel"
        ),
        serde_json::Value::Object(object) if object.len() == 1 => {
            if let Some((option, owner)) = object
                .get("opencodePermission")
                .map(|v| (v, AgentFamily::OpenCode))
                .or_else(|| {
                    object
                        .get("acpPermission")
                        .map(|v| (v, AgentFamily::Cursor))
                })
            {
                // Only the authenticated owning runtime can offer these
                // provider-owned rules. The exact signed object must also be
                // present in this request's stored advertised decisions.
                runtime_family == Some(owner)
                    && advertised.is_some()
                    && option.as_object().is_some_and(|fields| fields.len() == 2)
                    && option["optionId"]
                        .as_str()
                        .is_some_and(|id| !id.trim().is_empty())
                    && option["kind"]
                        .as_str()
                        .is_some_and(|kind| matches!(kind, "allow_always" | "reject_always"))
            } else if let Some(amendment) = object.get("acceptWithExecpolicyAmendment") {
                amendment
                    .get("execpolicy_amendment")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|items| items.iter().all(serde_json::Value::is_string))
            } else if let Some(amendment) = object.get("applyNetworkPolicyAmendment") {
                amendment
                    .get("network_policy_amendment")
                    .is_some_and(|value| value.is_object())
            } else {
                false
            }
        }
        _ => false,
    };
    if !valid {
        return Err("decision has an invalid App Server approval shape");
    }
    if advertised.is_some_and(|values| !values.iter().any(|value| value == decision)) {
        return Err("decision was not advertised");
    }
    Ok(())
}

fn build_approval_response(
    method: &str,
    params_json: &str,
    request: &ApprovalResolution,
    runtime_family: Option<AgentFamily>,
) -> Result<serde_json::Value, &'static str> {
    let response_json = request.response_json.as_deref();
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let params: serde_json::Value = serde_json::from_str(params_json)
                .map_err(|_| "approval parameters must be valid JSON")?;
            let advertised = params
                .get("availableDecisions")
                .and_then(serde_json::Value::as_array);
            validate_command_decision(
                &request.decision,
                advertised.map(Vec::as_slice),
                runtime_family,
            )?;
            if let Some(response_json) = response_json {
                let response: serde_json::Value = serde_json::from_str(response_json)
                    .map_err(|_| "responseJson must be valid JSON")?;
                let response_decision = response
                    .get("decision")
                    .ok_or("approval response must contain a decision")?;
                if response_decision != &request.decision {
                    return Err("approval response decision does not match decision");
                }
                let fields = response
                    .as_object()
                    .ok_or("approval response must be an object")?;
                if fields
                    .keys()
                    .any(|key| key != "decision" && key != "message")
                {
                    return Err("unsupported approval response field");
                }
                if let Some(message) = fields.get("message") {
                    let params: serde_json::Value = serde_json::from_str(params_json)
                        .map_err(|_| "approval parameters must be valid JSON")?;
                    if !runtime_family
                        .is_some_and(|family| family.provider().capabilities.deny_with_instruction)
                        || method != "item/commandExecution/requestApproval"
                        || params["kind"] != "command"
                        || params["supportsDenyMessage"] != true
                        || request.decision != "decline"
                    {
                        return Err("this approval does not support a denial instruction");
                    }
                    let message = message.as_str().ok_or("denial instruction must be text")?;
                    if message.trim().is_empty() || message.len() > 2000 {
                        return Err("denial instruction must contain 1 to 2000 UTF-8 bytes");
                    }
                }
                Ok(response)
            } else {
                Ok(serde_json::json!({ "decision": request.decision }))
            }
        }
        "item/permissions/requestApproval" => {
            let response_json =
                response_json.ok_or("permissions approval requires responseJson")?;
            let response: serde_json::Value = serde_json::from_str(response_json)
                .map_err(|_| "responseJson must be valid JSON")?;
            match request.decision.as_str() {
                Some("accept" | "acceptForSession") => {
                    validate_permissions_response(params_json, &response)?
                }
                Some("decline" | "cancel") => {
                    if response != serde_json::json!({"permissions": {}, "scope": "turn"}) {
                        return Err("declined permission requests cannot grant access");
                    }
                }
                _ => return Err("invalid permission decision"),
            }
            Ok(response)
        }
        "item/tool/requestUserInput" => {
            let response_json = response_json.ok_or("user-input approval requires responseJson")?;
            let response: serde_json::Value = serde_json::from_str(response_json)
                .map_err(|_| "responseJson must be valid JSON")?;
            validate_user_input_response(params_json, &response)?;
            Ok(response)
        }
        "mcpServer/elicitation/request" => {
            let response_json = response_json.ok_or("MCP elicitation requires responseJson")?;
            let response: serde_json::Value = serde_json::from_str(response_json)
                .map_err(|_| "responseJson must be valid JSON")?;
            validate_mcp_response(
                params_json,
                request
                    .decision
                    .as_str()
                    .ok_or("MCP decision must be a string")?,
                &response,
            )?;
            Ok(response)
        }
        "item/tool/call" => {
            if request.decision.as_str() != Some("respond") {
                return Err("dynamic tool response requires the respond decision");
            }
            let response_json =
                response_json.ok_or("dynamic tool response requires responseJson")?;
            let response: serde_json::Value = serde_json::from_str(response_json)
                .map_err(|_| "responseJson must be valid JSON")?;
            validate_dynamic_tool_response(&response)?;
            Ok(response)
        }
        _ => Err("unsupported App Server approval request type"),
    }
}

fn validate_permissions_response(
    params_json: &str,
    response: &serde_json::Value,
) -> Result<(), &'static str> {
    let requested = serde_json::from_str::<serde_json::Value>(params_json)
        .ok()
        .and_then(|params| params.get("permissions").cloned())
        .ok_or("permission request did not contain a permission profile")?;
    validate_permission_profile(&requested)?;
    let permissions = response
        .get("permissions")
        .ok_or("permissions response must contain permissions")?;
    if response.as_object().is_none_or(|object| {
        object
            .keys()
            .any(|key| !matches!(key.as_str(), "permissions" | "scope" | "strictAutoReview"))
    }) {
        return Err("permissions response contains an unknown field");
    }
    validate_permission_profile(permissions)?;
    if !permission_profile_subset(permissions, &requested) {
        return Err("granted permissions exceed the requested profile");
    }
    if response
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|scope| !matches!(scope, "turn" | "session"))
    {
        return Err("permission scope must be turn or session");
    }
    if response
        .get("strictAutoReview")
        .is_some_and(|value| !value.is_boolean() && !value.is_null())
    {
        return Err("strictAutoReview must be boolean or null");
    }
    Ok(())
}

fn validate_permission_profile(value: &serde_json::Value) -> Result<(), &'static str> {
    let object = value
        .as_object()
        .ok_or("permission profile must be an object")?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "fileSystem" | "network"))
    {
        return Err("permission profile contains an unknown field");
    }
    validate_filesystem_permissions(object.get("fileSystem").unwrap_or(&serde_json::Value::Null))?;
    validate_network_permissions(object.get("network").unwrap_or(&serde_json::Value::Null))
}

fn validate_filesystem_permissions(value: &serde_json::Value) -> Result<(), &'static str> {
    let Some(object) = value.as_object() else {
        return if value.is_null() {
            Ok(())
        } else {
            Err("fileSystem permissions must be an object or null")
        };
    };
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "entries" | "globScanMaxDepth" | "read" | "write"
        )
    }) {
        return Err("fileSystem permissions contain an unknown field");
    }
    if let Some(value) = object.get("globScanMaxDepth") {
        if !value.is_null() && value.as_i64().is_none_or(|depth| depth < 1) {
            return Err("globScanMaxDepth must be a positive integer or null");
        }
    }
    for key in ["read", "write"] {
        if let Some(value) = object.get(key) {
            if !value.is_null()
                && !value
                    .as_array()
                    .is_some_and(|items| items.iter().all(serde_json::Value::is_string))
            {
                return Err("filesystem legacy permissions must be string arrays or null");
            }
        }
    }
    if let Some(value) = object.get("entries") {
        if !value.is_null()
            && !value
                .as_array()
                .is_some_and(|items| items.iter().all(validate_filesystem_entry))
        {
            return Err("filesystem entries are malformed");
        }
    }
    Ok(())
}

fn validate_network_permissions(value: &serde_json::Value) -> Result<(), &'static str> {
    let Some(object) = value.as_object() else {
        return if value.is_null() {
            Ok(())
        } else {
            Err("network permissions must be an object or null")
        };
    };
    if object.keys().any(|key| key != "enabled")
        || object
            .get("enabled")
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
    {
        return Err("network permissions are malformed");
    }
    Ok(())
}

fn validate_filesystem_entry(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    matches!(
        object.get("access").and_then(serde_json::Value::as_str),
        Some("read" | "write" | "deny")
    ) && validate_filesystem_path(object.get("path").unwrap_or(&serde_json::Value::Null))
}

fn validate_filesystem_path(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("path") => object.get("path").is_some_and(serde_json::Value::is_string),
        Some("glob_pattern") => object
            .get("pattern")
            .is_some_and(serde_json::Value::is_string),
        Some("special") => object
            .get("value")
            .is_some_and(validate_filesystem_special_path),
        _ => false,
    }
}

fn validate_filesystem_special_path(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let Some(kind) = object.get("kind").and_then(serde_json::Value::as_str) else {
        return false;
    };
    match kind {
        "root" | "minimal" | "tmpdir" | "slash_tmp" => object.len() == 1,
        "project_roots" => {
            object
                .keys()
                .all(|key| matches!(key.as_str(), "kind" | "subpath"))
                && object
                    .get("subpath")
                    .is_none_or(|value| value.is_null() || value.is_string())
        }
        "unknown" => {
            object
                .keys()
                .all(|key| matches!(key.as_str(), "kind" | "path" | "subpath"))
                && object.get("path").is_some_and(serde_json::Value::is_string)
                && object
                    .get("subpath")
                    .is_none_or(|value| value.is_null() || value.is_string())
        }
        _ => false,
    }
}

fn permission_profile_subset(granted: &serde_json::Value, requested: &serde_json::Value) -> bool {
    let (Some(granted), Some(requested)) = (granted.as_object(), requested.as_object()) else {
        return false;
    };
    filesystem_subset(
        granted
            .get("fileSystem")
            .unwrap_or(&serde_json::Value::Null),
        requested
            .get("fileSystem")
            .unwrap_or(&serde_json::Value::Null),
    ) && network_subset(
        granted.get("network").unwrap_or(&serde_json::Value::Null),
        requested.get("network").unwrap_or(&serde_json::Value::Null),
    )
}

fn filesystem_subset(granted: &serde_json::Value, requested: &serde_json::Value) -> bool {
    if granted.is_null() {
        return true;
    }
    let (Some(granted), Some(requested)) = (granted.as_object(), requested.as_object()) else {
        return false;
    };
    for key in ["read", "write", "entries"] {
        let Some(granted_values) = granted.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let Some(requested_values) = requested.get(key).and_then(serde_json::Value::as_array)
        else {
            return false;
        };
        if !granted_values.as_array().is_some_and(|items| {
            items
                .iter()
                .all(|item| requested_values.iter().any(|allowed| allowed == item))
        }) {
            return false;
        }
    }
    match (
        granted.get("globScanMaxDepth"),
        requested.get("globScanMaxDepth"),
    ) {
        (Some(value), Some(allowed)) if !value.is_null() => value
            .as_i64()
            .zip(allowed.as_i64())
            .is_some_and(|(value, allowed)| value <= allowed),
        (Some(value), None) if !value.is_null() => false,
        _ => true,
    }
}

fn network_subset(granted: &serde_json::Value, requested: &serde_json::Value) -> bool {
    if granted.is_null()
        || granted.get("enabled").and_then(serde_json::Value::as_bool) != Some(true)
    {
        return true;
    }
    requested
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

fn validate_user_input_response(
    params_json: &str,
    response: &serde_json::Value,
) -> Result<(), &'static str> {
    let questions = serde_json::from_str::<serde_json::Value>(params_json)
        .ok()
        .and_then(|params| params.get("questions").cloned())
        .and_then(|questions| questions.as_array().cloned())
        .ok_or("user-input request did not contain questions")?;
    let answers = response
        .get("answers")
        .and_then(serde_json::Value::as_object)
        .ok_or("user-input response must contain an answers object")?;
    if answers.keys().any(|id| {
        !questions
            .iter()
            .any(|question| question.get("id").and_then(serde_json::Value::as_str) == Some(id))
    }) {
        return Err("user-input response contains an unknown question id");
    }
    if answers.values().any(|answer| {
        answer
            .get("answers")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|values| values.iter().any(|value| !value.is_string()))
    }) {
        return Err("each user-input answer must contain string values");
    }
    Ok(())
}

fn validate_mcp_response(
    params_json: &str,
    decision: &str,
    response: &serde_json::Value,
) -> Result<(), &'static str> {
    let action = response
        .get("action")
        .and_then(serde_json::Value::as_str)
        .ok_or("MCP response must contain an action")?;
    if !matches!(action, "accept" | "decline" | "cancel") || action != decision {
        return Err("MCP response action does not match decision");
    }
    if action != "accept" {
        if response
            .get("content")
            .is_some_and(|value| !value.is_null())
        {
            return Err("declined service requests cannot include form answers");
        }
        return Ok(());
    }
    let params: serde_json::Value =
        serde_json::from_str(params_json).map_err(|_| "invalid service request")?;
    match params.get("mode").and_then(serde_json::Value::as_str) {
        Some("url") => {
            if response
                .get("content")
                .is_some_and(|value| !value.is_null())
            {
                return Err("browser confirmations cannot include form answers");
            }
        }
        Some("form" | "openai/form" | "openaiForm") => {
            validate_elicitation_content(&params, response)?;
        }
        None if params.get("requestedSchema").is_some() => {
            validate_elicitation_content(&params, response)?;
        }
        None => {} // Legacy confirmation requests have no form schema.
        Some(_) => return Err("this service request can only be declined or cancelled"),
    }
    Ok(())
}

fn validate_elicitation_content(
    params: &serde_json::Value,
    response: &serde_json::Value,
) -> Result<(), &'static str> {
    let schema = params
        .get("requestedSchema")
        .and_then(redact_elicitation_schema)
        .ok_or("service form schema is missing or too large")?;
    // Never let a service-supplied schema read a file or make a network request.
    // Standard MCP forms are inline. Future reference-based forms remain declineable.
    fn contains_reference(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(object) => object.iter().any(|(key, value)| {
                matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef")
                    || contains_reference(value)
            }),
            serde_json::Value::Array(values) => values.iter().any(contains_reference),
            _ => false,
        }
    }
    if contains_reference(&schema) {
        return Err("referenced service schemas are unsupported");
    }
    let validator = jsonschema::options()
        .offline()
        .should_validate_formats(true)
        .build(&schema)
        .map_err(|_| "service form schema is invalid")?;
    let content = response
        .get("content")
        .ok_or("accepted service forms require answers")?;
    if !validator.is_valid(content) {
        return Err("service answers do not match the requested form");
    }
    Ok(())
}

fn validate_dynamic_tool_response(response: &serde_json::Value) -> Result<(), &'static str> {
    if response.get("success").and_then(serde_json::Value::as_bool) != Some(false) {
        return Err("Cupola can only decline unregistered dynamic tool calls");
    }
    if !response
        .get("contentItems")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        return Err("declined dynamic tool calls must have empty contentItems");
    }
    Ok(())
}

/// Why the host refused a device's sign-in, in words for the host log.
/// `None` for a pairing still waiting on confirmation: the phone polls
/// through that state, so it is not worth a line.
fn pairing_refusal_reason(error: &PairingError) -> Option<&'static str> {
    Some(match error {
        PairingError::NotFound => "unknown device (never paired with this computer, or forgotten)",
        PairingError::BindingMismatch => "device was revoked",
        PairingError::SessionExpired => "pairing expired; pair the device again",
        PairingError::ChallengeNotFound => {
            "sign-in challenge unknown (host restarted or challenge pruned)"
        }
        PairingError::ChallengeExpired => "sign-in challenge expired",
        PairingError::ChallengeReplayed => "sign-in challenge already used",
        PairingError::InvalidSignature => "signature did not match the paired device key",
        PairingError::ConfirmationRequired => return None,
        _ => "request was malformed",
    })
}

/// First characters of a device or challenge id for the host log: enough to
/// match the phone's diagnostics, never the whole value, and only id-safe
/// characters because the value may come from an unauthenticated caller.
fn id_prefix(id: &str) -> String {
    let prefix: String = id
        .chars()
        .take(8)
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if prefix.is_empty() {
        "(none)".into()
    } else {
        format!("{prefix}…")
    }
}

/// Logs one refused device sign-in or device action. Callers can be
/// unauthenticated, so lines are capped per minute to keep the log readable.
fn log_device_auth_refusal(context: &str, subject: &str, reason: &str) {
    const LINES_PER_MINUTE: u32 = 30;
    static WINDOW: std::sync::Mutex<(u64, u32)> = std::sync::Mutex::new((0, 0));
    let minute = now_ms() / 60_000;
    let count = {
        let mut window = WINDOW
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if window.0 != minute {
            *window = (minute, 0);
        }
        window.1 = window.1.saturating_add(1);
        window.1
    };
    if count <= LINES_PER_MINUTE {
        eprintln!("Cupola refused {context} for {subject}: {reason}");
    } else if count == LINES_PER_MINUTE + 1 {
        eprintln!("Cupola refused more device sign-ins this minute; further lines are skipped until the next minute");
    }
}

fn pairing_error_response(error: PairingError) -> Response {
    if error == PairingError::ConfirmationRequired {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "confirmation_required"})),
        )
            .into_response();
    }
    let status = match error {
        PairingError::BindingMismatch | PairingError::SessionExpired => StatusCode::UNAUTHORIZED,
        PairingError::NotFound
        | PairingError::ChallengeNotFound
        | PairingError::SessionNotFound => StatusCode::NOT_FOUND,
        PairingError::Expired
        | PairingError::ChallengeExpired
        | PairingError::AlreadyConsumed
        | PairingError::ChallengeReplayed => StatusCode::GONE,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "error": "pairing_failed" })),
    )
        .into_response()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(all(test, unix))]
mod approval_routing_tests;
#[cfg(all(test, unix))]
mod deleted_chats_tests;
#[cfg(all(test, unix))]
mod search_tests;
#[cfg(all(test, unix))]
mod status_truth_tests;

#[cfg(all(test, unix))]
mod tests {
    use super::{
        app_server_notification_key, attachment_path, attachment_relative_path,
        build_approval_response, canonical_approval_body, conversation_thread_projection,
        conversation_thread_projection_with_items, default_session_expiration,
        drain_pending_app_server_notifications, extract_text, find_client_message,
        host_readiness_state, inherited_child_resume_params, is_valid_public_origin,
        matches_public_origin, next_automation_run, now_ms, pairing_offer_response,
        project_app_server_notification, publish_app_server_notification, route_auth, router,
        runtime_app_summary, runtime_mcp_server_summary, runtime_skill_summary,
        sanitize_ansi_and_secrets, sanitize_typed_item, sanitized_relative_path,
        search_result_deep_link, steer_response_turn_id, steer_turn_params_with_attachments,
        trusted_origins_from_status_log, turn_input, valid_attachment_ids,
        valid_attachment_mime_type, valid_attachment_name, valid_device_label, AppServerThreadItem,
        AppState, ApprovalResolution, ChoiceOption, HostEventEnvelope, ModelOption, OriginKind,
        RouteAuth, RuntimeCatalog, WonderEvent, MAX_ATTACHMENT_BYTES,
    };
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wonder_store::{StoredAssistantMessage, StoredMessage};

    pub(super) fn validate_http_contract(definition: &str, value: &serde_json::Value) {
        let authority: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/schemas/wonder-http-v1.json"
        ))
        .expect("HTTP schema JSON");
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": format!("#/$defs/{definition}"),
            "$defs": authority["$defs"]
        });
        let validator = jsonschema::validator_for(&schema).expect("valid HTTP schema");
        let errors: Vec<_> = validator
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect();
        assert!(errors.is_empty(), "{definition}: {errors:?}");
    }

    #[test]
    fn computer_signaling_contract_requires_peer_revision_for_mutations() {
        let authority: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/schemas/wonder-http-v1.json"
        ))
        .expect("HTTP schema JSON");
        let validator = |definition: &str| {
            jsonschema::validator_for(&serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$ref": format!("#/$defs/{definition}"),
                "$defs": authority["$defs"]
            }))
            .expect("valid signaling schema")
        };
        let binding = serde_json::json!({
            "generation": 1,
            "conversationId": "conversation",
            "hostInstallationId": "host"
        });
        let mut poll = binding.clone();
        poll["cursor"] = serde_json::json!(0);
        validate_http_contract("computerSignalingPollRequest", &poll);

        let mut answer = binding.clone();
        answer["peerRevision"] = serde_json::json!("peer");
        answer["type"] = serde_json::json!("answer");
        answer["sdp"] = serde_json::json!("v=0\na=x");
        assert!(validator("computerSignalingAnswerRequest").is_valid(&answer));
        answer.as_object_mut().unwrap().remove("peerRevision");
        assert!(!validator("computerSignalingAnswerRequest").is_valid(&answer));

        poll["unexpected"] = serde_json::json!(true);
        assert!(!validator("computerSignalingPollRequest").is_valid(&poll));
    }

    #[test]
    fn send_message_contract_carries_the_project_model_the_host_reads() {
        let authority: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/schemas/wonder-http-v1.json"
        ))
        .expect("HTTP schema JSON");
        let validator = jsonschema::validator_for(&serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "#/$defs/sendMessageRequest",
            "$defs": authority["$defs"]
        }))
        .expect("valid send schema");
        let send = serde_json::json!({
            "deviceId": "device",
            "clientMessageId": "client-1",
            "body": "Continue with Claude",
            "projectModel": {
                "family": "claude",
                "model": "claude-sonnet-5-5",
                "effort": "high",
                "serviceTier": null
            }
        });
        validate_http_contract("sendMessageRequest", &send);
        let request: super::SendMessageRequest =
            serde_json::from_value(send.clone()).expect("host reads the send");
        assert_eq!(
            request.project_model.map(|model| model.family),
            Some(wonder_store::AgentFamily::Claude)
        );

        let mut minimal = send.clone();
        minimal["projectModel"] = serde_json::json!({ "family": "codex", "model": "gpt-6.1" });
        validate_http_contract("sendMessageRequest", &minimal);

        let mut unknown = send.clone();
        unknown["unexpected"] = serde_json::json!(true);
        assert!(!validator.is_valid(&unknown));
        let mut unknown_model_field = send.clone();
        unknown_model_field["projectModel"]["provider"] = serde_json::json!("claude");
        assert!(!validator.is_valid(&unknown_model_field));
        let mut missing_model = send;
        missing_model["projectModel"]
            .as_object_mut()
            .unwrap()
            .remove("model");
        assert!(!validator.is_valid(&missing_model));
    }

    #[test]
    fn child_resume_never_overrides_inherited_execution_settings() {
        let params = inherited_child_resume_params("child-thread");
        assert_eq!(
            params,
            serde_json::json!({
                "threadId": "child-thread",
                "excludeTurns": true
            })
        );
        for forbidden in [
            "cwd",
            "permissions",
            "runtimeWorkspaceRoots",
            "approvalPolicy",
            "approvalsReviewer",
            "model",
            "developerInstructions",
        ] {
            assert!(
                params.get(forbidden).is_none(),
                "must not override {forbidden}"
            );
        }
    }

    #[tokio::test]
    async fn child_user_mutations_and_saved_intent_are_read_only() {
        let (dir, state) = crate::ingestion::tests::fixture().await;

        crate::test_support::bind_project_thread(
            &state,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            wonder_store::AgentFamily::Codex,
            "thread",
            Some("session"),
            "now",
        )
        .await
        .unwrap();
        std::fs::write(dir.path().join("child-fixture"), "").unwrap();
        // Discovery needs a runtime route, not concurrent recovery and retention tasks.
        let health = state.projects.codex.lock().await.health();
        state
            .ingestion
            .register_project_runtime(&state.projects.codex, health);

        let children = crate::subagents::list_runtime_threads(
            &state,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9",
            "thread",
        )
        .await
        .unwrap();
        assert_eq!(children.len(), 1);
        let child = children[0].ownership.conversation_id.clone();
        assert!(crate::subagents::reject_user_mutation(
            &state,
            "d19b0f7e-97bb-45d7-8206-324a62386dd9"
        )
        .await
        .is_none());
        let sent = super::send_message_inner(
            axum::extract::State(state.clone()),
            axum::extract::Path(child.clone()),
            None,
            axum::Json(super::SendMessageRequest {
                project_model: None,
                codex_skills: vec![],
                device_id: "owner".into(),
                client_message_id: uuid::Uuid::new_v4().to_string(),
                body: "keep this unsent".into(),
                attachment_ids: vec![],
                take_over: false,
            }),
        )
        .await;
        assert_eq!(sent.status(), axum::http::StatusCode::FORBIDDEN);
        let guided = super::steer_turn(
            axum::extract::State(state.clone()),
            axum::extract::Path((child.clone(), "turn".into())),
            None,
            axum::Json(super::SteerTurnRequest {
                codex_skills: vec![],
                device_id: "owner".into(),
                client_message_id: uuid::Uuid::new_v4().to_string(),
                body: "keep this guide".into(),
                attachment_ids: vec![],
                expected_turn_id: "turn".into(),
            }),
        )
        .await;
        assert_eq!(guided.status(), axum::http::StatusCode::FORBIDDEN);
        let stopped = super::interrupt_turn(
            axum::extract::State(state.clone()),
            axum::extract::Path((child.clone(), "turn".into())),
        )
        .await;
        assert_eq!(stopped.status(), axum::http::StatusCode::FORBIDDEN);
        let upload = super::upload_conversation_file(
            axum::extract::State(state.clone()),
            axum::extract::Path(child.clone()),
            axum::Json(super::CreateConversationFileRequest {
                client_upload_id: None,
                name: "file.txt".into(),
                mime_type: None,
                content_base64: "aGVsbG8=".into(),
            }),
        )
        .await;
        assert_eq!(upload.status(), axum::http::StatusCode::FORBIDDEN);

        // An older client may have persisted a send before upgrading the host.
        let message = match state
            .store
            .insert_dispatch_message(
                "owner",
                "00000000-0000-4000-8000-000000000061",
                "preserved child draft",
                "hash",
                &child,
                &[],
                "now",
                true,
            )
            .await
            .unwrap()
        {
            wonder_store::MessageInsert::Inserted(message) => message,
            _ => panic!("new legacy child message"),
        };
        crate::projects::dispatch(state.clone(), message.clone()).await;
        let retained = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.body, "preserved child draft");
        assert!(retained.codex_turn_id.is_none());
        assert!(matches!(
            retained.state.as_str(),
            "failed" | "safe_to_retry"
        ));
        let guide = match state
            .store
            .insert_guide_message(
                "owner",
                "00000000-0000-4000-8000-000000000062",
                "preserved child guide",
                "hash",
                &child,
                &[],
                "now",
                "turn",
            )
            .await
            .unwrap()
        {
            wonder_store::MessageInsert::Inserted(message) => message,
            _ => panic!("new legacy guide"),
        };
        let guided = super::dispatch_guide(state.clone(), guide, "turn".into()).await;
        assert_eq!(guided.status(), axum::http::StatusCode::CONFLICT);

        let requests = std::fs::read_to_string(dir.path().join("requests-jsonl")).unwrap();
        for line in requests.lines() {
            let request: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(
                ![
                    "thread/resume",
                    "turn/start",
                    "turn/steer",
                    "turn/interrupt"
                ]
                .contains(&request["method"].as_str().unwrap_or("")),
                "unexpected runtime mutation: {}",
                request["method"]
            );
        }
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// The host status a phone reads names every route the running host
    /// serves, in the kinds the phone decodes. The saved fixture, which the
    /// Swift package decodes with the phone's route type, has exactly this
    /// response's fields and routes.
    #[tokio::test]
    async fn host_status_publishes_the_running_routes_a_phone_decodes() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/contracts/native/host-status.json"
        ))
        .unwrap();
        let pin = fixture["hostKeyPin"].as_str().unwrap();
        let (directory, mut state) = crate::test_support::fixture().await;
        let status_file = directory.path().join("origin-status.jsonl");
        std::fs::write(
            &status_file,
            concat!(
                r#"{"state":"ready","origin":"https://studio.example.ts.net","tunnel":"tailscale","#,
                r#""routes":[{"kind":"relay","origin":"https://studio.relay.example"}]}"#
            ),
        )
        .unwrap();
        state.public_origin_file = Some(status_file.clone());
        state.home_routes =
            crate::lan::HomeRoutes::serving(vec!["https://192.168.1.20:3778".into()], pin);
        let (status, body) = crate::test_support::call(
            &state,
            "GET",
            "/api/v1/host/status",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        validate_http_contract("hostStatus", &body);
        validate_http_contract("hostStatus", &fixture);
        let keys = |value: &serde_json::Value| {
            let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(keys(&body), keys(&fixture));
        assert_eq!(body["routes"], fixture["routes"]);
        assert_eq!(body["hostKeyPin"], pin);

        // Home network off, an owner's own address: no home route and no pin;
        // the address is offered as the phone saves it at pairing.
        std::fs::write(
            &status_file,
            r#"{"state":"ready","origin":"https://wonder.example","tunnel":"none"}"#,
        )
        .unwrap();
        state.home_routes = crate::lan::HomeRoutes::default();
        let (_, body) = crate::test_support::call(
            &state,
            "GET",
            "/api/v1/host/status",
            serde_json::Value::Null,
        )
        .await;
        validate_http_contract("hostStatus", &body);
        assert_eq!(
            body["routes"],
            serde_json::json!([{"kind": "tailscale", "origin": "https://wonder.example"}])
        );
        assert!(body.get("hostKeyPin").is_none());

        // Host still starting: nothing to offer yet.
        std::fs::write(&status_file, r#"{"state":"starting"}"#).unwrap();
        let (_, body) = crate::test_support::call(
            &state,
            "GET",
            "/api/v1/host/status",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(body["routes"], serde_json::json!([]));
    }

    /// `pair --route home` gets a link on a home address that answers now, or
    /// the reason it cannot.
    #[tokio::test]
    async fn pairing_offer_on_the_home_network_uses_a_serving_home_address() {
        let (directory, mut state) = crate::test_support::fixture().await;
        let status_file = directory.path().join("origin-status.jsonl");
        std::fs::write(
            &status_file,
            r#"{"state":"ready","origin":"https://studio.example.ts.net"}"#,
        )
        .unwrap();
        state.public_origin_file = Some(status_file);
        let home = serde_json::json!({"route": "home"});
        let (status, body) =
            crate::test_support::call(&state, "POST", "/api/v1/pairing/offers", home.clone()).await;
        assert_eq!(
            status,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "{body}"
        );
        let pin = crate::lan::host_key(&state.data_dir).await.unwrap().pin();
        state.home_routes =
            crate::lan::HomeRoutes::serving(vec!["https://192.168.1.20:3778".into()], &pin);
        let (status, offer) =
            crate::test_support::call(&state, "POST", "/api/v1/pairing/offers", home).await;
        assert_eq!(status, axum::http::StatusCode::CREATED, "{offer}");
        let url = offer["url"].as_str().unwrap();
        assert!(url.starts_with("https://192.168.1.20:3778/pair#"), "{url}");
        assert!(url.contains(&format!("hostKey={pin}")), "{url}");
        let (status, _) = crate::test_support::call(
            &state,
            "POST",
            "/api/v1/pairing/offers",
            serde_json::json!({"route": "relay"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn saved_native_response_fixtures_validate() {
        for (definition, raw) in [
            (
                "conversationSnapshot",
                include_str!("../../../tests/contracts/native/conversation-snapshot.json"),
            ),
            (
                "messageReceipt",
                include_str!("../../../tests/contracts/native/message-receipt.json"),
            ),
        ] {
            validate_http_contract(definition, &serde_json::from_str(raw).unwrap());
        }
        let output: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/contracts/native/tool-output.json"
        ))
        .unwrap();
        validate_http_contract("toolOutput", &output["fullOutput"]);
        validate_http_contract("conversationThreadItem", &output["compactItem"]);
        validate_http_contract("compactToolOutput", &output["compactItem"]["payload"]);
    }

    #[test]
    fn thread_projection_groups_visible_and_operational_items_by_turn() {
        let messages = vec![StoredMessage {
            id: "message-1".into(),
            device_id: "device-1".into(),
            client_message_id: "client-1".into(),
            body: "Make a launch plan".into(),
            body_sha256: "hash".into(),
            conversation_id: "conversation-1".into(),
            state: "completed".into(),
            created_at: "2026-09-02T10:00:00Z".into(),
            codex_thread_id: Some("thread-1".into()),
            codex_turn_id: Some("turn-1".into()),
        }];
        let assistant_messages = vec![StoredAssistantMessage {
            id: "assistant-1".into(),
            conversation_id: "conversation-1".into(),
            codex_thread_id: "thread-1".into(),
            codex_turn_id: "turn-1".into(),
            item_id: "item-1".into(),
            text: "## Launch plan".into(),
            state: "completed".into(),
            created_at: "2026-09-02T10:00:01Z".into(),
            updated_at: "2026-09-02T10:00:02Z".into(),
        }];
        let events = vec![
            HostEventEnvelope {
                event_id: "event-1".into(),
                host_epoch: "epoch-1".into(),
                sequence: 1,
                occurred_at: "2026-09-02T10:00:01Z".into(),
                request_id: None,
                device_id: None,
                conversation_id: Some("conversation-1".into()),
                message_id: None,
                thread_id: Some("thread-1".into()),
                turn_id: Some("turn-1".into()),
                item_id: Some("command-1".into()),
                approval_id: None,
                event: WonderEvent::Activity {
                    category: "command".into(),
                    state: "completed".into(),
                    detail: Some("cargo test".into()),
                },
            },
            HostEventEnvelope {
                event_id: "event-1-replayed".into(),
                host_epoch: "epoch-1".into(),
                sequence: 2,
                occurred_at: "2026-09-02T10:00:02Z".into(),
                request_id: None,
                device_id: None,
                conversation_id: Some("conversation-1".into()),
                message_id: None,
                thread_id: Some("thread-1".into()),
                turn_id: Some("turn-1".into()),
                item_id: Some("command-1".into()),
                approval_id: None,
                event: WonderEvent::Activity {
                    category: "command".into(),
                    state: "started".into(),
                    detail: None,
                },
            },
        ];
        let projection = conversation_thread_projection(
            Some("thread-1".into()),
            &messages,
            &assistant_messages,
            &events,
        );
        assert_eq!(projection.turns.len(), 1);
        // Completed message/item receipts do not replace a turn lifecycle event.
        assert_eq!(projection.turns[0].status, "unknown");
        assert_eq!(
            projection.turns[0]
                .items
                .iter()
                .filter(|item| item.item_type == "userMessage")
                .count(),
            1
        );
        assert_eq!(
            projection.turns[0]
                .items
                .iter()
                .filter(|item| item.item_type == "agentMessage")
                .count(),
            1
        );
        assert_eq!(
            projection.turns[0]
                .items
                .iter()
                .filter(|item| item.item_type == "commandExecution")
                .count(),
            1
        );
        let command = projection.turns[0]
            .items
            .iter()
            .find(|item| item.item_type == "commandExecution")
            .unwrap();
        assert_eq!(command.state, "completed");
        assert_eq!(command.text.as_deref(), Some("cargo test"));
    }

    #[test]
    fn typed_thread_items_cover_new_protocol_variants_and_win_over_fallbacks() {
        let typed = vec![
            AppServerThreadItem {
                turn_id: "turn-typed".into(),
                item: serde_json::json!({
                    "type": "dynamicToolCall", "id": "item-typed", "tool": "demo",
                    "status": "started", "arguments": {"token": "sk-secret"}
                }),
            },
            AppServerThreadItem {
                turn_id: "turn-typed".into(),
                item: serde_json::json!({
                    "type": "enteredReviewMode", "id": "review-1", "review": "code"
                }),
            },
            AppServerThreadItem {
                turn_id: "turn-typed".into(),
                item: serde_json::json!({"type": "futureItem", "id": "future-1"}),
            },
        ];
        let events = vec![
            HostEventEnvelope {
                event_id: "event-fallback".into(),
                host_epoch: "epoch".into(),
                sequence: 1,
                occurred_at: "2026-09-02T10:00:00Z".into(),
                request_id: None,
                device_id: None,
                conversation_id: Some("conversation".into()),
                message_id: None,
                thread_id: Some("thread".into()),
                turn_id: Some("turn-typed".into()),
                item_id: Some("item-typed".into()),
                approval_id: None,
                event: WonderEvent::Activity {
                    category: "command".into(),
                    state: "started".into(),
                    detail: Some("fallback".into()),
                },
            },
            HostEventEnvelope {
                event_id: "event-late-terminal".into(),
                host_epoch: "epoch".into(),
                sequence: 2,
                occurred_at: "2026-09-02T10:00:02Z".into(),
                request_id: None,
                device_id: None,
                conversation_id: Some("conversation".into()),
                message_id: None,
                thread_id: Some("thread".into()),
                turn_id: Some("turn-typed".into()),
                item_id: Some("item-typed".into()),
                approval_id: None,
                event: WonderEvent::Activity {
                    category: "command".into(),
                    state: "failed".into(),
                    detail: Some("late contradictory terminal".into()),
                },
            },
        ];
        let projection = conversation_thread_projection_with_items(
            Some("thread".into()),
            &[],
            &[StoredAssistantMessage {
                id: "assistant-typed".into(),
                conversation_id: "conversation".into(),
                codex_thread_id: "thread".into(),
                codex_turn_id: "turn-typed".into(),
                item_id: "item-typed".into(),
                text: "durable terminal fallback".into(),
                state: "completed".into(),
                created_at: "2026-09-02T10:00:00Z".into(),
                updated_at: "2026-09-02T10:00:01Z".into(),
            }],
            &events,
            &typed,
            None,
        );
        let items = &projection.turns[0].items;
        assert!(items.iter().any(|item| item.item_type == "dynamicToolCall"));
        assert!(items
            .iter()
            .any(|item| item.item_type == "enteredReviewMode"));
        assert!(items.iter().any(|item| item.item_type == "unknown"));
        let dynamic = items.iter().find(|item| item.id == "item-typed").unwrap();
        assert_eq!(dynamic.item_type, "dynamicToolCall");
        assert_eq!(dynamic.state, "completed");
        assert!(!dynamic.payload.to_string().contains("sk-secret"));
    }

    #[test]
    fn durable_messages_without_codex_turns_remain_visible() {
        let messages = vec![StoredMessage {
            id: "message-local".into(),
            device_id: "device-1".into(),
            client_message_id: "client-local".into(),
            body: "Queued while offline".into(),
            body_sha256: "hash".into(),
            conversation_id: "conversation-1".into(),
            state: "pending".into(),
            created_at: "2026-09-02T10:00:00Z".into(),
            codex_thread_id: None,
            codex_turn_id: None,
        }];
        let projection = conversation_thread_projection(None, &messages, &[], &[]);
        assert_eq!(projection.turns.len(), 1);
        assert_eq!(projection.turns[0].id, "local:message-local");
        assert_eq!(
            projection.turns[0].items[0].text.as_deref(),
            Some("Queued while offline")
        );
    }

    #[test]
    fn typed_and_durable_user_messages_dedupe_by_client_id() {
        let messages = vec![StoredMessage {
            id: "database-message-id".into(),
            device_id: "device-1".into(),
            client_message_id: "shared-client-id".into(),
            body: "Hydrate this once".into(),
            body_sha256: "hash".into(),
            conversation_id: "conversation-1".into(),
            state: "completed".into(),
            created_at: "2026-09-02T10:00:00Z".into(),
            codex_thread_id: Some("thread-1".into()),
            codex_turn_id: Some("turn-1".into()),
        }];
        let typed = vec![AppServerThreadItem {
            turn_id: "turn-1".into(),
            item: serde_json::json!({
                "type": "userMessage",
                "id": "codex-item-id",
                "clientId": "shared-client-id",
                "content": [{"type": "input_text", "text": "Hydrate this once"}]
            }),
        }];
        let projection = conversation_thread_projection_with_items(
            Some("thread-1".into()),
            &messages,
            &[],
            &[],
            &typed,
            Some("/workspace"),
        );
        let user_items = projection.turns[0]
            .items
            .iter()
            .filter(|item| item.item_type == "userMessage")
            .collect::<Vec<_>>();
        assert_eq!(user_items.len(), 1);
        assert_eq!(user_items[0].id, "codex-item-id");
        assert_eq!(user_items[0].payload["clientId"], "shared-client-id");
        assert_eq!(user_items[0].payload["deliveryState"], "completed");
    }

    #[test]
    fn typed_commentary_and_mixed_output_preserve_phase_and_order() {
        let typed = vec![
            AppServerThreadItem {
                turn_id: "turn".into(),
                item: serde_json::json!({"type":"agentMessage","id":"z-commentary","phase":"commentary","text":"Checking the files."}),
            },
            // Codex 0.162.0-alpha.17 labels answer text that tools may follow.
            AppServerThreadItem {
                turn_id: "turn".into(),
                item: serde_json::json!({"type":"agentMessage","id":"p-partial","phase":"partial_answer","text":"The first file is fine."}),
            },
            AppServerThreadItem {
                turn_id: "turn".into(),
                item: serde_json::json!({"type":"dynamicToolCall","id":"m-tool","tool":"inspect","status":"completed","success":true,"arguments":{},"contentItems":[{"type":"inputText","text":"Found a chart."},{"type":"inputImage","imageUrl":"data:image/png;base64,synthetic"}]}),
            },
            AppServerThreadItem {
                turn_id: "turn".into(),
                item: serde_json::json!({"type":"agentMessage","id":"a-final","phase":"final_answer","text":"The check passed."}),
            },
        ];
        let projection = conversation_thread_projection_with_items(
            Some("thread".into()),
            &[],
            &[],
            &[],
            &typed,
            Some("/workspace"),
        );
        let items = &projection.turns[0].items;
        assert_eq!(
            items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["z-commentary", "p-partial", "m-tool", "a-final"]
        );
        assert_eq!(items[0].payload["phase"], "commentary");
        assert_eq!(items[0].text.as_deref(), Some("Checking the files."));
        assert_eq!(items[1].payload["phase"], "partial_answer");
        assert_eq!(items[1].text.as_deref(), Some("The first file is fine."));
        assert_eq!(
            items[2].payload["contentItems"][0]["text"],
            "Found a chart."
        );
        assert_eq!(items[3].payload["phase"], "final_answer");
    }

    /// A newer Codex passes startup verification with labels Wonder has not
    /// seen (schema_compat accepts additive response changes), so ingestion
    /// must keep them as opaque values instead of dropping the turn.
    #[test]
    fn labels_from_a_newer_codex_degrade_to_opaque_items() {
        let typed = vec![
            AppServerThreadItem {
                turn_id: "turn".into(),
                item: serde_json::json!({"type":"agentMessage","id":"a-draft","phase":"future_phase","text":"Drafting."}),
            },
            AppServerThreadItem {
                turn_id: "turn".into(),
                item: serde_json::json!({"type":"futureItem","id":"b-future","status":"futureStatus","text":"New kind of work"}),
            },
            AppServerThreadItem {
                turn_id: "turn".into(),
                item: serde_json::json!({"type":"commandExecution","id":"c-command","command":"ls","status":{"type":"futureStatus"}}),
            },
        ];
        let projection = conversation_thread_projection_with_items(
            Some("thread".into()),
            &[],
            &[],
            &[],
            &typed,
            None,
        );
        let items = &projection.turns[0].items;
        assert_eq!(
            items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["a-draft", "b-future", "c-command"]
        );
        assert_eq!(items[0].item_type, "agentMessage");
        assert_eq!(items[0].payload["phase"], "future_phase");
        assert_eq!(items[0].text.as_deref(), Some("Drafting."));
        assert_eq!(items[1].item_type, "unknown");
        assert_eq!(items[1].payload["originalType"], "futureItem");
        assert_eq!(items[1].state, "unknown");
        assert_eq!(items[2].item_type, "commandExecution");
        assert_eq!(items[2].state, "unknown");
        assert!(project_app_server_notification(
            "item/completed",
            &serde_json::json!({"item":{"type":"futureItem","id":"b-future"}}),
            None
        )
        .is_none());
        assert_eq!(
            super::turn_completion_delivery_state(
                &serde_json::json!({"turn":{"id":"turn","status":"futureStatus"}})
            ),
            super::DeliveryState::Uncertain
        );
        for (status, delivery) in [
            ("completed", super::DeliveryState::Completed),
            ("failed", super::DeliveryState::Failed),
            ("interrupted", super::DeliveryState::Interrupted),
        ] {
            assert_eq!(
                super::turn_completion_delivery_state(
                    &serde_json::json!({"turn":{"id":"turn","status":status}})
                ),
                delivery
            );
        }
    }

    /// Startup verification protects only the notifications listed in
    /// `wonder_app_server::CONSUMED_NOTIFICATIONS`. Reading another Codex
    /// notification here without listing it would leave its contract unchecked.
    #[test]
    fn codex_notification_inventory_is_complete() {
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../research/codex-app-server/0.162.0-alpha.17.2/experimental/codex_app_server_protocol.v2.schemas.json"
        ))
        .unwrap();
        let notifications: Vec<&str> = schema["definitions"]["ServerNotification"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|variant| variant["properties"]["method"]["enum"][0].as_str())
            .filter(|method| method.contains('/'))
            .collect();
        let mut missing = std::collections::BTreeSet::new();
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut directories = vec![source];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for method in &notifications {
                        if text.contains(&format!("\"{method}\""))
                            && !wonder_app_server::CONSUMED_NOTIFICATIONS
                                .iter()
                                .any(|(consumed, _)| consumed == method)
                        {
                            missing.insert(*method);
                        }
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "wonderd reads {missing:?}; add them to wonder_app_server::CONSUMED_NOTIFICATIONS"
        );
    }

    #[test]
    fn typed_only_streaming_items_do_not_establish_turn_status() {
        let typed = vec![AppServerThreadItem {
            turn_id: "turn-active".into(),
            item: serde_json::json!({
                "type": "commandExecution",
                "id": "command-active",
                "status": "running",
                "command": "cargo test"
            }),
        }];
        let projection = conversation_thread_projection_with_items(
            Some("thread-1".into()),
            &[],
            &[],
            &[],
            &typed,
            Some("/workspace"),
        );
        assert_eq!(projection.turns[0].status, "unknown");
        assert_eq!(projection.turns[0].items[0].state, "streaming");
    }

    #[test]
    fn terminal_projection_strips_ansi_redacts_secrets_and_bounds_output() {
        let value = format!(
            "\u{1b}[31mBearer secret-token\u{1b}[0m {}",
            "x".repeat(70_000)
        );
        let clean = sanitize_ansi_and_secrets(&value, 64 * 1024);
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains("secret-token"));
        assert!(clean.len() <= 64 * 1024);
    }

    #[test]
    fn typed_projection_keeps_only_safe_bounded_command_detail() {
        let safe = sanitize_typed_item(
            &serde_json::json!({
                "type": "commandExecution",
                "id": "command-1",
                "command": "curl -H 'Authorization: Bearer secret-token' https://example.invalid",
                "cwd": "/workspace",
                "environment": {"API_KEY": "secret"},
                "headers": {"cookie": "secret"},
                "aggregatedOutput": format!("\u{1b}[31mtoken=secret\u{1b}[0m {}", "x".repeat(70_000)),
                "commandActions": [{"type": "read", "path": "/etc/passwd"}],
                "savedPath": "/workspace/output/report.pdf",
                "agentPath": "/Users/private/agent.json",
                "nested": {"filePath": "/workspace/src/main.rs"},
                "paths": ["/workspace/a.txt", "/private/b.txt"],
                "processId": 42
            }),
            Some("/workspace"),
        );
        let encoded = safe.to_string();
        assert_eq!(safe["cwd"], ".");
        assert!(safe.get("environment").is_none());
        assert!(safe.get("headers").is_none());
        assert_eq!(
            safe["commandActions"],
            serde_json::json!([{ "type": "read", "path": "[path outside workspace]" }])
        );
        assert!(!encoded.contains("/etc/passwd"));
        assert!(safe.get("processId").is_none());
        assert_eq!(safe["savedPath"], "output/report.pdf");
        assert_eq!(safe["agentPath"], "[path outside workspace]");
        assert_eq!(safe["nested"]["filePath"], "src/main.rs");
        assert_eq!(safe["paths"][0], "a.txt");
        assert_eq!(safe["paths"][1], "[path outside workspace]");
        assert!(!encoded.contains("secret-token"));
        assert!(!encoded.contains("token=secret"));
        assert!(safe["output"]
            .as_str()
            .is_some_and(|value| value.len() <= 64 * 1024));
    }

    #[test]
    fn same_host_get_without_origin_is_allowed_for_native_api() {
        let request = axum::http::Request::builder()
            .method("GET")
            .header("host", "wonder.example")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(matches_public_origin(&request, "https://wonder.example"));

        let wrong_host = axum::http::Request::builder()
            .method("GET")
            .header("host", "127.0.0.1:3777")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!matches_public_origin(
            &wrong_host,
            "https://wonder.example"
        ));

        let post = axum::http::Request::builder()
            .method("POST")
            .header("host", "wonder.example")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!matches_public_origin(&post, "https://wonder.example"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn browser_pairing_link_gets_one_line_and_native_api_stays_authenticated() {
        use axum::{
            body::{to_bytes, Body},
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;

        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let router = crate::router(state.clone());
        for (path, expected) in [
            ("/pair", StatusCode::OK),
            ("/", StatusCode::NOT_FOUND),
            ("/pair/style.css", StatusCode::NOT_FOUND),
            ("/sw.js", StatusCode::NOT_FOUND),
            ("/manifest.webmanifest", StatusCode::NOT_FOUND),
            ("/precache-manifest.json", StatusCode::NOT_FOUND),
            ("/assets/index.js", StatusCode::NOT_FOUND),
            ("/index.html", StatusCode::NOT_FOUND),
            ("/projects", StatusCode::NOT_FOUND),
        ] {
            let response = router
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert!(response.headers().contains_key("content-security-policy"));
            if path == "/pair" {
                assert!(response.headers()["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("text/plain"));
                let body = to_bytes(response.into_body(), 65536).await.unwrap();
                let text = std::str::from_utf8(&body).unwrap();
                assert!(text.contains("Open Cupola on your iPhone or iPad"));
                assert!(!text.contains('<'));
            }
        }
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projects")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!response.status().is_success());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projects")
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[test]
    fn api_route_auth_policy_keeps_pairing_public_and_everything_else_protected() {
        assert_eq!(route_auth("/healthz"), RouteAuth::Public);
        assert_eq!(route_auth("/pair"), RouteAuth::Public);
        assert_eq!(route_auth("/"), RouteAuth::NotFound);
        assert_eq!(route_auth("/pair/style.css"), RouteAuth::NotFound);
        assert_eq!(route_auth("/assets/index.js"), RouteAuth::NotFound);
        assert_eq!(route_auth("/manifest.webmanifest"), RouteAuth::NotFound);
        assert_eq!(
            route_auth("/api/v1/pairing/claim"),
            RouteAuth::PublicPairing
        );
        assert_eq!(route_auth("/api/v1/pairing/code"), RouteAuth::PublicPairing);
        assert_eq!(
            route_auth("/api/v1/pairing/session"),
            RouteAuth::PublicPairing
        );
        assert_eq!(
            route_auth("/api/v1/pairing/session/refresh-challenge"),
            RouteAuth::PublicPairing
        );
        assert_eq!(
            route_auth("/api/v1/conversations/default/messages"),
            RouteAuth::ProtectedApi
        );
        assert_eq!(
            route_auth("/api/v1/conversations/default/turns/turn-1/steer"),
            RouteAuth::ProtectedApi
        );
        assert_eq!(
            route_auth("/api/v1/search?q=launch"),
            RouteAuth::ProtectedApi
        );
        assert_eq!(route_auth("/api/v1/devices"), RouteAuth::ProtectedApi);
        assert_eq!(
            route_auth("/api/v1/devices/device-1"),
            RouteAuth::ProtectedApi
        );
    }

    // Contract: the Mac app's Agents settings read and change the same host
    // values as the phone. Its bridge holds only the loopback capability, so
    // each settings route answers that capability and refuses a request with
    // a wrong capability or no credentials at all.
    #[tokio::test]
    async fn agent_settings_routes_accept_only_the_loopback_capability() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;
        let (_dir, state) = crate::test_support::fixture().await;
        let skill = format!(
            "/api/v1/settings/wonder-skills/{}",
            crate::wonder_skills::SKILLS[0].id
        );
        let routes = [
            ("GET", "/api/v1/settings/wonder-skills", ""),
            ("PUT", skill.as_str(), r#"{"enabled":false}"#),
            ("GET", "/api/v1/settings/thread-tools", ""),
            (
                "PUT",
                "/api/v1/settings/thread-tools/codex",
                r#"{"enabled":true}"#,
            ),
            ("GET", "/api/v1/settings/chat-detail", ""),
            (
                "PUT",
                "/api/v1/settings/chat-detail/claude",
                r#"{"detail":"everything"}"#,
            ),
            ("GET", "/api/v1/host/name", ""),
            ("PUT", "/api/v1/host/name", r#"{"name":null}"#),
            ("GET", "/api/v1/settings/custom-instructions", ""),
            (
                "PUT",
                "/api/v1/settings/custom-instructions/codex",
                r#"{"enabled":false,"text":""}"#,
            ),
        ];
        for (method, path, body) in routes {
            for capability in [None, Some("not-the-capability")] {
                let mut request = Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json");
                if let Some(capability) = capability {
                    request = request.header("x-wonder-loopback-capability", capability);
                }
                let response = router(state.clone())
                    .oneshot(request.body(Body::from(body)).unwrap())
                    .await
                    .unwrap();
                assert!(
                    matches!(
                        response.status(),
                        StatusCode::UNAUTHORIZED
                            | StatusCode::FORBIDDEN
                            | StatusCode::SERVICE_UNAVAILABLE
                    ),
                    "{method} {path} with {capability:?}: {}",
                    response.status()
                );
            }
            let response = router(state.clone())
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("x-wonder-loopback-capability", &state.loopback_capability)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
        }
    }

    #[test]
    fn public_origin_must_be_https_and_non_loopback() {
        assert!(is_valid_public_origin("https://wonder.example.ts.net"));
        assert!(!is_valid_public_origin("http://wonder.example.ts.net"));
        assert!(!is_valid_public_origin("https://127.0.0.1:3777"));
        assert!(!is_valid_public_origin("https://wonder.invalid"));
        assert!(!is_valid_public_origin("https://wonder.example/path"));
    }

    #[test]
    fn host_readiness_is_degraded_until_public_tunnel_is_available() {
        assert_eq!(host_readiness_state(None), "degraded");
        assert_eq!(
            host_readiness_state(Some("https://wonder.example.ts.net")),
            "ready"
        );
    }

    #[test]
    fn public_origin_status_log_requires_the_newest_status_to_be_ready() {
        let origins = |log: &str| -> Vec<String> {
            trusted_origins_from_status_log(log)
                .into_iter()
                .map(|trusted| trusted.origin)
                .collect()
        };
        assert!(origins(
            r#"{"state":"ready","origin":"https://wonder.example.ts.net"}
{"state":"stopped"}"#
        )
        .is_empty());
        assert_eq!(
            origins(
                r#"{"state":"ready","origin":"https://wonder.example.ts.net"}
{"state":"error","error":"funnel unavailable"}
{"state":"ready","origin":"https://wonder.example.ts.net"}"#
            ),
            ["https://wonder.example.ts.net"]
        );
    }

    /// The pairing routes take requests from anyone who can reach the host, so
    /// they read small bodies only; their real payloads are a few hundred bytes.
    #[cfg(unix)]
    #[tokio::test]
    async fn unauthenticated_pairing_routes_refuse_large_bodies() {
        use crate::{router, MAX_PUBLIC_PAIRING_BODY_BYTES};
        use axum::{
            body::Body,
            http::{Method, Request, StatusCode},
        };
        use tower::ServiceExt;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        for path in [
            "/api/v1/pairing/claim",
            "/api/v1/pairing/code",
            "/api/v1/pairing/session",
            "/api/v1/pairing/session/refresh-challenge",
        ] {
            let padding = "x".repeat(MAX_PUBLIC_PAIRING_BODY_BYTES);
            let response = router(state.clone())
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .header("origin", &state.public_origin)
                        .header("content-type", "application/json")
                        .body(Body::from(format!(r#"{{"padding":"{padding}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{path}");
        }
    }

    #[test]
    fn status_log_routes_add_trusted_origins_after_the_primary() {
        let trusted = trusted_origins_from_status_log(
            r#"{"state":"ready","origin":"https://wonder.example.ts.net","routes":[
                {"kind":"home","origin":"https://192.168.1.20:3778"},
                {"kind":"tailscale","origin":"https://wonder.example.ts.net"},
                {"kind":"relay","origin":"https://a1b2.relay.example"},
                {"kind":"carrier-pigeon","origin":"https://unknown.example"},
                {"kind":"home","origin":"http://192.168.1.21:3778"},
                {"kind":"home","origin":"https://127.0.0.1:3778"},
                {"origin":"https://kindless.example"}
            ]}"#
            .replace('\n', "")
            .as_str(),
        );
        let found: Vec<(OriginKind, &str)> = trusted
            .iter()
            .map(|trusted| (trusted.kind, trusted.origin.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                (OriginKind::Tailscale, "https://wonder.example.ts.net"),
                (OriginKind::Home, "https://192.168.1.20:3778"),
                (OriginKind::Relay, "https://a1b2.relay.example"),
            ]
        );
        assert_eq!(
            trusted_origins_from_status_log(
                r#"{"state":"ready","origin":"https://wonder.example"}"#
            )[0]
            .kind,
            OriginKind::Custom
        );
    }

    // Axum must refuse every incomplete caller context in user language,
    // including a device extension without the origin extension (and vice versa).
    #[tokio::test]
    async fn event_challenge_refuses_each_missing_caller_extension() {
        use super::{event_challenge, AuthenticatedDevice, MatchedOrigin};
        use axum::{body::Body, http::StatusCode, Extension};
        use tower::ServiceExt as _;
        let (_directory, state) = crate::ingestion::tests::fixture().await;
        for (device, origin) in [(false, false), (true, false), (false, true)] {
            let mut app = axum::Router::new()
                .route("/challenge", axum::routing::get(event_challenge))
                .with_state(state.clone());
            if device {
                app = app.layer(Extension(AuthenticatedDevice {
                    device_id: "phone".into(),
                    session_binding: "session".into(),
                }));
            }
            if origin {
                app = app.layer(Extension(MatchedOrigin("https://host.example".into())));
            }
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/challenge")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "device={device}, origin={origin}"
            );
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            assert_eq!(
                &body[..],
                b"Only a paired device signs in with a challenge."
            );
        }
    }

    #[tokio::test]
    async fn pairing_offer_response_requires_a_ready_public_origin() {
        let mut pairing = super::HostPairingState::default();
        let response = pairing_offer_response(None, "test-installation", "pin", &mut pairing);
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("pairing readiness body");
        assert_eq!(
            &body[..],
            b"This computer's connection address isn't ready yet. Check `cupola status`."
        );

        let response = pairing_offer_response(
            Some("https://wonder.example.ts.net".into()),
            "test-installation",
            "pin",
            &mut pairing,
        );
        assert_eq!(response.status(), axum::http::StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("pairing offer body");
        let offer: serde_json::Value = serde_json::from_slice(&body).expect("pairing offer JSON");
        assert!(offer["url"]
            .as_str()
            .is_some_and(|url| url.starts_with("https://wonder.example.ts.net/pair#")));
    }

    #[test]
    fn host_proofs_are_bounded_per_route_origin() {
        let mut pairing = super::HostPairingState::default();
        let home = "https://192.168.1.20:3778";
        let tailnet = "https://wonder.example.ts.net";
        for _ in 0..super::HOST_PROOF_LIMIT {
            assert!(pairing.allow_host_proof(home, 1_000));
        }
        assert!(!pairing.allow_host_proof(home, 1_000));
        // A flood on the home network leaves the other route alone.
        assert!(pairing.allow_host_proof(tailnet, 1_000));
        assert!(pairing.allow_host_proof(home, 1_000 + super::HOST_PROOF_WINDOW_MS));
    }

    #[test]
    fn pairing_defaults_to_a_bounded_session() {
        assert_eq!(default_session_expiration(), "1d");
    }

    #[test]
    fn device_labels_are_bounded_and_human_readable() {
        assert!(valid_device_label("Phone"));
        assert!(valid_device_label("  My iPhone  "));
        assert!(!valid_device_label(""));
        assert!(!valid_device_label("\n"));
        assert!(!valid_device_label(&"x".repeat(161)));
    }

    #[test]
    fn app_server_projection_preserves_deltas_and_nested_completion_text() {
        assert_eq!(
            project_app_server_notification("skills/changed", &serde_json::json!({}), None),
            Some(wonder_api::WonderEvent::Activity {
                category: "codex_skills".into(),
                state: "changed".into(),
                detail: None
            })
        );

        assert_eq!(
            project_app_server_notification(
                "item/agentMessage/delta",
                &serde_json::json!({"delta": {"text": "partial"}}),
                None,
            ),
            Some(wonder_api::WonderEvent::AssistantDelta {
                text: "partial".into()
            })
        );
        assert_eq!(
            project_app_server_notification(
                "item/commandExecution/outputDelta",
                &serde_json::json!({"itemId": "command-1", "delta": "secret output"}),
                None,
            ),
            Some(wonder_api::WonderEvent::Activity {
                category: "command".into(),
                state: "working".into(),
                detail: Some("Output updated".into()),
            })
        );
        assert_eq!(
            project_app_server_notification(
                "item/completed",
                &serde_json::json!({"item": {"content": [{"text": "done"}]}}),
                None,
            ),
            Some(wonder_api::WonderEvent::AssistantCompleted {
                text: "done".into()
            })
        );
        assert_eq!(
            project_app_server_notification(
                "item/completed",
                &serde_json::json!({"item": {"type": "agentMessage", "content": []}}),
                None,
            ),
            Some(wonder_api::WonderEvent::AssistantCompleted {
                text: String::new()
            })
        );
        assert_eq!(
            project_app_server_notification(
                "item/started",
                &serde_json::json!({
                    "item": {"type": "commandExecution", "id": "command-1", "command": "rg hello"}
                }),
                None,
            ),
            Some(wonder_api::WonderEvent::Activity {
                category: "command".into(),
                state: "running".into(),
                detail: Some("rg hello".into()),
            })
        );
        assert_eq!(
            project_app_server_notification(
                "item/completed",
                &serde_json::json!({
                    "item": {"type": "fileChange", "id": "file-1", "changes": [{"path": "src/lib.rs"}]}
                }),
                None,
            ),
            Some(wonder_api::WonderEvent::Activity {
                category: "file change".into(),
                state: "completed".into(),
                detail: Some("src/lib.rs".into()),
            })
        );
        assert_eq!(
            project_app_server_notification(
                "item/commandExecution/outputDelta",
                &serde_json::json!({"delta": "secret output should not be persisted"}),
                None,
            ),
            Some(wonder_api::WonderEvent::Activity {
                category: "command".into(),
                state: "working".into(),
                detail: Some("Output updated".into()),
            })
        );
        assert_eq!(
            project_app_server_notification(
                "item/fileChange/outputDelta",
                &serde_json::json!({"delta": "file tool output"}),
                None,
            ),
            Some(wonder_api::WonderEvent::Activity {
                category: "file change".into(),
                state: "working".into(),
                detail: Some("Output updated".into()),
            })
        );
        assert_eq!(
            project_app_server_notification(
                "turn/completed",
                &serde_json::json!({"turn": {"status": "completed"}}),
                None,
            ),
            Some(wonder_api::WonderEvent::MessageState {
                state: wonder_api::DeliveryState::Completed,
            })
        );
        assert_eq!(
            project_app_server_notification("turn/completed", &serde_json::json!({}), None,),
            Some(wonder_api::WonderEvent::MessageState {
                state: wonder_api::DeliveryState::Uncertain,
            })
        );
        assert_eq!(
            project_app_server_notification(
                "future/notification",
                &serde_json::json!({"secret": "redact-me"}),
                None,
            ),
            Some(wonder_api::WonderEvent::Activity {
                category: "App Server".into(),
                state: "unhandled".into(),
                detail: Some("future/notification".into()),
            })
        );
        assert_eq!(
            project_app_server_notification(
                "item/mcpToolCall/progress",
                &serde_json::json!({}),
                None,
            ),
            Some(wonder_api::WonderEvent::Activity {
                category: "MCP tool".into(),
                state: "working".into(),
                detail: None,
            })
        );
    }

    #[test]
    fn item_completion_keeps_explicit_failed_and_interrupted_lifecycle() {
        for (item_type, raw_state, expected) in [
            ("commandExecution", "failed", "failed"),
            ("mcpToolCall", "interrupted", "interrupted"),
        ] {
            let item = serde_json::json!({
                "id": format!("{item_type}-1"),
                "type": item_type,
                "status": raw_state,
            });
            let detail = super::thread_item_upsert_detail_with_authority(
                "turn",
                &item,
                super::app_server_item_lifecycle_state("item/completed", &item),
                2,
                None,
            )
            .expect("item identity");
            let detail: serde_json::Value = serde_json::from_str(&detail).expect("detail JSON");
            assert_eq!(detail["state"], expected);
        }
    }

    #[test]
    fn search_result_deep_links_preserve_kind_and_stable_focus() {
        let message = wonder_store::StoredSearchResult {
            kind: "message".into(),
            id: "message-1".into(),
            title: "Chat".into(),
            snippet: Some("hello".into()),
            conversation_id: Some("conversation-1".into()),
            updated_at: "now".into(),
        };
        assert_eq!(
            search_result_deep_link(&message),
            "#/chats/conversation-1?focus=message-1&kind=message"
        );
    }

    #[test]
    fn delta_identity_uses_only_authoritative_app_server_sequence() {
        let without_sequence = serde_json::json!({
            "method": "item/agentMessage/delta",
            "params": {"itemId": "item-1", "delta": {"text": "chunk"}}
        });
        assert!(app_server_notification_key(
            "item/agentMessage/delta",
            &without_sequence,
            without_sequence.get("params").expect("params")
        )
        .is_none());

        let with_sequence = serde_json::json!({
            "method": "item/agentMessage/delta",
            "params": {"itemId": "item-1", "sequence": 2, "delta": {"text": "chunk"}}
        });
        assert!(app_server_notification_key(
            "item/agentMessage/delta",
            &with_sequence,
            with_sequence.get("params").expect("params")
        )
        .is_some());

        let patch_updated = serde_json::json!({
            "method": "item/fileChange/patchUpdated",
            "params": {
                "itemId": "file-item-1",
                "changes": [{"path": "report.md", "kind": {"type": "add"}, "diff": "+# report"}]
            }
        });
        let patch_params = patch_updated.get("params").expect("params");
        let patch_key = app_server_notification_key(
            "item/fileChange/patchUpdated",
            &patch_updated,
            patch_params,
        )
        .expect("patch update key");
        let different_patch = serde_json::json!({
            "method": "item/fileChange/patchUpdated",
            "params": {
                "itemId": "file-item-1",
                "changes": [{"path": "report.md", "kind": {"type": "add"}, "diff": "+different"}]
            }
        });
        let different_key = app_server_notification_key(
            "item/fileChange/patchUpdated",
            &different_patch,
            different_patch.get("params").expect("params"),
        )
        .expect("different patch update key");
        assert_ne!(patch_key, different_key);
    }

    #[test]
    fn approval_body_canonicalization_is_ordered_and_json_independent() {
        let request = ApprovalResolution {
            decision: "accept".into(),
            action_nonce: "nonce-1".into(),
            expected_state: "pending".into(),
            idempotency_key: "request-1".into(),
            issued_at_ms: now_ms(),
            signature: "ignored".into(),
            response_json: None,
        };
        assert_eq!(
            canonical_approval_body(&request),
            "[\"accept\",\"nonce-1\",\"pending\",\"request-1\",\"\"]"
        );
    }

    fn resolution(decision: &str, response_json: &str) -> ApprovalResolution {
        ApprovalResolution {
            decision: decision.into(),
            action_nonce: "nonce-1".into(),
            expected_state: "pending".into(),
            idempotency_key: "request-1".into(),
            issued_at_ms: now_ms(),
            signature: "ignored".into(),
            response_json: Some(response_json.into()),
        }
    }

    #[test]
    fn typed_approval_responses_are_validated_before_forwarding() {
        let structured = ApprovalResolution {
            decision: serde_json::json!({"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["*.md"]}}),
            action_nonce: "nonce-1".into(),
            expected_state: "pending".into(),
            idempotency_key: "request-1".into(),
            issued_at_ms: now_ms(),
            signature: "ignored".into(),
            response_json: Some(r#"{"decision":{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["*.md"]}}}"#.into()),
        };
        assert!(build_approval_response(
            "item/commandExecution/requestApproval",
            r#"{"availableDecisions":[{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["*.md"]}}],"threadId":"thread-1","turnId":"turn-1"}"#,
            &structured,
            None,
        ).is_ok());

        let permissions = resolution(
            "accept",
            r#"{"permissions":{"fileSystem":{"read":["/tmp/approved"]}},"scope":"turn"}"#,
        );
        assert!(build_approval_response(
            "item/permissions/requestApproval",
            r#"{"permissions":{"fileSystem":{"read":["/tmp/approved"],"write":["/tmp/approved"]}}}"#,
            &permissions,
            None,
        )
        .is_ok());

        let overbroad = resolution(
            "accept",
            r#"{"permissions":{"fileSystem":{"write":["/tmp/not-requested"]}},"scope":"turn"}"#,
        );
        assert!(build_approval_response(
            "item/permissions/requestApproval",
            r#"{"permissions":{"fileSystem":{"write":["/tmp/approved"]}}}"#,
            &overbroad,
            None,
        )
        .is_err());

        let answers = resolution(
            "submit",
            r#"{"answers":{"question-1":{"answers":["yes"]}}}"#,
        );
        assert!(build_approval_response(
            "item/tool/requestUserInput",
            r#"{"questions":[{"id":"question-1","header":"Confirm","question":"Proceed?"}]}"#,
            &answers,
            None,
        )
        .is_ok());

        let elicitation = resolution("accept", r#"{"action":"accept","content":{"ok":true}}"#);
        assert!(build_approval_response(
            "mcpServer/elicitation/request",
            r#"{"serverName":"recorded","threadId":"thread-1"}"#,
            &elicitation,
            None,
        )
        .is_ok());
        let mismatched_elicitation = resolution("decline", r#"{"action":"accept"}"#);
        assert!(build_approval_response(
            "mcpServer/elicitation/request",
            r#"{"serverName":"recorded","threadId":"thread-1"}"#,
            &mismatched_elicitation,
            None,
        )
        .is_err());
    }

    #[test]
    fn unregistered_dynamic_tools_can_only_be_declined() {
        let response = resolution("respond", r#"{"success":false,"contentItems":[]}"#);
        assert!(build_approval_response(
            "item/tool/call",
            r#"{"tool":"unregistered","callId":"call-1","threadId":"thread-1","turnId":"turn-1"}"#,
            &response,
            None,
        )
        .is_ok());
        let success = resolution("respond", r#"{"success":true,"contentItems":[]}"#);
        assert!(build_approval_response(
            "item/tool/call",
            r#"{"tool":"unregistered","callId":"call-1","threadId":"thread-1","turnId":"turn-1"}"#,
            &success,
            None,
        )
        .is_err());
        let wrong_decision = resolution("accept", r#"{"success":false,"contentItems":[]}"#);
        assert!(build_approval_response(
            "item/tool/call",
            r#"{"tool":"unregistered","callId":"call-1","threadId":"thread-1","turnId":"turn-1"}"#,
            &wrong_decision,
            None,
        )
        .is_err());
    }

    #[test]
    fn reconciliation_matches_only_the_exact_user_client_id() {
        let items = serde_json::json!({
            "data": [
                { "turnId": "turn-other", "item": { "type": "userMessage", "id": "item-1", "clientId": "other" } },
                { "turnId": "turn-target", "item": { "type": "userMessage", "id": "item-2", "clientId": "target" } },
                { "turnId": "turn-agent", "item": { "type": "agentMessage", "id": "item-3", "clientId": "target" } }
            ]
        });
        assert_eq!(
            find_client_message(&items, "target"),
            Some("turn-target".to_owned())
        );
        assert_eq!(find_client_message(&items, "missing"), None);
    }

    #[test]
    fn extracts_assistant_text_from_supported_item_shapes() {
        assert_eq!(
            extract_text(&serde_json::json!({"text": "direct"})),
            Some("direct".into())
        );
        assert_eq!(
            extract_text(&serde_json::json!({
                "content": [
                    {"type": "output_text", "text": "first"},
                    {"type": "output_text", "text": " second"}
                ]
            })),
            Some("first second".into())
        );
        assert_eq!(
            extract_text(&serde_json::json!({"message": {"output": [{"text": "nested"}]}})),
            Some("nested".into())
        );
        assert_eq!(extract_text(&serde_json::json!({"text": ""})), None);
    }

    #[tokio::test]
    async fn local_http_send_dispatches_through_fake_app_server_and_reloads_assistant_projection() {
        use axum::{
            body::{to_bytes, Body},
            http::{Method, Request},
        };
        use sha2::Digest;
        use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};
        use tokio::{sync::RwLock, time::Duration};
        use tower::ServiceExt;

        let directory = tempfile::tempdir().expect("temporary contract directory");
        let workspace = directory
            .path()
            .join("d19b0f7e-97bb-45d7-8206-324a62386dd9");
        fs::create_dir_all(&workspace).expect("project workspace");
        let workspace =
            crate::canonical_path::canonicalize(workspace).expect("resolved project workspace");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("wonder.sqlite3").display()
        );
        let store = wonder_store::Store::connect(&database_url)
            .await
            .expect("store");
        crate::test_support::project_thread(
            &store,
            "b53623f2-438d-47f3-afd8-11f76c40695b",
            "Default Project",
            workspace.to_str().expect("workspace path"),
            &crate::projects::store_key("codex", directory.path()),
            Some("fake-model"),
            Some("low"),
            "2026-09-01T10:00:00Z",
        )
        .await
        .expect("d19b0f7e-97bb-45d7-8206-324a62386dd9");
        let custom_workspace = directory.path().join("custom-workspace");
        fs::create_dir_all(&custom_workspace).unwrap();
        let custom_workspace = crate::canonical_path::canonicalize(custom_workspace)
            .expect("resolved custom workspace");
        crate::test_support::project_thread(
            &store,
            "f23c6839-034c-4f3a-a7c8-ad385725a309",
            "Custom Project",
            custom_workspace.to_str().expect("workspace path"),
            &crate::projects::store_key("codex", directory.path()),
            None,
            None,
            "2026-09-01T10:01:00Z",
        )
        .await
        .expect("custom project");
        store
            .upsert_owner_device("device-contract", "contract", "{}", "2026-09-01T10:00:00Z")
            .await
            .expect("device");

        let stable_schema = format!(
            "{}/../../research/codex-app-server/0.155.0-alpha.9/stable/codex_app_server_protocol.v2.schemas.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let experimental_schema = format!(
            "{}/../../research/codex-app-server/0.155.0-alpha.9/experimental/codex_app_server_protocol.v2.schemas.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let server_file = directory.path().join("fake-app-server.py");
        let request_log = directory.path().join("app-server-requests.log");
        let fake_server_source = r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    request_id = request.get("id")
    if method == "initialized":
        continue
    if method == "initialize":
        result = {"userAgent": "wonder-contract", "capabilities": {"experimentalApi": True}}
    elif method == "configRequirements/read":
        result = {"requirements": {}}
    elif method == "model/list":
        result = {"data": [{"id": "fake-model", "supportedReasoningEfforts": ["low", "high"]}], "nextCursor": None}
    elif method == "thread/turns/list":
        result = {"data": [], "nextCursor": None}
    elif method == "thread/items/list":
        result = {"data": [], "nextCursor": None}
    elif method == "thread/settings/update":
        with open("__REQUEST_LOG__", "a", encoding="utf-8") as log:
            log.write(method + "\n")
        result = {}
    elif method == "thread/resume":
        with open("__REQUEST_LOG__", "a", encoding="utf-8") as log:
            log.write(method + "\n")
            log.write("resume_developer=" + request.get("params", {}).get("developerInstructions", "") + "\n")
        result = {"thread": {"id": "thread-contract", "sessionId": "session-contract", "status": {"type": "idle"}}}
    elif method == "thread/start":
        result = {"thread": {"id": "thread-contract", "sessionId": "session-contract"}}
    elif method == "turn/start":
        with open("__REQUEST_LOG__", "a", encoding="utf-8") as log:
            params = request.get("params", {})
            log.write(method + "\n")
            log.write("turn_model=" + str(params.get("model")) + "\n")
            log.write("turn_effort=" + str(params.get("effort")) + "\n")
            log.write("turn_service_tier=" + str(params.get("serviceTier")) + "\n")
        result = {"turn": {"id": "turn-contract"}}
        print(json.dumps({"id": request_id, "result": result}), flush=True)
        print(json.dumps({"method": "item/completed", "params": {"threadId": "thread-contract", "turnId": "turn-contract", "item": {"id": "user-contract", "type": "userMessage", "content": [{"text": "say hello"}]}}}), flush=True)
        print(json.dumps({"method": "item/agentMessage/delta", "params": {"threadId": "thread-contract", "turnId": "turn-contract", "item": {"id": "item-contract"}, "sequence": 1, "delta": {"text": "hello "}}}), flush=True)
        print(json.dumps({"method": "item/agentMessage/delta", "params": {"threadId": "thread-contract", "turnId": "turn-contract", "item": {"id": "item-contract"}, "sequence": 2, "delta": {"text": "hello "}}}), flush=True)
        print(json.dumps({"method": "item/agentMessage/delta", "params": {"threadId": "thread-contract", "turnId": "turn-contract", "item": {"id": "item-contract"}, "sequence": 3, "delta": {"text": "world"}}}), flush=True)
        print(json.dumps({"method": "turn/completed", "params": {"threadId": "thread-contract", "turnId": "turn-contract"}}), flush=True)
        print(json.dumps({"method": "item/agentMessage/delta", "params": {"threadId": "thread-contract", "turnId": "turn-contract", "item": {"id": "item-contract"}, "sequence": 4, "delta": {"text": "late delta"}}}), flush=True)
        print(json.dumps({"method": "item/completed", "params": {"threadId": "thread-contract", "turnId": "turn-contract", "item": {"id": "item-contract", "content": [{"text": "late completion must not overwrite"}]}}}), flush=True)
        continue
    else:
        result = {}
    if request_id is not None:
        print(json.dumps({"id": request_id, "result": result}), flush=True)
"#
        .replace("__REQUEST_LOG__", &request_log.to_string_lossy());
        fs::write(&server_file, fake_server_source).expect("fake App Server source");
        // This fake server does not invoke code mode, but models its launch check.
        let helper = directory.path().join("codex-code-mode-host");
        fs::write(
            &helper,
            include_str!("../../../tests/fixtures/code-mode-host.py"),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        let fake_bin = directory.path().join("fake-codex");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli 0.155.0-alpha.9'; exit 0; fi\nif [ \"$1\" = \"app-server\" ] && [ \"$2\" = \"generate-json-schema\" ]; then last=\"\"; for arg in \"$@\"; do last=\"$arg\"; done; if [ \"$3\" = \"--experimental\" ]; then cp '{}' \"$last/codex_app_server_protocol.v2.schemas.json\"; else cp '{}' \"$last/codex_app_server_protocol.v2.schemas.json\"; fi; exit 0; fi\nif [ \"$1\" = \"app-server\" ]; then exec python3 '{}'; fi\nexit 1\n",
            experimental_schema, stable_schema, server_file.display()
        );
        fs::write(&fake_bin, script).expect("fake Codex executable");
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755))
            .expect("fake Codex permissions");
        let app_server = wonder_app_server::AppServerClient::spawn_with_notification_sink(
            wonder_app_server::LaunchConfig {
                codex_bin: fake_bin,
                wonder_version: env!("CARGO_PKG_VERSION").into(),
                codex_home: None,
            },
            crate::ingestion::notification_sink(store.clone()),
        )
        .await
        .expect("fake App Server");
        let (events, _) = tokio::sync::broadcast::channel(256);
        let (revocations, _) = tokio::sync::broadcast::channel(16);

        let logger = Arc::new(
            crate::logging::JsonlLogger::new(directory.path().join("logs"), "wonderd.jsonl")
                .unwrap(),
        );
        let tunnel_status_file = directory.path().join("tunnel-status.jsonl");
        fs::write(&tunnel_status_file, r#"{"state":"starting"}"#).unwrap();
        store.start_event_epoch("contract-epoch").await.unwrap();
        let state = AppState {
            projects: crate::projects::ProjectRuntime::configured(
                directory.path().join("fake-codex"),
                "test".into(),
                directory.path(),
                directory.path(),
                crate::ingestion::notification_sink(store.clone()),
            ),
            claude: None,
            opencode: None,
            cursor: None,
            ingestion: crate::ingestion::Ingestion::default(),
            store,
            logger,
            loopback_capability: "contract-capability".into(),
            host_epoch: "contract-epoch".into(),
            started_at: "2026-09-01T10:00:00Z".into(),
            events,
            journal_wake: Default::default(),
            revocations,
            pairing: Arc::new(tokio::sync::Mutex::new(super::HostPairingState::default())),
            public_origin: "https://wonder.test".into(),
            public_origin_file: Some(tunnel_status_file.clone()),
            home_routes: Default::default(),
            host_installation_id: "contract-installation".into(),
            data_dir: directory.path().to_path_buf(),
            codex_home: directory.path().to_path_buf(),
            denied_roots: vec![],
            provider_reconnect: Default::default(),
            installed_providers: Default::default(),
            dispatch_lock: Arc::new(tokio::sync::Mutex::new(())),
            dispatch_wake: Arc::default(),
            thread_waits: Arc::default(),
            update_admission: Arc::new(Default::default()),
            approval_lock: Arc::new(tokio::sync::Mutex::new(())),
            runtime_catalog: Arc::new(RwLock::new(RuntimeCatalog {
                models: vec![ModelOption {
                    agent_family: wonder_store::AgentFamily::Codex,
                    capabilities: crate::ModelCapabilities::for_family(
                        wonder_store::AgentFamily::Codex,
                    ),
                    id: "fake-model".into(),
                    display_name: "Fake model".into(),
                    description: None,
                    model_specialty: None,
                    hidden: false,
                    reasoning_efforts: vec![
                        ChoiceOption {
                            id: "low".into(),
                            label: "Low".into(),
                            description: None,
                        },
                        ChoiceOption {
                            id: "high".into(),
                            label: "High".into(),
                            description: None,
                        },
                    ],
                    default_reasoning_effort: Some("low".into()),
                    service_tiers: vec![
                        ChoiceOption {
                            id: "default".into(),
                            label: "Standard".into(),
                            description: None,
                        },
                        ChoiceOption {
                            id: "priority".into(),
                            label: "Fast".into(),
                            description: None,
                        },
                    ],
                    default_service_tier: Some("default".into()),
                    is_default: false,
                    provider_default: None,
                    native_ids: vec![],
                }],
                preferences: Default::default(),
                owner_hidden: Default::default(),
            })),
            computer_use_enabled: false,
            computer_use_bin: None,
            computer_supervisor: Arc::new(
                crate::computer_sessions::ComputerSessionSupervisor::default(),
            ),
            simulators: Arc::default(),
            voice: Arc::new(crate::voice::Voice::in_memory()),
            t3_code: Arc::new(crate::t3_code::T3Code::disabled()),
            claude_app: Arc::default(),
            terminals: Arc::default(),
            live_activity: Arc::default(),
            sealed_handshakes: Default::default(),
            relay: crate::relay::Manager::new(
                directory.path().to_path_buf(),
                crate::relay::Environment::disabled(),
                None,
            ),
        };

        *state.projects.codex.lock().await = app_server;
        let pairing_offer_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/pairing/offers")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .body(Body::empty())
                    .expect("pre-tunnel pairing offer request"),
            )
            .await
            .expect("pre-tunnel pairing offer response");
        assert_eq!(
            pairing_offer_response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        let pairing_offer_body = to_bytes(pairing_offer_response.into_body(), 64 * 1024)
            .await
            .expect("pre-tunnel pairing offer body");
        assert_eq!(
            &pairing_offer_body[..],
            b"This computer's connection address isn't ready yet. Check `cupola status`."
        );
        fs::write(
            &tunnel_status_file,
            r#"{"state":"ready","origin":"https://wonder.test"}"#,
        )
        .expect("ready tunnel status");

        let unauthorized_device_list_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/devices")
                    .header("origin", "https://wonder.test")
                    .body(Body::empty())
                    .expect("unauthorized device list request"),
            )
            .await
            .expect("unauthorized device list response");
        assert_eq!(
            unauthorized_device_list_response.status(),
            axum::http::StatusCode::UNAUTHORIZED
        );

        let device_list_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/devices")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .body(Body::empty())
                    .expect("device list request"),
            )
            .await
            .expect("device list response");
        assert_eq!(device_list_response.status(), axum::http::StatusCode::OK);
        let device_list_body = to_bytes(device_list_response.into_body(), 1_000_000)
            .await
            .expect("device list body");
        let device_list: serde_json::Value =
            serde_json::from_slice(&device_list_body).expect("device list JSON");
        assert!(device_list[0]["createdAt"]
            .as_str()
            .is_some_and(|date| !date.is_empty()));
        assert_eq!(device_list[0]["role"], "owner");
        assert_eq!(device_list[0]["label"], "contract");

        let rename_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/api/v1/devices/device-contract")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"label":"Phone"}"#))
                    .expect("device rename request"),
            )
            .await
            .expect("device rename response");
        assert_eq!(rename_response.status(), axum::http::StatusCode::OK);
        let rename_body = to_bytes(rename_response.into_body(), 1_000_000)
            .await
            .expect("device rename body");
        let renamed: serde_json::Value =
            serde_json::from_slice(&rename_body).expect("device rename JSON");
        assert_eq!(renamed["label"], "Phone");

        let invalid_rename_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/api/v1/devices/device-contract")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"label":"\n"}"#))
                    .expect("invalid device rename request"),
            )
            .await
            .expect("invalid device rename response");
        assert_eq!(
            invalid_rename_response.status(),
            axum::http::StatusCode::BAD_REQUEST
        );

        crate::test_support::project_thread(
            &state.store,
            "0818a068-f032-4374-a1b9-51782c8e0e9d",
            "Pending conversation",
            state.data_dir.to_str().unwrap(),
            crate::providers::provider_store(&state, wonder_store::AgentFamily::Codex),
            None,
            None,
            "2026-09-01T10:02:30Z",
        )
        .await
        .expect("pending conversation");
        let pending_message = match state
            .store
            .insert_message(
                "device-contract",
                "00000000-0000-4000-8000-000000000098",
                "pending body",
                &hex::encode(sha2::Sha256::digest(b"pending body")),
                "0818a068-f032-4374-a1b9-51782c8e0e9d",
                "2026-09-01T10:02:31Z",
            )
            .await
            .expect("pending message")
        {
            wonder_store::MessageInsert::Inserted(message) => message,
            _ => panic!("expected pending message to be inserted"),
        };
        publish_app_server_notification(
            &state,
            serde_json::json!({
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": "thread-pending",
                    "turnId": "turn-pending",
                    "item": { "id": "item-pending" },
                    "delta": { "text": "early" }
                }
            }),
        )
        .await;
        assert_eq!(
            state
                .store
                .pending_app_server_notifications_for_turn("turn-pending")
                .await
                .expect("pending notifications")
                .len(),
            1
        );
        state
            .store
            .update_message_delivery(
                &pending_message.id,
                "accepted_by_codex",
                Some("thread-pending"),
                Some("turn-pending"),
            )
            .await
            .expect("map pending turn");
        crate::test_support::bind_project_thread(
            &state,
            "0818a068-f032-4374-a1b9-51782c8e0e9d",
            wonder_store::AgentFamily::Codex,
            "thread-pending",
            None,
            "now",
        )
        .await
        .unwrap();
        drain_pending_app_server_notifications(&state, "thread-pending", "turn-pending").await;
        assert_eq!(
            state
                .store
                .assistant_messages_for_conversation("0818a068-f032-4374-a1b9-51782c8e0e9d")
                .await
                .expect("drained assistant")
                .first()
                .expect("drained message")
                .text,
            "early"
        );
        assert!(state
            .store
            .pending_app_server_notifications_for_turn("turn-pending")
            .await
            .expect("drained notifications")
            .is_empty());

        let mut emitted_events = state.events.subscribe();
        let notification_service = crate::ingestion::spawn(state.clone()).await;
        let dispatcher = crate::dispatch::spawn(state.clone()).await.unwrap();
        for _ in 0..100 {
            if state.ingestion.readiness(&state.store).await.ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let client_message_id = "00000000-0000-4000-8000-000000000001";
        let starts_before_send = fs::read_to_string(&request_log)
            .unwrap_or_default()
            .lines()
            .filter(|method| *method == "turn/start")
            .count();
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b/messages")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "deviceId": "device-contract",
                            "clientMessageId": client_message_id,
                            "body": "say hello"
                        })
                        .to_string(),
                    ))
                    .expect("send request"),
            )
            .await
            .expect("send response");
        if response.status() != axum::http::StatusCode::ACCEPTED {
            let status = response.status();
            let body = to_bytes(response.into_body(), 1_000_000)
                .await
                .expect("error body");
            panic!("send failed: {status}: {}", String::from_utf8_lossy(&body));
        }
        // A direct send is decided up front: the receipt says it is not
        // queued, and the queue never lists it, claimed by dispatch or not.
        let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        validate_http_contract("messageReceipt", &receipt);
        assert_eq!(receipt["queued"], false);
        let queue = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b/queue")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let queue: serde_json::Value =
            serde_json::from_slice(&to_bytes(queue.into_body(), 1_000_000).await.unwrap()).unwrap();
        assert_eq!(queue, serde_json::json!([]));
        // Native response-loss recovery and double taps reuse the exact request.
        // Discard the first receipt, then race two duplicates with dispatch.
        for _ in 0..2 {
            let duplicate = router(state.clone())
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b/messages")
                        .header("x-wonder-loopback-capability", "contract-capability")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "deviceId": "device-contract",
                                "clientMessageId": client_message_id,
                                "body": "say hello",
                                "attachmentIds": []
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(duplicate.status(), axum::http::StatusCode::ACCEPTED);
            let bytes = to_bytes(duplicate.into_body(), 1_000_000).await.unwrap();
            let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let stored = state
                .store
                .message_by_device_and_client_message_id("device-contract", client_message_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(receipt["wonderMessageId"], stored.id);
            assert_eq!(receipt["clientMessageId"], client_message_id);
        }
        for _ in 0..300 {
            if state
                .store
                .assistant_messages_for_conversation("b53623f2-438d-47f3-afd8-11f76c40695b")
                .await
                .expect("assistant lookup")
                .iter()
                .any(|message| message.state == "completed")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut assistant_deltas = Vec::new();
        let mut assistant_completions = Vec::new();
        let assistant_completion_context = loop {
            let event = tokio::time::timeout(Duration::from_secs(2), emitted_events.recv())
                .await
                .expect("completion broadcast")
                .expect("event");
            match &event.event {
                wonder_api::WonderEvent::AssistantDelta { text } => {
                    assistant_deltas.push(text.clone())
                }
                wonder_api::WonderEvent::AssistantCompleted { text } => {
                    assistant_completions.push(text.clone());
                    break event;
                }
                _ => {}
            }
        };
        // Transport batching may join adjacent chunks; clients receive the
        // exact ordered stream before completion, including repeated text.
        assert_eq!(assistant_deltas.concat(), "hello hello world");
        assert_eq!(assistant_completions, vec!["hello hello world"]);
        assert_eq!(
            fs::read_to_string(&request_log)
                .unwrap()
                .lines()
                .filter(|method| *method == "turn/start")
                .count(),
            starts_before_send + 1,
            "duplicate native sends must dispatch exactly once"
        );
        assert_eq!(
            assistant_completion_context.conversation_id.as_deref(),
            Some("b53623f2-438d-47f3-afd8-11f76c40695b")
        );
        assert_eq!(
            assistant_completion_context.thread_id.as_deref(),
            Some("thread-contract")
        );
        assert_eq!(
            assistant_completion_context.turn_id.as_deref(),
            Some("turn-contract")
        );
        assert_eq!(
            assistant_completion_context.item_id.as_deref(),
            Some("item-contract")
        );
        let sent_message = state
            .store
            .message_by_device_and_client_message_id("device-contract", client_message_id)
            .await
            .expect("sent message lookup")
            .expect("sent message");
        assert_eq!(
            assistant_completion_context.message_id.as_deref(),
            Some(sent_message.id.as_str())
        );
        crate::test_support::project_thread(
            &state.store,
            "630b7c71-7c06-40dc-861e-0e667348f948",
            "Other conversation",
            state.data_dir.to_str().unwrap(),
            crate::providers::provider_store(&state, wonder_store::AgentFamily::Codex),
            None,
            None,
            "2026-09-01T10:03:00Z",
        )
        .await
        .expect("other conversation");
        let wrong_send_route = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/conversations/630b7c71-7c06-40dc-861e-0e667348f948/messages")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "deviceId": "device-contract",
                            "clientMessageId": client_message_id,
                            "body": "say hello"
                        })
                        .to_string(),
                    ))
                    .expect("wrong send route request"),
            )
            .await
            .expect("wrong send route response");
        assert_eq!(wrong_send_route.status(), axum::http::StatusCode::CONFLICT);
        let wrong_steer_route = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/conversations/630b7c71-7c06-40dc-861e-0e667348f948/turns/turn-contract/steer")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "deviceId": "device-contract",
                            "clientMessageId": client_message_id,
                            "expectedTurnId": "turn-contract",
                            "body": "say hello"
                        })
                        .to_string(),
                    ))
                    .expect("wrong steer route request"),
            )
            .await
            .expect("wrong steer route response");
        assert_eq!(wrong_steer_route.status(), axum::http::StatusCode::CONFLICT);
        let steered_client_message_id = "00000000-0000-4000-8000-000000000002";
        let steered_body = "continue after completion";
        let steered_hash = hex::encode(sha2::Sha256::digest(steered_body.as_bytes()));
        let steered_message = match state
            .store
            .insert_guide_message(
                "device-contract",
                steered_client_message_id,
                steered_body,
                &steered_hash,
                "b53623f2-438d-47f3-afd8-11f76c40695b",
                &[],
                "2026-09-01T10:01:00Z",
                "turn-contract",
            )
            .await
            .expect("steered message")
        {
            wonder_store::MessageInsert::Inserted(message) => message,
            _ => panic!("expected steered message to be inserted"),
        };
        state
            .store
            .update_message_delivery(
                &steered_message.id,
                "completed",
                Some("thread-contract"),
                Some("turn-contract"),
            )
            .await
            .expect("complete steered message");
        let replay_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b/turns/turn-contract/steer")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "deviceId": "device-contract",
                            "clientMessageId": steered_client_message_id,
                            "expectedTurnId": "turn-contract",
                            "body": steered_body
                        })
                        .to_string(),
                    ))
                    .expect("replay request"),
            )
            .await
            .expect("replay response");
        assert_eq!(replay_response.status(), axum::http::StatusCode::ACCEPTED);
        let replay_body = to_bytes(replay_response.into_body(), 1_000_000)
            .await
            .expect("replay body");
        let replay: serde_json::Value = serde_json::from_slice(&replay_body).expect("replay JSON");
        validate_http_contract("messageReceipt", &replay);
        assert_eq!(replay["deliveryState"], "completed");
        assert_eq!(replay["codexTurnId"], "turn-contract");
        let snapshot_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .body(Body::empty())
                    .expect("snapshot request"),
            )
            .await
            .expect("snapshot response");
        let unsupported_write = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            unsupported_write.status(),
            axum::http::StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(snapshot_response.status(), axum::http::StatusCode::OK);
        let snapshot_body = to_bytes(snapshot_response.into_body(), 1_000_000)
            .await
            .expect("snapshot body");
        let snapshot: serde_json::Value =
            serde_json::from_slice(&snapshot_body).expect("snapshot JSON");
        validate_http_contract("conversationSnapshot", &snapshot);
        let websocket_schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/schemas/wonder-websocket-v1.json"
        ))
        .expect("WebSocket schema JSON");
        let validator = jsonschema::validator_for(&websocket_schema).expect("WebSocket schema");
        // Snapshot events contain canonical typed activities, not replay deltas.
        for event in snapshot["events"].as_array().expect("snapshot events") {
            assert!(
                validator.is_valid(event),
                "invalid WebSocket event: {event}"
            );
        }
        let mut missing_thread = snapshot.clone();
        missing_thread.as_object_mut().unwrap().remove("thread");
        let authority: serde_json::Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/schemas/wonder-http-v1.json"
        ))
        .unwrap();
        let validator = jsonschema::validator_for(&serde_json::json!({
            "$ref": "#/$defs/conversationSnapshot", "$defs": authority["$defs"]
        }))
        .unwrap();
        assert!(!validator.is_valid(&missing_thread));
        if let Ok(output) = std::env::var("WONDER_CONTRACT_FIXTURE_DIR") {
            let output = std::path::Path::new(&output);
            fs::create_dir_all(output).expect("fixture directory");
            for (name, value) in [
                ("conversation-snapshot", &snapshot),
                ("message-receipt", &replay),
            ] {
                fs::write(
                    output.join(format!("{name}.json")),
                    serde_json::to_vec_pretty(value).unwrap(),
                )
                .expect("write synthetic response fixture");
            }
        }
        assert_eq!(
            snapshot["assistantMessages"][0]["text"],
            "hello hello world"
        );
        assert_eq!(snapshot["assistantMessages"][0]["state"], "completed");
        assert_eq!(
            snapshot["assistantMessages"]
                .as_array()
                .expect("assistant messages")
                .len(),
            1
        );

        let upload_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b/files")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "notes.txt",
                            "mimeType": "text/plain",
                            "contentBase64": base64::engine::general_purpose::STANDARD.encode(b"hello")
                        })
                        .to_string(),
                    ))
                    .expect("attachment upload request"),
            )
            .await
            .expect("attachment upload response");
        assert_eq!(upload_response.status(), axum::http::StatusCode::OK);
        let upload_body = to_bytes(upload_response.into_body(), 1_000_000)
            .await
            .expect("attachment metadata body");
        let uploaded: serde_json::Value =
            serde_json::from_slice(&upload_body).expect("attachment metadata JSON");
        let uploaded_id = uploaded["id"].as_str().expect("attachment UUID");
        assert!(uuid::Uuid::parse_str(uploaded_id).is_ok());
        assert_eq!(
            uploaded["relativePath"],
            format!(".wonder/attachments/{uploaded_id}")
        );
        assert_eq!(uploaded["byteSize"], 5);
        assert_eq!(uploaded["sha256"], hex::encode(Sha256::digest(b"hello")));
        let workspace_root = crate::canonical_path::canonicalize_async(
            crate::projects::media_workspace(&state, "b53623f2-438d-47f3-afd8-11f76c40695b")
                .await
                .unwrap(),
        )
        .await
        .expect("canonical workspace");
        assert_eq!(
            tokio::fs::read(workspace_root.join(".wonder/attachments").join(uploaded_id))
                .await
                .expect("stored attachment"),
            b"hello"
        );

        let download_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b/files/{uploaded_id}"))
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .body(Body::empty())
                    .expect("attachment download request"),
            )
            .await
            .expect("attachment download response");
        assert_eq!(download_response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            download_response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/plain")
        );
        assert_eq!(
            to_bytes(download_response.into_body(), 1_000_000)
                .await
                .expect("download body"),
            b"hello".as_slice()
        );

        let wrong_conversation_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/v1/conversations/f23c6839-034c-4f3a-a7c8-ad385725a309/files/{uploaded_id}"))
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .body(Body::empty())
                    .expect("wrong conversation download request"),
            )
            .await
            .expect("wrong conversation response");
        assert_eq!(
            wrong_conversation_response.status(),
            axum::http::StatusCode::NOT_FOUND
        );

        // Removing it from the draft forgets the uploaded copy: record and bytes.
        let remove = |conversation: &str| {
            Request::builder()
                .method(Method::DELETE)
                .uri(format!(
                    "/api/v1/conversations/{conversation}/files/{uploaded_id}"
                ))
                .header("x-wonder-loopback-capability", "contract-capability")
                .body(Body::empty())
                .expect("upload removal request")
        };
        for (conversation, status) in [
            (
                "f23c6839-034c-4f3a-a7c8-ad385725a309",
                axum::http::StatusCode::NOT_FOUND,
            ),
            (
                "b53623f2-438d-47f3-afd8-11f76c40695b",
                axum::http::StatusCode::NO_CONTENT,
            ),
            (
                "b53623f2-438d-47f3-afd8-11f76c40695b",
                axum::http::StatusCode::NOT_FOUND,
            ),
        ] {
            let removed = router(state.clone())
                .oneshot(remove(conversation))
                .await
                .expect("upload removal response");
            assert_eq!(removed.status(), status, "{conversation}");
        }
        assert!(!workspace_root
            .join(".wonder/attachments")
            .join(uploaded_id)
            .exists());
        let gone = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b/files/{uploaded_id}"))
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .body(Body::empty())
                    .expect("removed download request"),
            )
            .await
            .expect("removed download response");
        assert_eq!(gone.status(), axum::http::StatusCode::NOT_FOUND);

        // A phone may lose an upload receipt. Reusing its identity must return
        // the same file, while changed bytes or metadata must never overwrite it.
        let mut upload_id = None;
        for name in ["retry.txt", "retry.txt", "changed.txt"] {
            let response = router(state.clone()).oneshot(
                Request::builder().method(Method::POST)
                    .uri("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b/files")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({
                        "clientUploadId":"00000000-0000-4000-8000-000000000099",
                        "name":name,"mimeType":"text/plain",
                        "contentBase64":base64::engine::general_purpose::STANDARD.encode(b"retained bytes")
                    }).to_string())).unwrap()
            ).await.unwrap();
            if name == "changed.txt" {
                assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
            } else {
                assert!(response.status().is_success());
                let data = to_bytes(response.into_body(), 65536).await.unwrap();
                let value: serde_json::Value = serde_json::from_slice(&data).unwrap();
                if let Some(ref id) = upload_id {
                    assert_eq!(id, &value["id"]);
                }
                upload_id = Some(value["id"].clone());
            }
        }

        for (name, mime_type) in [
            ("../secret.txt", "text/plain"),
            ("notes.txt", "text/plain; charset=utf-8"),
        ] {
            let rejected = router(state.clone())
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b/files")
                        .header("x-wonder-loopback-capability", "contract-capability")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "name": name,
                                "mimeType": mime_type,
                                "contentBase64": base64::engine::general_purpose::STANDARD.encode(b"hello")
                            })
                            .to_string(),
                        ))
                        .expect("rejected attachment request"),
                )
                .await
                .expect("rejected attachment response");
            assert_eq!(rejected.status(), axum::http::StatusCode::BAD_REQUEST);
        }

        let oversized = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
        let oversized_response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/conversations/b53623f2-438d-47f3-afd8-11f76c40695b/files")
                    .header("x-wonder-loopback-capability", "contract-capability")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "large.bin",
                            "mimeType": "application/octet-stream",
                            "contentBase64": base64::engine::general_purpose::STANDARD.encode(oversized)
                        })
                        .to_string(),
                    ))
                    .expect("oversized attachment request"),
            )
            .await
            .expect("oversized attachment response");
        assert_eq!(
            oversized_response.status(),
            axum::http::StatusCode::PAYLOAD_TOO_LARGE
        );

        dispatcher.abort();
        let _ = dispatcher.await;
        drop(notification_service);
        state
            .projects
            .codex
            .lock()
            .await
            .shutdown()
            .await
            .expect("shutdown");
    }

    #[test]
    fn steer_params_preserve_the_protocol_contract() {
        assert_eq!(
            steer_turn_params_with_attachments(
                "thread-1",
                "turn-1",
                "00000000-0000-4000-8000-000000000001",
                vec![serde_json::json!({ "type": "text", "text": "continue with the file" })]
            )
            .to_string(),
            serde_json::json!({
                "threadId": "thread-1",
                "expectedTurnId": "turn-1",
                "clientUserMessageId": "00000000-0000-4000-8000-000000000001",
                "input": [{ "type": "text", "text": "continue with the file" }]
            })
            .to_string()
        );
    }

    #[test]
    fn steer_response_requires_the_app_server_turn_id() {
        let missing = wonder_app_server::RpcResponse {
            id: 1,
            result: Some(serde_json::json!({})),
            error: None,
        };
        assert_eq!(steer_response_turn_id(&missing), None);

        let route_only = wonder_app_server::RpcResponse {
            id: 1,
            result: Some(serde_json::json!({ "turnId": "" })),
            error: None,
        };
        assert_eq!(steer_response_turn_id(&route_only), None);

        let accepted = wonder_app_server::RpcResponse {
            id: 1,
            result: Some(serde_json::json!({ "turnId": "turn-from-app-server" })),
            error: None,
        };
        assert_eq!(
            steer_response_turn_id(&accepted),
            Some("turn-from-app-server".to_owned())
        );
    }

    #[tokio::test]
    async fn late_guide_receipt_waits_for_update_lease_and_rechecks_exact_message() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let wonder_store::MessageInsert::Inserted(message) = state
            .store
            .insert_guide_message(
                "owner",
                "late-guide",
                "continue",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                &[],
                "now",
                "parent-turn",
            )
            .await
            .unwrap()
        else {
            panic!("new guide")
        };
        assert!(state
            .store
            .claim_guide(&message.id, "thread")
            .await
            .unwrap());
        assert_eq!(
            state
                .update_admission
                .prepare_update("guide-update", &state)
                .await,
            Ok(())
        );
        assert!(!super::accept_reconciled_guide(&state, &message, "thread", "accepted-turn").await);
        assert_eq!(
            state
                .store
                .message_by_id(&message.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "uncertain"
        );
        assert!(state.update_admission.cancel_lease("guide-update").await);
        assert!(
            !super::accept_reconciled_guide(&state, &message, "forged-thread", "accepted-turn")
                .await
        );
        assert!(super::accept_reconciled_guide(&state, &message, "thread", "accepted-turn").await);
        assert!(!super::accept_reconciled_guide(&state, &message, "thread", "other-turn").await);
        let saved = state
            .store
            .message_by_id(&message.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, "accepted_by_codex");
        assert_eq!(saved.codex_turn_id.as_deref(), Some("accepted-turn"));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[test]
    fn automation_schedule_calculates_daily_and_weekly_local_times() {
        let after = chrono::DateTime::parse_from_rfc3339("2026-01-15T14:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            next_automation_run("FREQ=DAILY;BYHOUR=9;BYMINUTE=0", "America/New_York", after)
                .unwrap(),
            Some("2026-01-16T14:00:00.000Z".into())
        );
        assert_eq!(
            next_automation_run(
                "FREQ=WEEKLY;BYDAY=FR;BYHOUR=9;BYMINUTE=0",
                "America/New_York",
                after,
            )
            .unwrap(),
            Some("2026-01-16T14:00:00.000Z".into())
        );
        assert_eq!(
            next_automation_run(
                "FREQ=MINUTELY;INTERVAL=5",
                "UTC",
                chrono::DateTime::parse_from_rfc3339("2026-01-15T14:32:14Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            )
            .unwrap(),
            Some("2026-01-15T14:35:00.000Z".into())
        );
    }

    #[test]
    fn automation_schedule_rejects_unknown_timezone_or_rule() {
        let after = chrono::DateTime::parse_from_rfc3339("2026-01-15T14:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(next_automation_run("FREQ=YEARLY", "UTC", after).is_err());
        assert!(next_automation_run("FREQ=DAILY;INTERVAL=5", "UTC", after).is_err());
        assert!(next_automation_run("FREQ=MINUTELY;INTERVAL=1441", "UTC", after).is_err());
        assert!(next_automation_run("FREQ=DAILY;BYHOUR=9", "Not/AZone", after).is_err());
    }

    #[test]
    fn runtime_catalog_uses_model_specific_effort_and_speed_options() {
        let mut catalog = RuntimeCatalog::default();
        catalog.apply_models_page(
            wonder_store::AgentFamily::Codex,
            &serde_json::json!({
                "data": [{
                    "id": "model-a",
                    "displayName": "Model A",
                    "description": "A useful model",
                    "modelSpecialty": "Planning",
                    "supportedReasoningEfforts": [{"reasoningEffort":"high","description":"Deep"}],
                    "defaultReasoningEffort": "high",
                    "serviceTiers": [{"id":"fast","name":"Fast"}]
                }]
            }),
        );
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].reasoning_efforts[0].id, "high");
        assert_eq!(
            catalog.models[0].model_specialty.as_deref(),
            Some("Planning")
        );
        assert_eq!(
            catalog.models[0].description.as_deref(),
            Some("A useful model")
        );
        assert_eq!(catalog.models[0].service_tiers[0].id, "default");
        assert_eq!(catalog.models[0].service_tiers[0].label, "Standard");
        assert_eq!(catalog.models[0].service_tiers[1].id, "fast");
        assert_eq!(
            catalog.models[0].default_reasoning_effort.as_deref(),
            Some("high")
        );
    }

    // Contract: the provider that lists a model owns its family. A model id is
    // never read to decide it, and one provider cannot shadow another's models.
    #[test]
    fn model_family_comes_from_the_listing_provider_not_the_model_id() {
        let mut catalog = RuntimeCatalog::default();
        let refused = catalog.apply_models_page(
            wonder_store::AgentFamily::Codex,
            &serde_json::json!({"data":[{"id":"gpt-5"},{"id":"claude:imposter"}]}),
        );
        assert_eq!(refused, 1, "Codex cannot list inside the Claude namespace");
        assert_eq!(
            catalog.family_of("gpt-5"),
            Some(wonder_store::AgentFamily::Codex)
        );
        assert_eq!(catalog.family_of("claude:imposter"), None);
        assert_eq!(catalog.family_of("never-listed"), None);

        let refused = catalog.apply_models_page(
            wonder_store::AgentFamily::Claude,
            &serde_json::json!({"data":[{"id":"claude:sonnet"},{"id":"gpt-5"},{"id":"plain"}]}),
        );
        assert_eq!(
            refused, 2,
            "Claude cannot shadow Codex or leave its namespace"
        );
        assert_eq!(
            catalog.family_of("claude:sonnet"),
            Some(wonder_store::AgentFamily::Claude)
        );
        assert_eq!(
            catalog.family_of("gpt-5"),
            Some(wonder_store::AgentFamily::Codex)
        );
        assert_eq!(catalog.family_of("plain"), None);
        let native = "cursor:gpt-6-luna[context=272k,reasoning=low,fast=false]";
        let refused = catalog.apply_models_page(wonder_store::AgentFamily::Cursor,
            &serde_json::json!({"data":[{"id":native},{"id":"opencode:local/model"},{"id":"plain"}]}));
        assert_eq!(refused, 2);
        assert_eq!(
            catalog.family_of(native),
            Some(wonder_store::AgentFamily::Cursor)
        );
        let cursor = catalog.models.iter().find(|m| m.id == native).unwrap();
        assert!(!cursor.capabilities.guide && !cursor.capabilities.goals);
        assert!(cursor.service_tiers.iter().all(|tier| tier.id == "default"));
        let claude = catalog
            .models
            .iter()
            .find(|m| m.id == "claude:sonnet")
            .unwrap();
        assert!(!claude.capabilities.goals && claude.service_tiers.is_empty());
    }

    #[test]
    fn relative_path_projection_rejects_external_and_traversal_paths() {
        assert_eq!(
            sanitized_relative_path("/workspace/src/main.rs", "/workspace"),
            Some("src/main.rs".into())
        );
        assert_eq!(
            sanitized_relative_path("/Users/example/.ssh/id_rsa", "/workspace"),
            None
        );
        assert_eq!(sanitized_relative_path("../secret.txt", "/workspace"), None);
    }

    #[test]
    fn runtime_capability_projection_excludes_install_urls_and_filesystem_paths() {
        let app = runtime_app_summary(&serde_json::json!({
            "id": "calendar",
            "name": "Calendar",
            "description": "Schedule context",
            "installUrl": "https://secret.example/install",
            "isEnabled": true,
            "isAccessible": true
        }))
        .expect("valid App summary");
        let skill = runtime_skill_summary(&serde_json::json!({
            "name": "launch-brief",
            "description": "Prepare a launch brief",
            "path": "/Users/example/.codex/skills/launch-brief/SKILL.md",
            "enabled": true
        }))
        .expect("valid Skill summary");
        let server = runtime_mcp_server_summary(&serde_json::json!({
            "name": "local-tools",
            "authStatus": "unsupported",
            "runtimeStatus": "connected",
            "tools": {"read": {}, "write": {}},
            "token": "must-not-be-returned"
        }))
        .expect("valid MCP summary");
        let serialized = serde_json::to_string(&(app, skill, server)).unwrap();
        assert!(!serialized.contains("installUrl"));
        assert!(!serialized.contains("SKILL.md"));
        assert!(!serialized.contains("must-not-be-returned"));
        assert!(serialized.contains("toolCount"));
    }

    // Contract: a retried upload that recreates a file a removal just deleted
    // waits for that removal's cleanup, so the new record keeps its bytes.
    #[tokio::test]
    async fn a_retried_upload_after_a_removal_keeps_its_bytes() {
        use crate::test_support::{call, fixture};
        use axum::body::{to_bytes, Body};
        use axum::http::Request;
        use tower::ServiceExt;
        let (_dir, state) = fixture().await;
        let files = "/api/v1/conversations/d19b0f7e-97bb-45d7-8206-324a62386dd9/files";
        let upload = serde_json::json!({
            "clientUploadId": "00000000-0000-4000-8000-0000000000aa",
            "name": "sketch.txt",
            "mimeType": "text/plain",
            "contentBase64": base64::engine::general_purpose::STANDARD.encode(b"sketch bytes"),
        });
        let (status, first) = call(&state, "POST", files, upload.clone()).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{first}");
        let id = first["id"].as_str().unwrap().to_owned();
        let reached = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *crate::attachments::REMOVAL_PAUSE.lock().unwrap() =
            Some((id.clone(), reached.clone(), release.clone()));
        let removal = tokio::spawn({
            let (state, path) = (state.clone(), format!("{files}/{id}"));
            async move { call(&state, "DELETE", &path, serde_json::json!({})).await.0 }
        });
        // The record is deleted; the removal has not cleaned up the bytes yet.
        reached.notified().await;
        let retry = tokio::spawn({
            let state = state.clone();
            async move { call(&state, "POST", files, upload).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!retry.is_finished(), "the retry waits for the removal");
        release.notify_one();
        assert_eq!(removal.await.unwrap(), axum::http::StatusCode::NO_CONTENT);
        let (status, again) = retry.await.unwrap();
        *crate::attachments::REMOVAL_PAUSE.lock().unwrap() = None;
        assert_eq!(status, axum::http::StatusCode::OK, "{again}");
        assert_eq!(again["id"], id.as_str());
        let download = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(format!("{files}/{id}"))
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(download.status(), axum::http::StatusCode::OK);
        assert_eq!(
            to_bytes(download.into_body(), 1_000_000)
                .await
                .unwrap()
                .as_ref(),
            b"sketch bytes"
        );
        state.projects.shutdown().await;
    }

    // Contract: a Project's Outputs list what its agents produced across its
    // threads, without the owner's uploads by default; a file whose bytes
    // left this Mac is listed as gone, not served; pins come first and pages
    // never repeat.
    #[tokio::test]
    async fn project_outputs_list_agent_files_and_label_vanished_ones() {
        use crate::test_support::{assert_http_contract, call, fixture};
        use axum::http::StatusCode;
        let (dir, state) = fixture().await;
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let project = state
            .store
            .project_conversation(conversation)
            .await
            .unwrap()
            .unwrap()
            .project_id;
        let outputs = format!("/api/v1/projects/{project}/files");
        let (status, _) = call(
            &state,
            "POST",
            &format!("/api/v1/conversations/{conversation}/files"),
            serde_json::json!({
                "clientUploadId": "00000000-0000-4000-8000-0000000000ab",
                "name": "brief.txt",
                "mimeType": "text/plain",
                "contentBase64": base64::engine::general_purpose::STANDARD.encode(b"brief"),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let storage = crate::projects::media_workspace(&state, conversation)
            .await
            .unwrap();
        let report = crate::tool_media::store(
            &state,
            conversation,
            &storage,
            "tool-media:outputs:t:i:0",
            "tool-media:t:i:0",
            "Weekly report.html",
            "text/html",
            b"<p>report</p>",
        )
        .await
        .unwrap();
        let report_id = report["file"]["id"].as_str().unwrap().to_owned();
        let picture = dir.path().join("chart.png");
        std::fs::write(&picture, b"png bytes").unwrap();
        state
            .store
            .upsert_referenced_file(
                "chart",
                conversation,
                "chart.png",
                "image/png",
                9,
                &hex::encode(Sha256::digest(b"png bytes")),
                "tool-media:t:i:1",
                &wonder_store::MediaSource {
                    location: wonder_store::MediaSourceLocation::File {
                        path: picture.to_string_lossy().into_owned(),
                    },
                    width: None,
                    height: None,
                },
                "2999-01-01T00:00:00.000Z",
            )
            .await
            .unwrap();

        let (status, listed) = call(&state, "GET", &outputs, serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        assert_http_contract("projectOutputsResponse", &listed);
        let names: Vec<_> = listed["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["chart.png", "Weekly report.html"],
            "no owner upload"
        );
        assert!(listed["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file["availability"] == "available"
                && file["origin"] == "agent"
                && file["conversationId"] == conversation));
        assert!(listed["nextCursor"].is_null());

        std::fs::remove_file(&picture).unwrap();
        let (_, listed) = call(
            &state,
            "GET",
            &format!("{outputs}?type=images"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(listed["files"][0]["id"], "chart");
        assert_eq!(listed["files"][0]["availability"], "gone");
        let (_, listed) = call(
            &state,
            "GET",
            &format!("{outputs}?type=pages&q=WEEKLY"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(listed["files"][0]["id"], report_id.as_str());
        assert_eq!(listed["files"][0]["availability"], "available");
        let (_, uploads) = call(
            &state,
            "GET",
            &format!("{outputs}?kind=uploaded"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(uploads["files"][0]["name"], "brief.txt");
        assert_eq!(uploads["files"][0]["origin"], "owner");
        assert_eq!(uploads["files"].as_array().unwrap().len(), 1);

        let pin = format!("{outputs}/{report_id}/pin");
        assert_eq!(
            call(&state, "PUT", &pin, serde_json::json!({})).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(
                &state,
                "PUT",
                &format!("{outputs}/unknown/pin"),
                serde_json::json!({})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let (status, first) = call(
            &state,
            "GET",
            &format!("{outputs}?limit=1"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_http_contract("projectOutputsResponse", &first);
        assert_eq!(first["files"][0]["id"], report_id.as_str());
        assert_eq!(first["files"][0]["pinned"], true);
        let cursor = first["nextCursor"].as_str().unwrap();
        let (_, next) = call(
            &state,
            "GET",
            &format!("{outputs}?limit=1&cursor={cursor}"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(next["files"][0]["id"], "chart");
        assert!(next["nextCursor"].is_null());
        assert_eq!(
            call(&state, "DELETE", &pin, serde_json::json!({})).await.0,
            StatusCode::NO_CONTENT
        );
        let (_, unpinned) = call(&state, "GET", &outputs, serde_json::json!({})).await;
        assert_eq!(unpinned["files"][0]["id"], "chart");

        for bad in [
            "?type=videos",
            "?kind=everything",
            "?cursor=garbage",
            "?limit=0",
            "?limit=51",
            "?unexpected=1",
        ] {
            assert_eq!(
                call(
                    &state,
                    "GET",
                    &format!("{outputs}{bad}"),
                    serde_json::json!({})
                )
                .await
                .0,
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
        assert_eq!(
            call(
                &state,
                "GET",
                "/api/v1/projects/no-such-project/files",
                serde_json::json!({})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        state.projects.shutdown().await;
    }

    #[tokio::test]
    async fn attachment_ids_are_bounded_and_turn_input_preserves_media_types() {
        let first = "550e8400-e29b-41d4-a716-446655440000".to_owned();
        let second = "550e8400-e29b-41d4-a716-446655440001".to_owned();
        assert!(valid_attachment_ids(&[first.clone(), second.clone()]));
        assert!(!valid_attachment_ids(&[first.clone(), first.clone()]));
        assert!(!valid_attachment_ids(&["not-a-uuid".to_owned()]));
        let files = [
            wonder_store::StoredConversationFile {
                id: first,
                conversation_id: "conversation".into(),
                kind: "attachment".into(),
                name: "photo.png".into(),
                mime_type: Some("image/png".into()),
                byte_size: Some(1),
                sha256: Some("a".repeat(64)),
                relative_path: Some(".wonder/attachments/photo".into()),
                state: "available".into(),
                additions: None,
                deletions: None,
                source_id: None,
                created_at: "now".into(),
                updated_at: "now".into(),
            },
            wonder_store::StoredConversationFile {
                id: second,
                conversation_id: "conversation".into(),
                kind: "attachment".into(),
                name: "notes.txt".into(),
                mime_type: Some("text/plain".into()),
                byte_size: Some(1),
                sha256: Some("a".repeat(64)),
                relative_path: Some(".wonder/attachments/notes".into()),
                state: "available".into(),
                additions: None,
                deletions: None,
                source_id: None,
                created_at: "now".into(),
                updated_at: "now".into(),
            },
        ];
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1];
        let sent = [
            (files[0].clone(), Some(png.to_vec())),
            (files[1].clone(), Some(b"n".to_vec())),
            (
                wonder_store::StoredConversationFile {
                    mime_type: Some("audio/wav".into()),
                    ..files[0].clone()
                },
                Some(b"RIFF".to_vec()),
            ),
        ];
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().to_str().unwrap();
        let input = turn_input("Look at these", root, &sent).await.unwrap();
        assert_eq!(input[0]["text"], "Look at these");
        // Media goes as the verified bytes, never a path the provider reopens.
        assert_eq!(input[1]["type"], "image");
        assert_eq!(
            input[1]["url"],
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(png)
            )
        );
        assert_eq!(input[2]["type"], "text");
        assert!(input[2]["text"].as_str().unwrap().contains("notes.txt"));
        assert_eq!(input[3]["type"], "audio");
        assert_eq!(input[3]["url"], "data:audio/wav;base64,UklGRg==");
        assert!(!serde_json::to_string(&input[1]).unwrap().contains(root));
        let native = crate::attachments::acp_input("Read these", "/tmp/project", &files).unwrap();
        assert_eq!(native[1]["type"], "localImage");
        assert_eq!(native[2]["type"], "localFile");
        assert_eq!(native[2]["mimeType"], "text/plain");
        assert_eq!(native[2]["sha256"], "a".repeat(64));
        assert_eq!(native[2]["byteSize"], 1);
        assert_eq!(native[2]["name"], "notes.txt");
        let audio = wonder_store::StoredConversationFile {
            mime_type: Some("audio/wav".into()),
            ..files[0].clone()
        };
        assert!(
            crate::attachments::acp_input("Listen", "/tmp/project", &[audio])
                .unwrap_err()
                .contains("audio")
        );
    }

    #[tokio::test]
    async fn attachment_boundary_and_metadata_are_durable() {
        let directory = tempfile::tempdir().expect("workspace directory");
        assert!(valid_attachment_name("notes.txt"));
        assert!(!valid_attachment_name("../notes.txt"));
        assert!(!valid_attachment_name("nested/notes.txt"));
        assert!(!valid_attachment_name(" notes.txt"));
        assert!(!valid_attachment_name("notes.txt\n"));
        assert!(valid_attachment_mime_type("text/plain"));
        assert!(valid_attachment_mime_type("application/vnd.example+json"));
        assert!(!valid_attachment_mime_type("text/plain; charset=utf-8"));
        assert!(!valid_attachment_mime_type("text"));
        assert!(!valid_attachment_mime_type("text/plain\r\n"));
        let empty = Vec::<u8>::new();
        assert!(empty.is_empty());
        let small = [0u8; 1];
        assert!(small.len() <= MAX_ATTACHMENT_BYTES);
        let oversized = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
        assert!(oversized.len() > MAX_ATTACHMENT_BYTES);

        let file_id = "00000000-0000-4000-8000-000000000001";
        let relative = attachment_relative_path(file_id);
        let root = crate::canonical_path::canonicalize_async(directory.path())
            .await
            .expect("workspace root");
        let candidate = root.join(&relative);
        let contained = attachment_path(
            directory.path().to_str().expect("workspace path"),
            file_id,
            true,
        )
        .await
        .expect("contained attachment");
        assert_eq!(contained, candidate);
        let workspace = directory.path().to_str().expect("workspace path");
        std::fs::remove_dir(candidate.parent().unwrap()).expect("empty attachment storage");
        assert_eq!(
            crate::locate_attachment(workspace, file_id, false).await,
            Err(crate::AttachmentPathError::Missing)
        );
        assert_eq!(
            crate::locate_attachment(workspace, "../outside.txt", false).await,
            Err(crate::AttachmentPathError::Unsafe)
        );
        assert_eq!(
            crate::locate_attachment(workspace, file_id, true).await,
            Ok(candidate.clone())
        );
        assert!(attachment_path(
            directory.path().to_str().expect("workspace path"),
            "../outside.txt",
            false,
        )
        .await
        .is_none());
        #[cfg(unix)]
        {
            let redirected = tempfile::tempdir().expect("redirected workspace");
            let outside = tempfile::tempdir().expect("outside workspace");
            std::os::unix::fs::symlink(outside.path(), redirected.path().join(".wonder"))
                .expect("redirected attachment parent");
            assert!(attachment_path(
                redirected.path().to_str().expect("redirected path"),
                file_id,
                true,
            )
            .await
            .is_none());
            assert!(
                !outside.path().join("attachments").exists(),
                "an attachment lookup must not create a directory through a symlink"
            );
        }

        let content = b"hello";
        let sha256 = hex::encode(Sha256::digest(content));
        let store = wonder_store::Store::connect("sqlite::memory:?cache=shared")
            .await
            .expect("store");
        crate::test_support::project_thread(
            &store,
            "default",
            "Default",
            directory.path().to_str().expect("workspace path"),
            &crate::projects::store_key("codex", directory.path()),
            None,
            None,
            "2026-09-01T00:00:00Z",
        )
        .await
        .expect("d19b0f7e-97bb-45d7-8206-324a62386dd9");
        store
            .upsert_conversation_file(
                "file-1",
                "conversation-1",
                "attachment",
                "notes.txt",
                Some("text/plain"),
                Some(content.len() as i64),
                Some(&sha256),
                Some(&relative),
                "available",
                None,
                None,
                None,
                "2026-09-01T00:00:00Z",
            )
            .await
            .expect("metadata");
        let files = store
            .list_conversation_files("conversation-1")
            .await
            .expect("files");
        assert_eq!(files[0].byte_size, Some(5));
        assert_eq!(files[0].sha256.as_deref(), Some(sha256.as_str()));
        assert!(store
            .list_conversation_files("other-conversation")
            .await
            .expect("other conversation files")
            .is_empty());
    }
}

#[cfg(all(test, unix))]
mod enrollment_tests;
