//! T3 Code on this Mac (owner, Oct 18): when T3 Code has a Claude or Codex
//! thread open and the owner connected Cupola to it, a message from the phone
//! goes into that T3 thread through T3 Code's own MCP endpoint
//! (`t3_thread_send`) instead of waiting for T3 Code or stopping it. Without
//! the connection the message waits, and Send now takes the thread over (see
//! `projects::held_on_mac`). The thread stays held either way: only the send
//! path asks whether T3 Code takes a message (`delivery`), and that send is
//! durable and read back like any other (`projects::deliver_through_t3_code`).
//!
//! A thread T3 Code knows (started there, or with Cupola's Start in T3 Code)
//! goes through T3 Code even when T3 Code no longer has it open (owner,
//! Oct 21): T3 Code reopens its session for the message (read in T3 Code
//! 0.1.1: `ThreadManagementService.sendToThread` dispatches the message, and
//! starting its turn opens or resumes the provider session), so the reply has
//! T3 Code's tools and shows there. Cupola runs the reply itself only while
//! T3 Code isn't running or answering, and the phone says so (`unheld_note`).
//! Whether T3 Code owns such a thread is read from T3 Code's own store by
//! the session's exact ID (`T3Code::kept`), so it is known at once, also for
//! a thread started in T3 Code that Cupola never saw open there; a thread it
//! doesn't own goes at once and never waits on T3 Code. A thread a lookup
//! matched stays in Cupola's store too, which outlives a restart.
//!
//! T3 Code signs outside agents in with OAuth 2.1 (authorization code with
//! S256 PKCE, dynamic registration, a loopback redirect). Its approval page
//! asks a browser without a T3 Code session for a one-time pairing code, so
//! Connect approves Cupola itself (owner, Oct 21: one tap, no codes): T3
//! Code's own command (`t3 auth pairing create`) issues that code on this Mac
//! and Cupola sends T3 Code the page's decision with it (`approve_here`).
//! The page opens in the computer's browser only when the owner chooses it. T3
//! Code issues no refresh token: a grant lasts 30 days, so Cupola signs in
//! the same way again before it ends or once T3 Code refuses it (`renew`).
//! Its tools do not expose the Claude or Codex session behind
//! a thread, so a thread is matched only on evidence: the same provider and
//! folder, its first message opens the session's transcript and its latest
//! is among the transcript's latest. Exactly one thread must match.
//!
//! Matching reads T3 Code over the network, so it never runs where a send
//! waits for it (the dispatch loop, the send receipt, the phone's note): each
//! thread is looked up in a task of its own, bounded by a timeout, and those
//! read only what the last lookup found (`route`). A thread not yet looked up
//! is held as without T3 Code until its lookup finds the T3 thread and wakes
//! dispatch; a failed lookup is retried after a wait that doubles.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wonder_store::{AgentFamily, StoredProjectConversation};

use crate::{AppState, OwnerAuthority};

pub(crate) mod feed;
pub(crate) mod questions;
pub(crate) mod queue;
pub(crate) mod stream;
pub(crate) mod threads;
pub(crate) mod titles;

pub(crate) const FEATURE: &str = "t3code.mcp.v1";
/// T3's own thread modes, read from its projection rather than Cupola's
/// settings for replies it runs itself. Permission requests cannot be
/// answered by T3's MCP pending-request tools; see plans/t3-approval-api.md.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Permissions {
    pub(crate) runtime_mode: String,
    pub(crate) interaction_mode: String,
}

/// The holder name `desktop_activity` gives T3 Code's processes.
pub(crate) const APP: &str = "T3 Code";
const PROTOCOL: &str = "2025-06-18";
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "com.swaymun.wonder.t3code";
/// How long the owner has to approve Cupola in T3 Code's page.
const APPROVAL_WINDOW: Duration = Duration::from_secs(10 * 60);
/// The access Cupola asks T3 Code for. T3 Code refuses a message into a
/// thread whose access is broader than its client's
/// (`runtime_mode_escalation_denied`), and its threads run with full access
/// by default, so anything less would leave those messages waiting.
const ACCESS: &str = "full-access";
/// What a pairing code must carry for `ACCESS` (T3 Code's `mcpClientScopes`).
const ACCESS_SCOPES: [&str; 4] = [
    "orchestration:read",
    "orchestration:operate",
    "settings:write",
    "source-control:write",
];
/// A grant is renewed once it has less than this left.
const RENEW_BEFORE_MS: i64 = 3 * 24 * 3600 * 1000;
/// After a renewal failed, the next waits this long.
const RENEW_RETRY: Duration = Duration::from_secs(10 * 60);
/// A Keychain that could not be read is read again after this.
const UNREADABLE_RETRY: Duration = Duration::from_secs(60);
/// A thread no T3 thread matched is looked for again after this.
const UNMATCHED_RETRY: Duration = Duration::from_secs(30);
/// A match is used this long after its lookup, and looked up again in the
/// background once it is `MATCH_REFRESH` old.
const MATCH_TTL: Duration = Duration::from_secs(10 * 60);
const MATCH_REFRESH: Duration = Duration::from_secs(5 * 60);
/// A lookup T3 Code has not answered by then counts as failed.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);
/// The wait after a failed lookup, doubling with each failure in a row.
const LOOKUP_BACKOFF: Duration = Duration::from_secs(5);
const LOOKUP_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
/// Threads whose lookup is remembered at once.
const MAX_LOOKUPS: usize = 256;
/// After T3 Code did not take a message, sends wait as before this long.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);
/// How long Take over with files waits for T3 Code's reply to stop.
const RESTART_WAIT: Duration = Duration::from_secs(10);
/// The refusal when that reply didn't stop in time: nothing was sent.
pub(crate) const RESTART_TIMED_OUT: &str = "restart_timed_out";
const MAX_PAGES: usize = 20;
/// The wait after a failed lookup of whether T3 Code started a message
/// (`reconcile_sends`), doubling with each failure in a row.
const ASK_BACKOFF: Duration = Duration::from_secs(30);
const ASK_BACKOFF_MAX: Duration = Duration::from_secs(3600);
const MAX_CANDIDATES: usize = 30;
/// The most one T3 Code response may hold in memory: an MCP answer or
/// refusal, an OAuth document, server-sent events. Counted over the bytes
/// actually received (`collect_bounded`), never from Content-Length.
const MAX_BODY: usize = 4 << 20;
/// How long the browser launcher (`open`, `xdg-open`) may run before it is
/// killed and the launch counts as failed.
const OPEN_DEADLINE: Duration = Duration::from_secs(10);
/// How long a launcher that was killed is waited for, to reap it.
const REAP_WAIT: Duration = Duration::from_secs(2);
/// What T3 Code takes in one message: up to 8 files, images (GIF, JPEG, PNG,
/// WebP) up to 10 MiB and other files up to 50 MiB.
pub(crate) const MAX_FILES: usize = 8;
const MAX_IMAGE_BYTES: i64 = 10 << 20;
const MAX_FILE_BYTES: i64 = 50 << 20;
const IMAGE_TYPES: [&str; 4] = ["image/gif", "image/jpeg", "image/png", "image/webp"];

/// Where the grant (T3 Code's access token) is kept: the login Keychain for
/// the daemon, memory for tests.
pub trait GrantStore: Send + Sync {
    fn read(&self) -> Result<Option<String>, String>;
    fn write(&self, grant: &str) -> Result<(), String>;
    fn remove(&self) -> Result<(), String>;
}

/// A generic-password item, one per Wonder data home (like the voice key).
/// Elsewhere, a file only this user can read inside the data home.
pub struct KeychainGrantStore {
    #[cfg(target_os = "macos")]
    account: String,
    #[cfg(not(target_os = "macos"))]
    path: PathBuf,
}

impl KeychainGrantStore {
    pub fn for_data_home(data_home: &Path) -> Self {
        Self {
            #[cfg(target_os = "macos")]
            account: crate::data_home::keychain_account(data_home),
            #[cfg(not(target_os = "macos"))]
            path: data_home.join("secrets").join("t3code-grant"),
        }
    }
}

#[cfg(target_os = "macos")]
impl GrantStore for KeychainGrantStore {
    fn read(&self) -> Result<Option<String>, String> {
        match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, &self.account) {
            Ok(bytes) => Ok(String::from_utf8(bytes).ok()),
            Err(error) if error.code() == -25300 => Ok(None),
            Err(_) => Err("This computer's Keychain could not be read.".into()),
        }
    }
    fn write(&self, grant: &str) -> Result<(), String> {
        security_framework::passwords::set_generic_password(
            KEYCHAIN_SERVICE,
            &self.account,
            grant.as_bytes(),
        )
        .map_err(|_| "This computer's Keychain could not save the connection.".into())
    }
    fn remove(&self) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(
            KEYCHAIN_SERVICE,
            &self.account,
        ) {
            Err(error) if error.code() != -25300 => {
                Err("This computer's Keychain could not remove the connection.".into())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl GrantStore for KeychainGrantStore {
    fn read(&self) -> Result<Option<String>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("The saved T3 Code connection could not be read.".into()),
        }
    }
    fn write(&self, grant: &str) -> Result<(), String> {
        let failed = |_| "The T3 Code connection could not be saved.".to_owned();
        let dir = self.path.parent().ok_or("The secrets folder is missing.")?;
        std::fs::create_dir_all(dir).map_err(failed)?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, grant).map_err(failed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(failed)?;
        }
        std::fs::rename(&tmp, &self.path).map_err(failed)
    }
    fn remove(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err("The T3 Code connection could not be removed.".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Default)]
pub struct MemoryGrantStore(Mutex<Option<String>>);

impl GrantStore for MemoryGrantStore {
    fn read(&self) -> Result<Option<String>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&self, grant: &str) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(grant.to_owned());
        Ok(())
    }
    fn remove(&self) -> Result<(), String> {
        *self.0.lock().unwrap() = None;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Grant {
    /// The T3 Code server it was issued by, such as `http://127.0.0.1:3773`.
    origin: String,
    access_token: String,
    expires_at_ms: i64,
    /// T3 Code's session behind the token (the `sid` its token carries),
    /// which is the row under T3 Code's Authorized clients: Cupola revokes
    /// it once a renewal replaces it and on Disconnect, so T3 Code lists one
    /// Cupola at a time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    /// T3 Code refused the grant before it ended: the owner removed Cupola
    /// there. Cupola never signs itself in again after that; only the
    /// owner's Connect does (owner, Oct 21: "ask on phone").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    revoked: bool,
}

/// The session ID in a T3 Code token: its claims (`sid`) are the
/// base64url JSON before the signature (`SessionStore.issue`).
fn token_session(token: &str) -> Option<String> {
    let claims = URL_SAFE_NO_PAD
        .decode(token.split('.').next()?.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Value>(&claims)
        .ok()?
        .get("sid")
        .and_then(Value::as_str)
        .filter(|sid| !sid.is_empty())
        .map(str::to_owned)
}

impl Grant {
    /// Its T3 Code session: kept with the grant, or read from the token of
    /// a grant saved before Cupola kept it.
    fn session(&self) -> Option<String> {
        self.session_id
            .clone()
            .or_else(|| token_session(&self.access_token))
    }
}

/// A chat's T3 Code subtasks; `complete` is false when the budget ran out
/// before every page was read, so a missing child may just be unread.
pub(crate) struct Subtasks {
    pub(crate) found: Vec<Value>,
    pub(crate) complete: bool,
}

/// Why a call to T3 Code did not succeed, in the terms dispatch acts on.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CallError {
    NotConnected,
    /// T3 Code no longer accepts the grant: the owner connects again.
    Unauthorized,
    /// An HTTP 403 that doesn't say the grant was revoked (T3 Code itself
    /// answers a removed, ended or unknown grant with 401): Cupola signs in
    /// again as once the grant ended. Only `post_mcp` returns it, and the
    /// callers in this file turn it into `Unauthorized`.
    Forbidden,
    /// T3 Code is not running: the call never reached it.
    Offline,
    /// The call may have reached T3 Code but its answer was lost (a timeout,
    /// a dropped connection, a server error, an unreadable reply).
    Lost,
    /// T3 Code answered and refused, such as `thread_not_found`.
    Refused {
        code: String,
        message: String,
    },
    /// T3 Code rejected the request before running the tool.
    Protocol(String),
}

impl CallError {
    /// A failed step before the message itself was sent (an upload): T3
    /// Code has nothing to read back, so nothing is unconfirmed.
    fn before_sending(self) -> Self {
        match self {
            Self::Lost => Self::Offline,
            Self::Refused { code, message } if code == "orchestration_error" => Self::Refused {
                code: "upload_failed".into(),
                message,
            },
            error => error,
        }
    }

    /// Whether T3 Code may have acted on the call anyway. Its
    /// `orchestration_error` also covers a failure after the message was
    /// dispatched (reading back the run it started).
    pub(crate) fn unconfirmed(&self) -> bool {
        match self {
            Self::Lost => true,
            Self::Refused { code, .. } => code == "orchestration_error",
            _ => false,
        }
    }
}

/// The providers whose T3 Code threads Cupola follows. Claude and Codex
/// sessions are attached (`native_sessions`) so a launched thread's session
/// joins its chat (`thread_for`, `kept`); OpenCode and Cursor have none to
/// attach, so their chat reads T3 Code's own timeline (`feed`).
pub(crate) fn launch_families() -> Vec<AgentFamily> {
    AgentFamily::ALL.into_iter().collect()
}

/// The T3 Code driver that runs a Cupola provider family.
fn t3_driver(family: AgentFamily) -> &'static str {
    match family {
        AgentFamily::Codex => "codex",
        AgentFamily::Claude => "claudeAgent",
        AgentFamily::Cursor => "cursor",
        AgentFamily::OpenCode => "opencode",
    }
}

/// The models a provider of `orchestrator_capabilities` lists.
fn listed_models(provider: &Value) -> impl Iterator<Item = &str> {
    provider
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
}

/// Project only options the connected T3 instance actually advertises. Keep
/// instance identity in model IDs: two instances may offer the same model.
fn picker_catalog(listed: &Value) -> Value {
    let listed_providers = listed["providers"].as_array();
    let mut providers = Vec::new();
    for provider in listed_providers.into_iter().flatten() {
        let Some(family) = launch_families()
            .into_iter()
            .find(|family| provider["driverKind"] == t3_driver(*family))
        else {
            continue;
        };
        // Missing constraints are not evidence that this provider is usable.
        if !provider["constraints"]
            .as_array()
            .is_some_and(Vec::is_empty)
        {
            continue;
        }
        let Some(instance) = provider["providerInstanceId"].as_str() else {
            continue;
        };
        let same_driver = listed_providers
            .into_iter()
            .flatten()
            .filter(|other| other["driverKind"] == provider["driverKind"])
            .count();
        let mut models = Vec::new();
        for model in provider["models"].as_array().into_iter().flatten() {
            let Some(id) = model["id"].as_str() else {
                continue;
            };
            let option = |name: &str| {
                model["options"]
                    .as_array()?
                    .iter()
                    .find(|option| option["id"] == name)
            };
            let effort = option(model_option_keys(Some(family)).0);
            let (tiers, default_tier) = if let Some(tier) = option("serviceTier") {
                (picker_choices(Some(tier)), tier["default"].clone())
            } else if let Some(fast) = option("fastMode") {
                let tiers = fast["values"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_bool)
                    .map(|enabled| {
                        json!({
                            "id": if enabled { "fast" } else { "default" },
                            "label": if enabled { "Fast" } else { "Standard" },
                            "description": null,
                        })
                    })
                    .collect();
                let default = if fast["default"] == true {
                    "fast"
                } else {
                    "default"
                };
                (tiers, json!(default))
            } else {
                (vec![], Value::Null)
            };
            // The same naming rule as Cupola's own catalog, so T3 Code's
            // slug-like labels read "Claude Opus 5.5", never "claude-opus-5-5".
            let label = crate::model_names::display_name(family, id, model["label"].as_str());
            let label = if same_driver > 1 {
                format!(
                    "{label} ({})",
                    provider["displayName"].as_str().unwrap_or(instance)
                )
            } else {
                label
            };
            models.push(json!({
                "id": format!("t3:{instance}:{id}"),
                "agentFamily": family,
                "displayName": label,
                "hidden": false,
                "reasoningEfforts": picker_choices(effort),
                "defaultReasoningEffort": effort.map(|value| &value["default"]),
                "serviceTiers": tiers,
                "defaultServiceTier": default_tier,
            }));
        }
        if models.is_empty() {
            continue;
        }
        let mut runtime_modes: Vec<_> = provider["runtimeModes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|mode| {
                mode.as_str().is_some_and(|mode| {
                    [
                        "approval-required",
                        "auto-accept-edits",
                        "auto",
                        "full-access",
                    ]
                    .contains(&mode)
                })
            })
            .cloned()
            .collect();
        // T3 treats a provider that lists no supported modes as accepting
        // any (it keeps the requested mode), so absence is not "none".
        if runtime_modes.is_empty() && provider_accepts_any_mode(provider) {
            runtime_modes = [
                "approval-required",
                "auto-accept-edits",
                "auto",
                "full-access",
            ]
            .map(|mode| json!(mode))
            .to_vec();
        }
        providers.push(json!({
            "instanceId": instance,
            "family": family,
            "name": provider["displayName"].as_str().unwrap_or(instance),
            "omittedModels": provider["omittedModels"].as_u64().unwrap_or(0),
            "runtimeModes": runtime_modes,
            "models": models,
        }));
    }
    json!({"providers": providers})
}

/// T3 keeps any requested runtime mode for a provider whose supported-mode
/// list is missing or empty.
fn provider_accepts_any_mode(provider: &Value) -> bool {
    provider["runtimeModes"]
        .as_array()
        .is_none_or(Vec::is_empty)
}

/// Labels for T3's string-valued model options, in their advertised order.
fn picker_choices(descriptor: Option<&Value>) -> Vec<Value> {
    descriptor
        .and_then(|value| value["values"].as_array())
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|id| {
            let label = if id == "default" {
                "Standard".to_owned()
            } else {
                let mut label = id.to_owned();
                if let Some(first) = label.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                label
            };
            json!({"id": id, "label": label, "description": null})
        })
        .collect()
}

/// Where a launched T3 thread works: a folder T3 Code has as a project, or
/// T3 Code's own "No project" scratch folder (`scratch: true`), which T3
/// Code makes and names itself.
#[derive(Clone, Copy)]
pub(crate) enum LaunchWorkspace<'a> {
    Folder(&'a str),
    Scratch,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct LaunchChoices<'a> {
    pub instance: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub tier: Option<&'a str>,
    pub runtime_mode: Option<&'a str>,
}

/// Validate again at launch: a stale phone catalog never authorizes a value
/// that T3 no longer offers. All checks precede the thread-creating call.
fn launch_choices(
    listed: &Value,
    selection: &mut Value,
    choices: &LaunchChoices<'_>,
) -> Result<(), CallError> {
    let refused = || CallError::Refused {
        code: "selection_unavailable".into(),
        message: "The T3 Code choices changed. Reopen the model picker and choose again.".into(),
    };
    let provider = listed["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| {
            p["providerInstanceId"] == selection["instanceId"]
                && p["constraints"].as_array().is_some_and(Vec::is_empty)
        })
        .ok_or_else(refused)?;
    let model = provider["models"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["id"] == selection["model"])
        .ok_or_else(refused)?;
    if let Some(mode) = choices.runtime_mode {
        if !provider_accepts_any_mode(provider)
            && !provider["runtimeModes"]
                .as_array()
                .is_some_and(|modes| modes.iter().any(|v| v == mode))
        {
            return Err(refused());
        }
    }
    let chosen = Chosen {
        family: launch_families()
            .into_iter()
            .find(|family| provider["driverKind"] == t3_driver(*family)),
        effort: choices.effort.map(str::to_owned),
        service_tier: choices.tier.map(str::to_owned),
        ..Chosen::default()
    };
    if model_option_keys(chosen.family).1 == "fastMode"
        && choices
            .tier
            .is_some_and(|tier| !["default", "fast"].contains(&tier))
    {
        return Err(refused());
    }
    for (mut id, mut value) in chosen_options(&chosen) {
        // Codex catalogs may advertise boolean fastMode rather than serviceTier.
        if id == "serviceTier"
            && !model["options"]
                .as_array()
                .is_some_and(|opts| opts.iter().any(|o| o["id"] == id))
        {
            if ![Some("default"), Some("fast")].contains(&choices.tier) {
                return Err(refused());
            }
            id = "fastMode";
            value = json!(choices.tier == Some("fast"));
        }
        let descriptor = model["options"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|o| o["id"] == id)
            .ok_or_else(refused)?;
        if !descriptor["values"]
            .as_array()
            .is_some_and(|values| values.contains(&value))
        {
            return Err(refused());
        }
        set_option(selection, id, value);
    }
    Ok(())
}

/// A fallback launch's `modelSelection` on T3 Code's provider for `family`:
/// `model` when that provider lists it, otherwise its first model.
fn model_selection(
    capabilities: &Value,
    family: AgentFamily,
    model: Option<&str>,
) -> Option<Value> {
    let driver = t3_driver(family);
    capabilities
        .get("providers")
        .and_then(Value::as_array)?
        .iter()
        .filter(|provider| provider.get("driverKind").and_then(Value::as_str) == Some(driver))
        .find_map(|provider| {
            let instance = provider.get("providerInstanceId").and_then(Value::as_str)?;
            let chosen = match model {
                Some(model) if listed_models(provider).any(|id| id == model) => model,
                _ => listed_models(provider).next()?,
            };
            Some(json!({"instanceId": instance, "model": chosen}))
        })
}

/// The first model of the first provider T3 Code lists.
fn any_model_selection(capabilities: &Value) -> Option<Value> {
    capabilities
        .get("providers")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|provider| {
            let instance = provider.get("providerInstanceId").and_then(Value::as_str)?;
            let model = listed_models(provider).next()?;
            Some(json!({"instanceId": instance, "model": model}))
        })
}

/// Opens T3 Code's approval page in the Mac's browser. The launch is awaited
/// by `connect_attempt` with no lock held, and dropped (so stopped) when a
/// newer Connect or a Disconnect replaces the attempt.
pub type UrlOpener = Arc<
    dyn Fn(String) -> futures_util::future::BoxFuture<'static, Result<(), String>> + Send + Sync,
>;

/// An opener that finishes at once: `open` runs and returns, never waiting.
fn immediate_opener(
    open: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static,
) -> UrlOpener {
    Arc::new(move |url| {
        let result = open(&url);
        Box::pin(std::future::ready(result))
    })
}

/// Issues a one-time T3 Code pairing code on this Mac that carries
/// `ACCESS_SCOPES`, as `t3 auth pairing create` does.
pub type CodeIssuer = Arc<
    dyn Fn() -> futures_util::future::BoxFuture<'static, Result<String, PairingError>>
        + Send
        + Sync,
>;

/// Fixed, secret-free failure classes shared by the command, log and phone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingError {
    NoHome,
    NoBinary,
    Timeout,
    CliError,
    MissingDecision,
    Transport,
    RefusedPost,
    ServerBusy,
    InvalidResponse,
    Redirect,
    StateMismatch,
    Finish,
    Setup,
}

impl PairingError {
    fn class(self) -> &'static str {
        match self {
            Self::NoHome => "no_home",
            Self::NoBinary => "no_binary",
            Self::Timeout => "timeout",
            Self::CliError => "cli_error",
            Self::MissingDecision => "missing_decision",
            Self::Transport => "transport",
            Self::RefusedPost => "refused_post",
            Self::ServerBusy => "server_busy",
            Self::InvalidResponse => "invalid_response",
            Self::Redirect => "redirect",
            Self::StateMismatch => "state_mismatch",
            Self::Finish => "finish",
            Self::Setup => "setup",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::NoHome => "Cupola couldn’t find T3 Code’s data folder on this computer.",
            Self::NoBinary => "T3 Code’s command isn’t installed on this computer. Open T3 Code to install it, then try again.",
            Self::Timeout => "T3 Code took too long to approve Cupola on this computer.",
            Self::CliError => "T3 Code’s command couldn’t issue an approval code. Update or reopen T3 Code, then try again.",
            Self::MissingDecision => "This version of T3 Code doesn’t support approval from Cupola. Update T3 Code or approve in its browser.",
            Self::Transport => "Cupola couldn’t reach T3 Code to approve the connection.",
            Self::RefusedPost => "T3 Code refused Cupola’s approval request. Try again or approve in its browser.",
            Self::ServerBusy => "T3 Code is temporarily unable to approve Cupola.",
            Self::InvalidResponse => "T3 Code returned an unreadable approval reply. Update or reopen T3 Code, then try again.",
            Self::Redirect => "T3 Code returned an unexpected approval address. Cupola ignored it. Update T3 Code and try again.",
            Self::StateMismatch => "T3 Code’s approval didn’t match this connection. Cupola ignored it. Try again.",
            Self::Finish => "Cupola couldn’t finish signing in to T3 Code. Try again.",
            Self::Setup => "Cupola couldn’t prepare approval with T3 Code. Reopen T3 Code and try again.",
        }
    }

    fn transient(self) -> bool {
        matches!(self, Self::Timeout | Self::Transport | Self::ServerBusy)
    }
}

/// Revokes one of T3 Code's sessions on this Mac, as `t3 auth session
/// revoke` does: the row leaves T3 Code's Authorized clients and its token
/// stops working.
pub type SessionRevoker = Arc<
    dyn Fn(String) -> futures_util::future::BoxFuture<'static, Result<(), String>> + Send + Sync,
>;

/// What T3 Code names its command in `<T3 home>/bin`: a Windows install has an
/// executable or a `.cmd` launcher, not a bare `t3`.
#[cfg(windows)]
const CLI_NAMES: &[&str] = &["t3.exe", "t3.cmd", "t3"];
#[cfg(not(windows))]
const CLI_NAMES: &[&str] = &["t3"];

/// T3 Code's command under `home`, when it is installed.
fn cli_path(home: &Path) -> Option<PathBuf> {
    first_file(&home.join("bin"), CLI_NAMES)
}

fn first_file(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

/// T3 Code's home: `T3CODE_HOME`, else `.t3` in the user's home directory
/// (`HOME`, or `USERPROFILE`, which a Windows account sets instead).
fn default_home(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    var("T3CODE_HOME").map(PathBuf::from).or_else(|| {
        ["HOME", "USERPROFILE"]
            .into_iter()
            .find_map(|name| var(name).filter(|value| !value.is_empty()))
            .map(|home| PathBuf::from(home).join(".t3"))
    })
}

/// Opens `url` in the user's default browser.
fn open_in_browser(url: String) -> futures_util::future::BoxFuture<'static, Result<(), String>> {
    #[cfg(target_os = "macos")]
    let mut command = tokio::process::Command::new("open");
    // `start` would pass the URL through cmd.exe, which reads `&` as a command.
    #[cfg(windows)]
    let mut command = {
        let mut command = tokio::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut command = tokio::process::Command::new("xdg-open");
    command.arg(url);
    Box::pin(run_launcher(command, OPEN_DEADLINE))
}

/// Runs the browser launcher `command` for at most `deadline`. One that has
/// not exited by then is killed and reaped and the launch fails; dropping
/// this future (a cancelled launch) kills it too, so no launcher outlives
/// the attempt that started it.
async fn run_launcher(
    mut command: tokio::process::Command,
    deadline: Duration,
) -> Result<(), String> {
    const FAILED: &str = "Cupola couldn't open a browser on this computer.";
    command.kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| FAILED.to_owned())?;
    match tokio::time::timeout(deadline, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(_) => Err(FAILED.to_owned()),
        Err(_) => {
            // `kill` waits for the exit, which reaps it.
            let _ = tokio::time::timeout(REAP_WAIT, child.kill()).await;
            Err(FAILED.to_owned())
        }
    }
}

/// T3 Code's own command revoking a session (`t3 auth session revoke ID`).
fn cli_revoker(home: &Path) -> Option<SessionRevoker> {
    let cli = cli_path(home)?;
    let home = home.to_owned();
    Some(Arc::new(move |session: String| {
        let (cli, home) = (cli.clone(), home.clone());
        Box::pin(async move {
            let output = tokio::time::timeout(
                Duration::from_secs(30),
                tokio::process::Command::new(&cli)
                    .args(["auth", "session", "revoke", &session])
                    .env("T3CODE_HOME", &home)
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| "T3 Code's command didn't answer.".to_owned())?
            .map_err(|_| "T3 Code's command couldn't run.".to_owned())?;
            if output.status.success() {
                Ok(())
            } else {
                Err("T3 Code's command didn't remove the old sign-in.".into())
            }
        })
    }))
}

/// T3 Code's own command on this Mac, which T3 Code installs at
/// `<T3 home>/bin/t3` (`t3.exe` or `t3.cmd` on Windows), issuing a pairing code good for five minutes.
fn resolving_cli_issuer(home: PathBuf) -> CodeIssuer {
    Arc::new(move || {
        // T3 Code may install its command after the daemon started. Retry must
        // discover it then, while in-memory/test instances never read this home.
        let issue = cli_issuer(&home);
        Box::pin(async move { issue.ok_or(PairingError::NoBinary)?().await })
    })
}

fn cli_issuer(home: &Path) -> Option<CodeIssuer> {
    cli_issuer_with_timeout(home, Duration::from_secs(30))
}

fn cli_issuer_with_timeout(home: &Path, timeout: Duration) -> Option<CodeIssuer> {
    let cli = cli_path(home)?;
    let home = home.to_owned();
    Some(Arc::new(move || {
        let (cli, home) = (cli.clone(), home.clone());
        Box::pin(async move {
            let mut command = tokio::process::Command::new(&cli);
            command.args([
                "auth", "pairing", "create", "--ttl", "5m", "--label", "Cupola", "--json",
            ]);
            for scope in ACCESS_SCOPES {
                command.args(["--scope", scope]);
            }
            let output = tokio::time::timeout(
                timeout,
                command
                    .env("T3CODE_HOME", &home)
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| PairingError::Timeout)?
            .map_err(|_| PairingError::CliError)?;
            if !output.status.success() {
                return Err(PairingError::CliError);
            }
            pairing_credential(&output.stdout).ok_or(PairingError::CliError)
        })
    }))
}

/// The code in `t3 auth pairing create --json`'s answer.
fn pairing_credential(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let start = text.find('{')?;
    let issued: Value = serde_json::from_str(text[start..].trim()).ok()?;
    issued
        .get("credential")
        .and_then(Value::as_str)
        .filter(|code| !code.trim().is_empty())
        .map(str::to_owned)
}

struct Inner {
    /// `None` until the store is first read.
    grant: Option<Option<Grant>>,
    /// When the store last could not be read (it is read again later).
    unreadable: Option<Instant>,
    expired: bool,
    /// The renewal running now, or when the next may start.
    renewing: Option<tokio::task::AbortHandle>,
    renew_after: Option<Instant>,
    /// Bumped by Disconnect: a sign-in started before keeps nothing.
    generation: u64,
    /// Why messages to each conversation last did not go through T3 Code,
    /// so a change is logged once.
    reasons: HashMap<String, &'static str>,
    error: Option<String>,
    pairing_failure: Option<PairingError>,
    waiting: Option<(u64, tokio::task::AbortHandle)>,
    /// The browser launch running now (`connect_attempt`); a newer Connect
    /// or a Disconnect stops it.
    opening: Option<tokio::task::AbortHandle>,
    attempt: u64,
    /// T3 Code's MCP session for the current grant.
    session: Option<(String, String)>,
    /// Native session or thread ID -> its T3 thread lookup.
    lookups: HashMap<String, Lookup>,
    /// Bumped when the grant changes: a lookup started before keeps nothing.
    epoch: u64,
    /// `LOOKUP_TIMEOUT` and `LOOKUP_BACKOFF` (shorter in tests).
    lookup_timeout: Duration,
    lookup_backoff: Duration,
    failed: HashMap<String, Instant>,
    /// When a call last found T3 Code not running (cleared by an answer).
    offline: Option<Instant>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            grant: None,
            unreadable: None,
            expired: false,
            renewing: None,
            renew_after: None,
            generation: 0,
            reasons: HashMap::new(),
            error: None,
            pairing_failure: None,
            waiting: None,
            opening: None,
            attempt: 0,
            session: None,
            lookups: HashMap::new(),
            epoch: 0,
            lookup_timeout: LOOKUP_TIMEOUT,
            lookup_backoff: LOOKUP_BACKOFF,
            failed: HashMap::new(),
            offline: None,
        }
    }
}

/// What is known of the T3 thread behind one Claude session or Codex thread.
#[derive(Default)]
struct Lookup {
    /// The last finished lookup: the T3 thread (`None`: none matched), when.
    found: Option<(Option<String>, Instant)>,
    /// Lookups failed in a row, and when the next may start.
    failures: u32,
    retry_at: Option<Instant>,
    /// A lookup runs now (one per thread).
    running: bool,
}

impl Lookup {
    /// The matched T3 thread, while the match is fresh enough to use.
    fn thread(&self) -> Option<String> {
        match &self.found {
            Some((Some(thread), at)) if at.elapsed() < MATCH_TTL => Some(thread.clone()),
            _ => None,
        }
    }

    /// Whether another lookup should start now.
    fn due(&self) -> bool {
        if self.running || self.retry_at.is_some_and(|at| Instant::now() < at) {
            return false;
        }
        match &self.found {
            Some((Some(_), at)) => at.elapsed() >= MATCH_REFRESH,
            Some((None, at)) => at.elapsed() >= UNMATCHED_RETRY,
            None => true,
        }
    }
}

/// Aborts its task when dropped: a cancelled Connect stops its launcher.
struct StopOnDrop(tokio::task::AbortHandle);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct T3Code {
    grants: Arc<dyn GrantStore>,
    /// A fixed server (tests, `WONDER_T3CODE_ORIGIN`); otherwise the one
    /// T3 Code's runtime file names.
    origin: Option<String>,
    home: Option<PathBuf>,
    open_url: UrlOpener,
    /// Approves Cupola without the browser (`approve_here`), when T3 Code's
    /// command is installed.
    issue_code: Option<CodeIssuer>,
    /// Revokes T3 Code's session behind a grant Cupola no longer keeps.
    revoke_session: Option<SessionRevoker>,
    /// Where renewals and refusals are recorded (the daemon's log), once a
    /// caller with the app state hands it over.
    log: std::sync::OnceLock<Arc<crate::logging::JsonlLogger>>,
    http: reqwest::Client,
    inner: Mutex<Inner>,
    /// The T3 threads whose questions the phone answers (`questions`).
    pub(crate) questions: questions::Questions,
    /// T3 Code's thread titles, as last read from its store (`titles`).
    titles: titles::Titles,
    /// The conversations whose T3 queue is read (`queue`).
    pub(crate) queue: queue::Checks,
    /// Tests: runs as `renew_if_due` begins, before it takes the lock.
    #[cfg(test)]
    before_renewal: Mutex<Option<BeforeRenewal>>,
    /// Tests: the most bytes of one response `read_body` has held at once.
    #[cfg(test)]
    body_peak: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
type BeforeRenewal = Box<dyn FnOnce(&T3Code) + Send>;

impl T3Code {
    pub fn new(grants: Arc<dyn GrantStore>, origin: Option<String>, open_url: UrlOpener) -> Self {
        let home = default_home(|name| std::env::var_os(name));
        Self {
            grants,
            origin,
            home,
            open_url,
            issue_code: None,
            revoke_session: None,
            log: std::sync::OnceLock::new(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            inner: Mutex::default(),
            questions: questions::Questions::default(),
            titles: titles::Titles::default(),
            queue: queue::Checks::default(),
            #[cfg(test)]
            before_renewal: Mutex::default(),
            #[cfg(test)]
            body_peak: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The installed daemon: the grant in the login Keychain, the approval
    /// page opened in the default browser.
    pub fn keychain(data_home: &Path) -> Self {
        let origin = std::env::var("WONDER_T3CODE_ORIGIN").ok();
        let t3 = Self::new(
            Arc::new(KeychainGrantStore::for_data_home(data_home)),
            origin,
            Arc::new(open_in_browser),
        );
        let issue_code = t3.home.clone().map(resolving_cli_issuer);
        let revoke_session = t3.home.as_deref().and_then(cli_revoker);
        Self {
            issue_code,
            revoke_session,
            ..t3
        }
    }

    /// Revokes superseded sessions with `revoke` (T3 Code's command).
    pub fn with_session_revoker(self, revoke: SessionRevoker) -> Self {
        Self {
            revoke_session: Some(revoke),
            ..self
        }
    }

    /// Records renewals and refusals in the daemon's log from now on.
    pub(crate) fn log_to(&self, logger: &Arc<crate::logging::JsonlLogger>) {
        let _ = self.log.set(logger.clone());
    }

    fn record(&self, event: &str, fields: Value) {
        if let Some(log) = self.log.get() {
            let _ = log.record("info", event, fields);
        }
    }

    /// Revokes T3 Code's session `session` (best effort: the result says
    /// whether T3 Code's command confirmed it).
    async fn revoke(&self, session: Option<String>, why: &'static str) -> Result<(), String> {
        let Some(session) = session else {
            return Err("Cupola doesn't know which T3 Code sign-in this was.".into());
        };
        let Some(revoke) = self.revoke_session.clone() else {
            return Err("T3 Code's command isn't installed on this computer.".into());
        };
        let result = revoke(session).await;
        self.record(
            "t3_code_session_revoked",
            json!({"why": why, "ok": result.is_ok()}),
        );
        result
    }

    #[cfg(test)]
    /// Approves Cupola with codes from `issue` instead of the browser.
    pub fn with_code_issuer(self, issue: CodeIssuer) -> Self {
        Self {
            issue_code: Some(issue),
            ..self
        }
    }

    pub fn in_memory(origin: Option<String>, open_url: UrlOpener) -> Self {
        Self::new(Arc::new(MemoryGrantStore::default()), origin, open_url)
    }

    /// Tests and diagnostics: never opens anything, never finds a server.
    pub fn disabled() -> Self {
        let mut t3 = Self::in_memory(None, immediate_opener(|_| Err("disabled".into())));
        t3.home = None;
        t3
    }

    /// The saved grant. A store that can't be read (a locked Keychain) is
    /// not taken as no grant for good: it is read again a while later.
    fn grant(&self) -> Option<Grant> {
        self.grant_in(&mut self.inner.lock().unwrap())
    }

    /// `grant` under a lock the caller already holds, so what it decides
    /// from the grant and `Inner::generation` is one moment's.
    fn grant_in(&self, inner: &mut Inner) -> Option<Grant> {
        if inner.grant.is_none()
            && inner
                .unreadable
                .is_none_or(|at| at.elapsed() >= UNREADABLE_RETRY)
        {
            match self.grants.read() {
                Ok(read) => {
                    inner.grant = Some(read.and_then(|text| serde_json::from_str(&text).ok()));
                    inner.unreadable = None;
                }
                Err(_) => inner.unreadable = Some(Instant::now()),
            }
        }
        inner.grant.clone().flatten()
    }

    /// A grant T3 Code should still accept.
    pub(crate) fn connected(&self) -> bool {
        let grant = self.grant();
        let inner = self.inner.lock().unwrap();
        !inner.expired
            && grant.is_some_and(|grant| !grant.revoked && grant.expires_at_ms > now_ms())
    }

    /// The running T3 Code server's origin, from its runtime file.
    fn server(&self) -> Option<String> {
        if let Some(origin) = &self.origin {
            return Some(origin.clone());
        }
        let runtime = self.home.as_ref()?.join("userdata/server-runtime.json");
        let runtime: Value = serde_json::from_slice(&std::fs::read(runtime).ok()?).ok()?;
        let pid = runtime.get("pid").and_then(Value::as_i64)?;
        if !crate::platform::process_alive(i32::try_from(pid).ok()?) {
            return None;
        }
        let origin = runtime.get("origin").and_then(Value::as_str)?;
        loopback(origin).then(|| origin.trim_end_matches('/').to_owned())
    }

    fn installed(&self) -> bool {
        self.origin.is_some() || self.home.as_ref().is_some_and(|home| home.is_dir())
    }

    /// Where T3 Code keeps the pages its `html_render` tool published, each
    /// with T3's theme bootstrap ahead of the agent's HTML (`tool_media`),
    /// and the files attached to its messages.
    pub(crate) fn attachments_dir(&self) -> Option<PathBuf> {
        Some(self.home.as_ref()?.join("userdata/attachments"))
    }

    pub(crate) fn status(&self) -> Value {
        let grant = self.grant();
        let running = self.server().is_some();
        let inner = self.inner.lock().unwrap();
        let state = if inner.waiting.is_some() {
            "waiting"
        } else {
            match &grant {
                Some(grant)
                    if !inner.expired && !grant.revoked && grant.expires_at_ms > now_ms() =>
                {
                    "connected"
                }
                Some(_) => "expired",
                None => "disconnected",
            }
        };
        // The owner removed Cupola in T3 Code: the phone asks whether to
        // connect again (`state` stays "expired" for phones that predate it).
        let revoked = state == "expired" && grant.as_ref().is_some_and(|grant| grant.revoked);
        let error = inner.error.clone().or_else(|| {
            (state == "disconnected" && inner.unreadable.is_some()).then(|| {
                "Cupola couldn't read its T3 Code sign-in from this computer's Keychain.".to_owned()
            })
        });
        json!({
            "installed": self.installed(),
            "running": running,
            // Authorization does not establish reachability. Expose only a
            // recent failed call, never invent a successful connection probe.
            "reachable": if !running || inner.offline.is_some() {
                Some(false)
            } else { None },
            "state": state,
            "error": error,
            "pairingFailure": inner.pairing_failure.map(PairingError::class),
            // This Mac starts threads in T3 Code (`launch`); older Macs omit it.
            "launches": true,
            // The providers it starts there (`launch_families`).
            "launchProviders": launch_families(),
            "revoked": revoked,
            "expiresAt": grant.and_then(|grant| chrono::DateTime::from_timestamp_millis(grant.expires_at_ms))
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        })
    }

    /// Explicit Settings retry: read capabilities only, with a bounded wait.
    /// Never authorizes or sends a message, and a stale timeout cannot mark a
    /// replacement grant offline.
    async fn check_reachability(&self) {
        if !self.connected() {
            return;
        }
        let epoch = self.inner.lock().unwrap().epoch;
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            self.call("orchestrator_capabilities", json!({})),
        )
        .await;
        if matches!(
            outcome,
            Err(_) | Ok(Err(CallError::Offline | CallError::Lost))
        ) {
            let mut inner = self.inner.lock().unwrap();
            if inner.epoch == epoch {
                inner.offline = Some(Instant::now());
            }
        }
    }

    /// Connect through T3 Code's command; a browser requires an explicit choice.
    pub(crate) async fn connect(self: &Arc<Self>) -> Result<Value, (StatusCode, String)> {
        self.connect_using(false).await
    }

    async fn connect_using(self: &Arc<Self>, browser: bool) -> Result<Value, (StatusCode, String)> {
        let retained_grant = self.connected();
        let result = self.connect_attempt(browser).await;
        // Connect supersedes an in-flight renewal. If it fails, keep renewing
        // the existing grant, including when it expired during the attempt.
        if retained_grant {
            self.renew_if_due();
        }
        result
    }

    async fn connect_attempt(
        self: &Arc<Self>,
        browser: bool,
    ) -> Result<Value, (StatusCode, String)> {
        // A newer Connect or Disconnect owns the result, including after awaits.
        let generation = {
            let mut inner = self.inner.lock().unwrap();
            inner.generation += 1;
            inner.error = None;
            inner.pairing_failure = None;
            if let Some((_, previous)) = inner.waiting.take() {
                previous.abort();
            }
            if let Some(previous) = inner.renewing.take() {
                previous.abort();
            }
            if let Some(previous) = inner.opening.take() {
                previous.abort();
            }
            inner.generation
        };
        if browser {
            let (pending, listener) = self.sign_in(generation).await.inspect_err(|_| {
                self.record(
                    "t3_code_pairing_failed",
                    json!({"class": "setup", "browser": true, "retry": false}),
                );
            })?;
            // The launch is its own task, awaited with no lock held, so
            // status and Disconnect answer meanwhile. A newer Connect or a
            // Disconnect aborts it (`Inner::opening`), as does dropping this
            // attempt (`StopOnDrop`); a launcher that is dropped is killed.
            let launch = {
                let mut inner = self.inner.lock().unwrap();
                if inner.generation != generation {
                    drop(inner);
                    return Ok(self.status());
                }
                let launch = tokio::spawn((self.open_url)(pending.page.to_string()));
                inner.opening = Some(launch.abort_handle());
                launch
            };
            let _stop = StopOnDrop(launch.abort_handle());
            let launched = launch.await;
            {
                // A superseded attempt installs nothing, whatever the launch did.
                let mut inner = self.inner.lock().unwrap();
                if inner.generation != generation {
                    drop(inner);
                    return Ok(self.status());
                }
                inner.opening = None;
            }
            let opened = launched.unwrap_or_else(|_| Err("the launch stopped".to_owned()));
            if opened.is_err() {
                self.record(
                    "t3_code_pairing_failed",
                    json!({"class": "browser_open", "attempt": 1, "retry": false}),
                );
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Cupola couldn’t open approval in this computer’s browser. Try again.".into(),
                ));
            }
            return self.wait_for_browser(generation, pending, listener);
        }
        for attempt in 1..=2 {
            let epoch = self.inner.lock().unwrap().epoch;
            let result = tokio::time::timeout(Duration::from_secs(60), async {
                if self.home.is_none() && self.issue_code.is_none() {
                    return Err(PairingError::NoHome);
                }
                let (pending, _listener) = self
                    .sign_in(generation)
                    .await
                    .map_err(|_| PairingError::Setup)?;
                let issue = self.issue_code.clone().ok_or(if self.home.is_none() {
                    PairingError::NoHome
                } else {
                    PairingError::NoBinary
                })?;
                self.approve_here(&issue, &pending).await
            })
            .await
            .unwrap_or(Err(PairingError::Timeout));
            {
                let mut inner = self.inner.lock().unwrap();
                if inner.generation != generation {
                    drop(inner);
                    return Ok(self.status());
                }
                // finish may already have saved the grant while revoking an old session.
                let result = if inner.epoch != epoch { Ok(()) } else { result };
                match result {
                    Ok(()) => {
                        inner.error = None;
                        inner.pairing_failure = None;
                        inner.renew_after = None;
                        drop(inner);
                        self.record("t3_code_pairing_connected", json!({"attempt": attempt}));
                        return Ok(self.status());
                    }
                    Err(error) => {
                        let retry = attempt == 1 && error.transient();
                        self.record(
                            "t3_code_pairing_failed",
                            json!({"class": error.class(), "attempt": attempt, "retry": retry}),
                        );
                        if !retry {
                            inner.error = Some(error.reason().into());
                            inner.pairing_failure = Some(error);
                            drop(inner);
                            return Ok(self.status());
                        }
                    }
                }
                drop(inner);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            if self.inner.lock().unwrap().generation != generation {
                return Ok(self.status());
            }
        }
        unreachable!("the second attempt always returns")
    }

    fn wait_for_browser(
        self: &Arc<Self>,
        generation: u64,
        pending: Pending,
        listener: tokio::net::TcpListener,
    ) -> Result<Value, (StatusCode, String)> {
        // The lock is held until `waiting` names this attempt, so the task
        // cannot finish (and clear it) first.
        let mut inner = self.inner.lock().unwrap();
        if inner.generation != generation {
            drop(inner);
            return Ok(self.status());
        }
        inner.attempt += 1;
        inner.error = None;
        if let Some((_, previous)) = inner.waiting.take() {
            previous.abort();
        }
        let attempt = inner.attempt;
        let this = self.clone();
        let task = tokio::spawn(async move {
            let result =
                match tokio::time::timeout(APPROVAL_WINDOW, this.approval(listener, pending)).await
                {
                    Ok(result) => result,
                    Err(_) => Err(
                        "Cupola wasn't approved in time. Connect again and approve it in T3 Code."
                            .to_owned(),
                    ),
                };
            let mut inner = this.inner.lock().unwrap();
            if inner.waiting.as_ref().is_some_and(|(id, _)| *id == attempt) {
                inner.waiting = None;
                inner.error = result.err();
            }
        });
        inner.waiting = Some((attempt, task.abort_handle()));
        drop(inner);
        Ok(self.status())
    }

    /// Registers Cupola with the running T3 Code and prepares its approval:
    /// the request and the loopback listener its answer comes back to.
    /// `generation` is the one current when the caller began.
    async fn sign_in(
        &self,
        generation: u64,
    ) -> Result<(Pending, tokio::net::TcpListener), (StatusCode, String)> {
        let origin = self.server().ok_or((
            StatusCode::CONFLICT,
            "T3 Code isn't open on this computer. Open it, then connect.".to_owned(),
        ))?;
        let unexpected = || {
            (
                StatusCode::BAD_GATEWAY,
                "T3 Code didn't answer as expected, so Cupola didn't connect. Update T3 Code and try again.".to_owned(),
            )
        };
        let resource: Value = self
            .get_json(&format!(
                "{origin}/.well-known/oauth-protected-resource/mcp"
            ))
            .await
            .map_err(|_| unexpected())?;
        let issuer = resource
            .pointer("/authorization_servers/0")
            .and_then(Value::as_str)
            .filter(|issuer| same_origin(issuer, &origin))
            .ok_or_else(unexpected)?
            .trim_end_matches('/')
            .to_owned();
        let server: Value = self
            .get_json(&format!("{issuer}/.well-known/oauth-authorization-server"))
            .await
            .map_err(|_| unexpected())?;
        let endpoint = |key: &str| {
            server
                .get(key)
                .and_then(Value::as_str)
                .filter(|url| same_origin(url, &origin))
                .map(str::to_owned)
        };
        let (Some(authorize), Some(token), Some(register)) = (
            endpoint("authorization_endpoint"),
            endpoint("token_endpoint"),
            endpoint("registration_endpoint"),
        ) else {
            return Err(unexpected());
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| unexpected())?;
        let port = listener.local_addr().map_err(|_| unexpected())?.port();
        let redirect = format!("http://127.0.0.1:{port}/t3-code/callback");
        let client = self
            .http
            .post(&register)
            .json(&json!({"client_name": "Cupola", "redirect_uris": [redirect], "token_endpoint_auth_method": "none"}))
            .send()
            .await
            .map_err(|_| unexpected())?;
        let client: Value = self.read_json(client).await.map_err(|_| unexpected())?;
        let client_id = client
            .get("client_id")
            .and_then(Value::as_str)
            .ok_or_else(unexpected)?
            .to_owned();
        let verifier = random();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random();
        let resource_url = resource
            .get("resource")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let request = json!({"response_type": "code", "client_id": client_id,
            "redirect_uri": redirect, "code_challenge": challenge,
            "code_challenge_method": "S256", "state": state, "resource": resource_url});
        let mut page = Url::parse(&authorize).map_err(|_| unexpected())?;
        for key in [
            "response_type",
            "client_id",
            "redirect_uri",
            "code_challenge",
            "code_challenge_method",
            "state",
            "resource",
        ] {
            page.query_pairs_mut()
                .append_pair(key, request[key].as_str().unwrap_or_default());
        }
        let pending = Pending {
            generation,
            origin,
            issuer,
            token,
            client_id,
            redirect,
            verifier,
            state,
            resource: resource_url,
            decision: authorize
                .strip_suffix("/authorize")
                .map(|base| format!("{base}/decision")),
            request,
            page,
        };
        Ok((pending, listener))
    }

    /// Approves Cupola the way T3 Code's approval page does with a pairing
    /// code (its `decision`), with a code T3 Code's own command issues here,
    /// so the owner copies nothing: the owner's tap on Connect, from a
    /// paired phone, is the approval.
    async fn approve_here(
        &self,
        issue: &CodeIssuer,
        pending: &Pending,
    ) -> Result<(), PairingError> {
        let decision = pending
            .decision
            .as_deref()
            .ok_or(PairingError::MissingDecision)?;
        let code = issue().await?;
        if self.inner.lock().unwrap().generation != pending.generation {
            return Err(PairingError::Finish);
        }
        let response = self
            .http
            .post(decision)
            .json(&json!({"authorization": pending.request,
                "decision": {"_tag": "pairing-code", "access": ACCESS, "code": code}}))
            .send()
            .await
            .map_err(|_| PairingError::Transport)?;
        if response.status().is_redirection() {
            return Err(PairingError::Redirect);
        }
        if response.status().is_server_error() || response.status() == StatusCode::TOO_MANY_REQUESTS
        {
            return Err(PairingError::ServerBusy);
        }
        if !response.status().is_success() {
            return Err(PairingError::RefusedPost);
        }
        let answer: Value = self
            .read_json(response)
            .await
            .map_err(|_| PairingError::InvalidResponse)?;
        let address = answer
            .get("redirectTo")
            .and_then(Value::as_str)
            .ok_or(PairingError::InvalidResponse)?;
        let back = Url::parse(address).map_err(|_| PairingError::Redirect)?;
        let mut target = back.clone();
        target.set_query(None);
        if target.as_str() != pending.redirect {
            return Err(PairingError::Redirect);
        }
        let query: HashMap<String, String> = back.query_pairs().into_owned().collect();
        if query.get("state") != Some(&pending.state) {
            return Err(PairingError::StateMismatch);
        }
        self.finish(pending, &query)
            .await
            .map_err(|_| PairingError::Finish)
    }

    /// Signs Cupola in again without the owner (`approve_here`) once the
    /// grant has less than `RENEW_BEFORE_MS` left or T3 Code refused it
    /// after it ended: one renewal at a time, and the next only
    /// `RENEW_RETRY` after one failed. A grant that hasn't ended is renewed
    /// only while T3 Code still takes it (`probe`). No grant (never
    /// connected, or disconnected) and a revoked one (T3 Code refused it
    /// before it ended: the owner removed Cupola there) are the owner's to
    /// make again. Callers ask often; it returns at once.
    pub(crate) fn renew_if_due(self: &Arc<Self>) {
        let Some(issue) = self.issue_code.clone() else {
            return;
        };
        #[cfg(test)]
        if let Some(hook) = self.before_renewal.lock().unwrap().take() {
            hook(self);
        }
        // One lock from reading the grant until `renewing` names this
        // renewal: a Disconnect comes before (no grant, nothing to renew) or
        // after (a new generation, and it stops the task), and the task
        // cannot finish (and clear `renewing`) first.
        let mut inner = self.inner.lock().unwrap();
        let Some(grant) = self.grant_in(&mut inner) else {
            return;
        };
        if grant.revoked {
            return;
        }
        let ended = grant.expires_at_ms <= now_ms();
        let due = inner.expired || grant.expires_at_ms - now_ms() < RENEW_BEFORE_MS;
        if !due
            || inner.renewing.is_some()
            || inner.waiting.is_some()
            || inner.renew_after.is_some_and(|at| Instant::now() < at)
        {
            return;
        }
        let generation = inner.generation;
        let this = self.clone();
        let task = tokio::spawn(async move {
            // Before it ends, T3 Code refuses the grant only once the owner
            // removed Cupola there: that stops renewing (`refused`).
            let probed = if ended {
                Ok(())
            } else {
                this.probe(&grant).await
            };
            let renewed = match probed {
                Ok(()) => match this.sign_in(generation).await {
                    Ok((pending, _listener)) => {
                        this.approve_here(&issue, &pending).await.map_err(|error| {
                            this.record(
                                "t3_code_pairing_failed",
                                json!({"class": error.class(), "renewal": true}),
                            );
                            error.reason().to_owned()
                        })
                    }
                    Err((_, error)) => Err(error),
                },
                Err(CallError::Unauthorized) => Err("revoked".to_owned()),
                Err(_) => Err("T3 Code didn't answer.".to_owned()),
            };
            this.record(
                "t3_code_renewal",
                json!({"ok": renewed.is_ok(), "ended": ended,
                    "revoked": matches!(probed, Err(CallError::Unauthorized))}),
            );
            let mut inner = this.inner.lock().unwrap();
            if inner.generation != generation {
                return;
            }
            inner.renewing = None;
            inner.renew_after = renewed.is_err().then(|| Instant::now() + RENEW_RETRY);
            if renewed.is_ok() {
                inner.error = None;
                inner.pairing_failure = None;
            }
        });
        inner.renewing = Some(task.abort_handle());
    }

    /// Why Cupola isn't connected now, for the phone.
    pub(crate) fn unconnected(&self) -> Unconnected {
        let grant = self.grant();
        let inner = self.inner.lock().unwrap();
        match grant {
            None if inner.unreadable.is_some() => Unconnected::Unreadable,
            None => Unconnected::SignedOut,
            Some(grant) if grant.revoked => Unconnected::Revoked,
            Some(_) if inner.renewing.is_some() => Unconnected::Renewing,
            Some(_) => Unconnected::Expired,
        }
    }

    /// Waits for T3 Code to send the browser back with the owner's answer,
    /// then exchanges the code for a grant.
    async fn approval(
        &self,
        listener: tokio::net::TcpListener,
        pending: Pending,
    ) -> Result<(), String> {
        let query = loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return Err("Cupola stopped waiting for T3 Code.".into());
            };
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") && request.len() < 16384 {
                match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer)).await {
                    Ok(Ok(read)) if read > 0 => request.extend_from_slice(&buffer[..read]),
                    _ => break,
                }
            }
            let line = String::from_utf8_lossy(&request);
            let target = line
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("GET "))
                .and_then(|rest| rest.split(' ').next())
                .unwrap_or_default()
                .to_owned();
            let Some(query) = target.strip_prefix("/t3-code/callback?") else {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                continue;
            };
            let query: HashMap<String, String> = Url::parse(&format!("http://callback/?{query}"))
                .map(|url| url.query_pairs().into_owned().collect())
                .unwrap_or_default();
            if query.get("state") != Some(&pending.state) {
                let _ = stream
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                continue;
            }
            let page = if query.contains_key("code") {
                "Cupola is connected to T3 Code. You can close this tab."
            } else {
                "Cupola isn't connected to T3 Code. You can close this tab."
            };
            let body = format!("<!doctype html><meta charset=utf-8><title>Cupola</title><p style=\"font:17px -apple-system,sans-serif;margin:3em\">{page}</p>");
            let _ = stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
            break query;
        };
        self.finish(&pending, &query).await
    }

    /// Takes T3 Code's answer to this sign-in (the approval page's or
    /// `approve_here`'s): exchanges its code for a grant and keeps it.
    async fn finish(
        &self,
        pending: &Pending,
        query: &HashMap<String, String>,
    ) -> Result<(), String> {
        if let Some(error) = query.get("error") {
            return Err(if error == "access_denied" {
                "Cupola wasn't approved in T3 Code.".into()
            } else {
                "T3 Code didn't approve Cupola. Connect again to retry.".into()
            });
        }
        if query
            .get("iss")
            .is_some_and(|iss| iss.trim_end_matches('/') != pending.issuer)
        {
            return Err(
                "T3 Code's answer came from an unexpected server, so Cupola ignored it.".into(),
            );
        }
        let code = query
            .get("code")
            .ok_or("T3 Code didn't approve Cupola. Connect again to retry.")?;
        let mut form = Url::parse("http://form/").unwrap();
        form.query_pairs_mut()
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", &pending.redirect)
            .append_pair("client_id", &pending.client_id)
            .append_pair("code_verifier", &pending.verifier)
            .append_pair("resource", &pending.resource);
        const UNFINISHED: &str =
            "T3 Code approved Cupola, but its sign-in couldn't be finished. Connect again.";
        let failed = |_: reqwest::Error| UNFINISHED.to_owned();
        let response = self
            .http
            .post(&pending.token)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form.query().unwrap_or_default().to_owned())
            .send()
            .await
            .map_err(failed)?;
        if !response.status().is_success() {
            return Err(UNFINISHED.to_owned());
        }
        let token: Value = self
            .read_json(response)
            .await
            .map_err(|_| UNFINISHED.to_owned())?;
        let access_token = token
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| UNFINISHED.to_owned())?;
        let expires_in = token
            .get("expires_in")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let grant = Grant {
            origin: pending.origin.clone(),
            access_token: access_token.to_owned(),
            expires_at_ms: now_ms() + (expires_in * 1000.0) as i64,
            session_id: token_session(access_token),
            revoked: false,
        };
        // Kept under the lock, so a Disconnect either comes after (and
        // removes it) or before (and this sign-in keeps nothing).
        let superseded = self.keep(pending, grant)?;
        if superseded.is_some() {
            // Best effort: the old grant is forgotten either way, and T3
            // Code ends it when it expires.
            let _ = self.revoke(superseded, "renewed").await;
        }
        Ok(())
    }

    /// `finish` under the lock: keeps `grant` and returns the session of
    /// the one it replaces, which is revoked next.
    fn keep(&self, pending: &Pending, grant: Grant) -> Result<Option<String>, String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.generation != pending.generation {
            return Err("Cupola was disconnected from T3 Code meanwhile.".into());
        }
        // The grant this one replaces: its session is revoked below, so T3
        // Code lists one Cupola (one already refused needs nothing).
        let superseded = inner
            .grant
            .clone()
            .flatten()
            .filter(|old| !old.revoked && old.expires_at_ms > now_ms())
            .and_then(|old| old.session())
            .filter(|old| grant.session().as_ref() != Some(old));
        self.grants.write(&serde_json::to_string(&grant).unwrap())?;
        inner.grant = Some(Some(grant));
        inner.unreadable = None;
        inner.expired = false;
        inner.session = None;
        inner.lookups.clear();
        inner.epoch += 1;
        inner.failed.clear();
        Ok(superseded)
    }

    /// Forgets the grant (and stops waiting for an approval), and revokes
    /// its session in T3 Code, so Cupola leaves T3 Code's Authorized
    /// clients too. When T3 Code's command can't, the phone says where to
    /// remove it.
    pub(crate) async fn disconnect(&self) -> Result<Value, String> {
        let session = self.forget()?;
        if session.is_some() && self.revoke(session, "disconnected").await.is_err() {
            self.inner.lock().unwrap().error = Some(
                "Cupola is disconnected, but T3 Code may still list it. Remove Cupola in T3 Code under Settings → Connections."
                    .into(),
            );
        }
        Ok(self.status())
    }

    /// `disconnect` here: the session of a grant T3 Code still takes, which
    /// it revokes next.
    fn forget(&self) -> Result<Option<String>, String> {
        let mut inner = self.inner.lock().unwrap();
        let session = self
            .grant_in(&mut inner)
            .filter(|grant| !grant.revoked && grant.expires_at_ms > now_ms())
            .and_then(|grant| grant.session());
        // A sign-in or renewal still running keeps nothing (`finish`).
        inner.generation += 1;
        if let Some(task) = inner.opening.take() {
            task.abort();
        }
        if let Some((_, task)) = inner.waiting.take() {
            task.abort();
        }
        if let Some(task) = inner.renewing.take() {
            task.abort();
        }
        self.grants.remove()?;
        *inner = Inner {
            grant: Some(None),
            attempt: inner.attempt,
            generation: inner.generation,
            epoch: inner.epoch + 1,
            lookup_timeout: inner.lookup_timeout,
            lookup_backoff: inner.lookup_backoff,
            ..Inner::default()
        };
        Ok(session)
    }

    async fn get_json(&self, url: &str) -> Result<Value, ()> {
        let response = self.http.get(url).send().await.map_err(|_| ())?;
        if !response.status().is_success() {
            return Err(());
        }
        self.read_json(response).await
    }

    /// The whole body of a T3 Code response, at most `MAX_BODY` bytes.
    async fn read_body(&self, response: reqwest::Response) -> Result<Vec<u8>, BodyError> {
        let mut retained = 0;
        let body = collect_bounded(response.bytes_stream(), MAX_BODY, &mut retained).await;
        #[cfg(test)]
        self.body_peak
            .fetch_max(retained, std::sync::atomic::Ordering::SeqCst);
        body
    }

    /// A T3 Code response's body as JSON (`read_body`).
    async fn read_json(&self, response: reqwest::Response) -> Result<Value, ()> {
        let body = self.read_body(response).await.map_err(|_| ())?;
        serde_json::from_slice(&body).map_err(|_| ())
    }

    fn mcp_url(&self, grant: &Grant) -> String {
        format!(
            "{}/mcp",
            self.server().unwrap_or_else(|| grant.origin.clone())
        )
    }

    async fn post_mcp(
        &self,
        grant: &Grant,
        session: Option<&str>,
        body: &Value,
    ) -> Result<(Option<String>, Option<Value>), CallError> {
        let mut request = self
            .http
            .post(self.mcp_url(grant))
            .bearer_auth(&grant.access_token)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL)
            .json(body);
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        let response = request.send().await.map_err(|error| {
            if error.is_connect() {
                CallError::Offline
            } else {
                CallError::Lost
            }
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CallError::Unauthorized);
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            // An oversized or unreadable refusal does not say it was revoked.
            let revoked = self.read_body(response).await.is_ok_and(|body| {
                String::from_utf8_lossy(&body)
                    .to_ascii_lowercase()
                    .contains("revoked")
            });
            return Err(if revoked {
                CallError::Unauthorized
            } else {
                CallError::Forbidden
            });
        }
        if status.is_server_error() {
            return Err(CallError::Lost);
        }
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let sse = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        if !status.is_success() {
            return Err(CallError::Protocol(format!("HTTP {status}")));
        }
        let bytes = self
            .read_body(response)
            .await
            .map_err(|_| CallError::Lost)?;
        Ok((session, json_rpc_payload(&bytes, sse)))
    }

    async fn mcp_session(&self, grant: &Grant) -> Result<String, CallError> {
        if let Some((token, session)) = &self.inner.lock().unwrap().session {
            if *token == grant.access_token {
                return Ok(session.clone());
            }
        }
        let (session, _) = self
            .post_mcp(
                grant,
                None,
                &json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
                    "protocolVersion": PROTOCOL, "capabilities": {},
                    "clientInfo": {"name": "Cupola", "version": crate::RELEASE_VERSION}}}),
            )
            .await?;
        let session = session.unwrap_or_default();
        let _ = self
            .post_mcp(
                grant,
                Some(&session)
                    .filter(|session| !session.is_empty())
                    .map(String::as_str),
                &json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            )
            .await;
        self.inner.lock().unwrap().session = Some((grant.access_token.clone(), session.clone()));
        Ok(session)
    }

    /// T3 Code refused `grant`. A 401 (or a 403 saying so) before the
    /// grant ends means the owner removed Cupola in T3 Code (T3 Code answers
    /// a revoked and an expired token alike): the grant is kept as revoked,
    /// which outlives a restart, and Cupola doesn't sign itself in again.
    /// Any other refusal (`said_revoked` false: another 403) ends the grant
    /// and Cupola signs in again, as before.
    fn refused(&self, grant: &Grant, said_revoked: bool) {
        let revoked = said_revoked && grant.expires_at_ms > now_ms();
        let mut inner = self.inner.lock().unwrap();
        // A renewal may have replaced it meanwhile.
        let current = inner.grant.clone().flatten();
        if current
            .as_ref()
            .is_none_or(|current| current.access_token != grant.access_token)
        {
            return;
        }
        let first = !inner.expired;
        inner.expired = true;
        inner.session = None;
        if revoked {
            let kept = Grant {
                revoked: true,
                ..grant.clone()
            };
            if self
                .grants
                .write(&serde_json::to_string(&kept).unwrap())
                .is_ok()
            {
                inner.grant = Some(Some(kept));
            }
            inner.error = Some(
                "T3 Code removed Cupola's access. Connect again to send messages through T3 Code."
                    .into(),
            );
        } else {
            inner.error =
                Some("T3 Code no longer accepts Cupola's sign-in. Connect T3 Code again.".into());
        }
        drop(inner);
        if first {
            self.record("t3_code_refused", json!({"revoked": revoked}));
        }
    }

    /// Whether T3 Code still takes `grant`, with the cheapest request it
    /// checks one with (`initialize`).
    async fn probe(&self, grant: &Grant) -> Result<(), CallError> {
        let probed = self
            .post_mcp(
                grant,
                None,
                &json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
                    "protocolVersion": PROTOCOL, "capabilities": {},
                    "clientInfo": {"name": "Cupola", "version": crate::RELEASE_VERSION}}}),
            )
            .await
            .map(|_| ());
        match probed {
            Err(CallError::Unauthorized) => {
                self.refused(grant, true);
                probed
            }
            // Not a revoke: renewing goes ahead.
            Err(CallError::Forbidden) => Ok(()),
            probed => probed,
        }
    }

    /// Calls one of T3 Code's tools and returns its structured result.
    pub(crate) async fn call(&self, tool: &str, arguments: Value) -> Result<Value, CallError> {
        let grant = self
            .grant()
            .filter(|_| self.connected())
            .ok_or(CallError::NotConnected)?;
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}});
        let mut outcome = Err(CallError::Offline);
        for attempt in 0..2 {
            let session = self.mcp_session(&grant).await;
            outcome = match session {
                Ok(session) => {
                    let session = Some(session.as_str()).filter(|session| !session.is_empty());
                    self.post_mcp(&grant, session, &request).await
                }
                // The tool call itself was never sent.
                Err(CallError::Lost) => Err(CallError::Offline),
                Err(error) => Err(error),
            };
            // An expired MCP session (T3 Code restarted) is refused before
            // the tool runs: start another once. Nothing that may have run
            // (`Lost`) is called again.
            if attempt == 0 && matches!(outcome, Err(CallError::Protocol(_))) {
                self.inner.lock().unwrap().session = None;
                continue;
            }
            break;
        }
        self.inner.lock().unwrap().offline = match &outcome {
            Err(CallError::Offline) => Some(Instant::now()),
            _ => None,
        };
        let payload = match outcome {
            Ok((_, payload)) => payload,
            Err(CallError::Unauthorized) => {
                self.refused(&grant, true);
                return Err(CallError::Unauthorized);
            }
            Err(CallError::Forbidden) => {
                self.refused(&grant, false);
                return Err(CallError::Unauthorized);
            }
            Err(error) => return Err(error),
        };
        // T3 Code accepted the request; an unreadable answer hides its result.
        tool_result(payload.ok_or(CallError::Lost)?)
    }

    /// The T3 thread the last lookup found for this native thread (or why
    /// there is none), without waiting for T3 Code, and whether the caller
    /// should start a lookup now (`Some(epoch)`, which this marks running).
    fn known(&self, native: &str) -> (Result<String, Unrouted>, Option<u64>) {
        let mut inner = self.inner.lock().unwrap();
        let epoch = inner.epoch;
        if !inner.lookups.contains_key(native) && inner.lookups.len() >= MAX_LOOKUPS {
            inner.lookups.retain(|_, lookup| {
                lookup.running
                    || lookup.retry_at.is_some_and(|at| Instant::now() < at)
                    || lookup
                        .found
                        .as_ref()
                        .is_some_and(|(_, at)| at.elapsed() < MATCH_TTL)
            });
            if inner.lookups.len() >= MAX_LOOKUPS {
                return (Err(Unrouted::Finding), None);
            }
        }
        let lookup = inner.lookups.entry(native.to_owned()).or_default();
        let start = lookup.due();
        lookup.running |= start;
        let thread = lookup.thread().ok_or(if lookup.failures > 0 {
            Unrouted::Unreachable
        } else if matches!(lookup.found, Some((None, _))) {
            Unrouted::Unmatched
        } else {
            Unrouted::Finding
        });
        (thread, start.then_some(epoch))
    }

    /// Whether `reason` (why messages to this conversation don't go through
    /// T3 Code, `None` when they do) differs from the last one seen.
    fn reason_changed(&self, conversation: &str, reason: Option<&'static str>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let previous = match reason {
            Some(reason) => {
                if !inner.reasons.contains_key(conversation) && inner.reasons.len() >= MAX_LOOKUPS {
                    inner.reasons.clear();
                }
                inner.reasons.insert(conversation.to_owned(), reason)
            }
            None => inner.reasons.remove(conversation),
        };
        previous != reason
    }

    /// Keeps what a lookup found, or backs off after it failed. Returns
    /// whether it found a T3 thread a message can now go into.
    ///
    /// A thread once matched stays matched when a later lookup finds none:
    /// replies Cupola ran while T3 Code was closed are in the session but not
    /// in T3 Code's thread, so its latest message falls behind the
    /// transcript's. T3 Code saying the thread is gone ends it (`settle`).
    fn finish_lookup(
        &self,
        native: &str,
        epoch: u64,
        found: &Result<Option<String>, CallError>,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let backoff = inner.lookup_backoff;
        if inner.epoch != epoch {
            return false;
        }
        let Some(lookup) = inner.lookups.get_mut(native) else {
            return false;
        };
        lookup.running = false;
        match found {
            Ok(thread) => {
                let before = lookup.thread();
                let kept = match (thread, &lookup.found) {
                    (None, Some((Some(earlier), _))) => Some(earlier.clone()),
                    _ => thread.clone(),
                };
                let found = kept.is_some() && before != kept;
                lookup.found = Some((kept, Instant::now()));
                lookup.failures = 0;
                lookup.retry_at = None;
                found
            }
            Err(_) => {
                lookup.failures = lookup.failures.saturating_add(1);
                let wait = backoff
                    .saturating_mul(1 << (lookup.failures - 1).min(16))
                    .min(LOOKUP_BACKOFF_MAX);
                lookup.retry_at = Some(Instant::now() + wait);
                false
            }
        }
    }

    /// The T3 thread that runs this Claude session or Codex thread, matched
    /// as the module comment describes, read from T3 Code now (`route`
    /// keeps the answer). `prompts` are the native transcript's messages
    /// from the owner, oldest first.
    pub(crate) async fn thread_for(
        &self,
        family: AgentFamily,
        cwd: &str,
        prompts: &[String],
    ) -> Result<Option<String>, CallError> {
        let driver = match family {
            AgentFamily::Claude | AgentFamily::Codex => t3_driver(family),
            _ => return Ok(None),
        };
        let capabilities = self.call("orchestrator_capabilities", json!({})).await?;
        let instances: Vec<&str> = capabilities
            .get("providers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|provider| provider.get("driverKind").and_then(Value::as_str) == Some(driver))
            .filter_map(|provider| provider.get("providerInstanceId").and_then(Value::as_str))
            .collect();
        let cwd = cwd.trim_end_matches('/');
        let projects = self.projects().await?;
        // A worktree thread runs outside its project's folder: look in all.
        let roots: HashMap<String, String> = projects.iter().cloned().collect();
        let mut searched: Vec<_> = projects.iter().filter(|(_, root)| root == cwd).collect();
        if searched.is_empty() {
            searched = projects.iter().collect();
        }
        let mut candidates = Vec::new();
        for (project, _) in searched {
            let list = self
                .call(
                    "t3_thread_list",
                    json!({"projectId": project, "includeSubagents": false, "limit": 100}),
                )
                .await?;
            for thread in list
                .get("threads")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let instance = thread.get("providerInstanceId").and_then(Value::as_str);
                if let (Some(id), true) = (
                    thread.get("threadId").and_then(Value::as_str),
                    instance.is_some_and(|instance| instances.contains(&instance)),
                ) {
                    let updated = thread
                        .get("updatedAt")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    candidates.push((updated.to_owned(), id.to_owned()));
                }
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        candidates.truncate(MAX_CANDIDATES);
        let mut matched = Vec::new();
        for (_, thread) in candidates {
            let mut after = None;
            let mut messages = Vec::new();
            for page in 0..MAX_PAGES {
                let read = self.read_page(&thread, after, None).await?;
                if page == 0 {
                    let detail = read.get("thread").cloned().unwrap_or_default();
                    let folder = detail
                        .get("worktreePath")
                        .and_then(Value::as_str)
                        .map(|path| path.trim_end_matches('/').to_owned())
                        .or_else(|| {
                            detail
                                .get("projectId")
                                .and_then(Value::as_str)
                                .and_then(|project| roots.get(project).cloned())
                        });
                    if folder.as_deref() != Some(cwd) {
                        break;
                    }
                }
                let items = read
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for item in &items {
                    if let Some(position) = item.get("position").and_then(Value::as_i64) {
                        after = after.max(Some(position));
                    }
                    if item.get("type").and_then(Value::as_str) == Some("user_message") {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            messages.push(text.to_owned());
                        }
                    }
                }
                if read.get("hasMore").and_then(Value::as_bool) != Some(true) || items.is_empty() {
                    break;
                }
            }
            if same_conversation(&messages, prompts) {
                matched.push(thread);
            }
        }
        Ok((matched.len() == 1).then(|| matched.remove(0)))
    }

    /// T3 Code's own subtasks of `thread`: the listed threads whose
    /// `parentThreadId` is `thread`, each with its `relationshipToParent`.
    /// Searches the T3 project whose folder is `cwd`, else all. Everything,
    /// the project list and the MCP handshake included, stops at `budget`; the
    /// children found by then are returned (`complete` false), not dropped.
    /// A budget spent before anything answered is `CallError::Lost`.
    pub(crate) async fn subtasks(
        &self,
        thread: &str,
        cwd: &str,
        budget: Duration,
    ) -> Result<Subtasks, CallError> {
        let deadline = tokio::time::Instant::now() + budget;
        let projects = tokio::time::timeout_at(deadline, self.projects())
            .await
            .map_err(|_| CallError::Lost)??;
        let cwd = cwd.trim_end_matches('/');
        let mut searched: Vec<_> = projects.iter().filter(|(_, root)| root == cwd).collect();
        if searched.is_empty() {
            searched = projects.iter().collect();
        }
        let mut found = Vec::new();
        let mut failed = None;
        let mut complete = true;
        'projects: for (project, _) in searched {
            // T3 Code lists a project's threads newest first, children among
            // them, 100 a page: a chat's older children are on later pages.
            let mut cursor = Value::Null;
            for _ in 0..MAX_PAGES {
                let mut arguments =
                    json!({"projectId": project, "includeSubagents": true, "limit": 100});
                if !cursor.is_null() {
                    arguments["cursor"] = cursor.clone();
                }
                // One project that fails to list must not drop what others found.
                let list =
                    match tokio::time::timeout_at(deadline, self.call("t3_thread_list", arguments))
                        .await
                    {
                        Ok(Ok(list)) => list,
                        Ok(Err(error)) => {
                            failed.get_or_insert(error);
                            break;
                        }
                        // Out of time: what earlier pages found stands.
                        Err(_) => {
                            complete = false;
                            break 'projects;
                        }
                    };
                found.extend(
                    list.get("threads")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|child| {
                            child.get("parentThreadId").and_then(Value::as_str) == Some(thread)
                        })
                        .cloned(),
                );
                cursor = list.get("nextCursor").cloned().unwrap_or(Value::Null);
                if cursor.is_null() {
                    break;
                }
            }
        }
        // Nothing found and a listing failed: T3 Code did not answer, which is
        // not the same as the chat having no subtasks.
        match failed {
            Some(error) if found.is_empty() => Err(error),
            _ => Ok(Subtasks { found, complete }),
        }
    }

    /// The messages of one T3 thread, oldest first, with its detail from the
    /// first page: what a subtask's sheet shows. Bounded to `MAX_PAGES` pages
    /// of 100.
    pub(crate) async fn thread_messages(
        &self,
        thread: &str,
    ) -> Result<(Value, Vec<Value>), CallError> {
        let mut after = None;
        let mut detail = Value::Null;
        let mut items = Vec::new();
        for page in 0..MAX_PAGES {
            let read = self.read_page(thread, after, Some(50_000)).await?;
            if page == 0 {
                detail = read.get("thread").cloned().unwrap_or_default();
            }
            let batch = read
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in &batch {
                if let Some(position) = item.get("position").and_then(Value::as_i64) {
                    after = after.max(Some(position));
                }
            }
            let empty = batch.is_empty();
            items.extend(batch);
            if read.get("hasMore").and_then(Value::as_bool) != Some(true) || empty {
                break;
            }
        }
        Ok((detail, items))
    }

    /// T3 Code's projects: (ID, folder without a trailing slash).
    pub(crate) async fn projects(&self) -> Result<Vec<(String, String)>, CallError> {
        let mut projects = Vec::new();
        let mut cursor = Value::Null;
        for _ in 0..MAX_PAGES {
            let mut arguments = json!({"limit": 100});
            if !cursor.is_null() {
                arguments["cursor"] = cursor.clone();
            }
            let page = self.call("t3_project_list", arguments).await?;
            projects.extend(
                page.get("projects")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|project| {
                        Some((
                            project.get("id")?.as_str()?.to_owned(),
                            project
                                .get("workspaceRoot")?
                                .as_str()?
                                .trim_end_matches('/')
                                .to_owned(),
                        ))
                    }),
            );
            cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
            if cursor.is_null() {
                break;
            }
        }
        Ok(projects)
    }

    /// A page of the T3 thread's timeline after `after` (from its start
    /// without one: T3 Code takes only a non-negative position).
    /// `chars` raises T3 Code's per-message cap (20 000 by default, 50 000 at most).
    async fn read_page(
        &self,
        thread: &str,
        after: Option<i64>,
        chars: Option<u32>,
    ) -> Result<Value, CallError> {
        let mut arguments = json!({"threadId": thread, "view": "messages", "limit": 100});
        if let Some(chars) = chars {
            arguments["maxCharsPerItem"] = chars.into();
        }
        if let Some(after) = after {
            arguments["afterPosition"] = after.into();
        }
        self.call("t3_thread_read", arguments).await
    }

    /// Where T3 Code has the message Cupola sent with `client_request_id`:
    /// in the thread's timeline, where T3 Code names a message sent through
    /// its MCP `message:mcp:<grant>:thread-send:<clientRequestId>`, or
    /// waiting in the thread's queue, whose entries carry only their text.
    /// For a send whose answer was lost (`projects::reconcile_t3_code`);
    /// T3 Code not answering in time counts as a lost answer.
    /// A message sent with files has no `clientRequestId` in T3 Code: it is
    /// the user message carrying the files T3 Code issued for this send
    /// (`attachments`, their IDs), which no other message can have.
    pub(crate) async fn has_sent(
        &self,
        thread: &str,
        client_request_id: &str,
        text: &str,
        attachments: &[String],
    ) -> Result<Found, CallError> {
        let timeout = self.inner.lock().unwrap().lookup_timeout;
        tokio::time::timeout(
            timeout,
            self.find_sent(thread, client_request_id, text, attachments),
        )
        .await
        .unwrap_or(Err(CallError::Lost))
    }

    async fn find_sent(
        &self,
        thread: &str,
        client_request_id: &str,
        text: &str,
        attachments: &[String],
    ) -> Result<Found, CallError> {
        let sent = Sent {
            message: client_request_id.to_owned(),
            t3_message: None,
            attachments: attachments.to_vec(),
        };
        if self.scan_timeline(thread, |item| sent.is(item)).await? {
            return Ok(Found::Started);
        }
        // A message with files goes in steering or starting a turn, never
        // queued (`send_attachments`): it is in the timeline or not sent.
        if !attachments.is_empty() {
            return Ok(Found::Missing);
        }
        Ok(self
            .queue(thread)
            .await?
            .into_iter()
            .find(|entry| match entry.truncated {
                true => !entry.text.is_empty() && text.starts_with(&entry.text),
                false => entry.text == text,
            })
            .map_or(Found::Missing, |entry| Found::Queued(entry.run)))
    }

    /// Reads the T3 thread's timeline from its start, `visit`ing each item,
    /// until `visit` returns true (then true) or the timeline ends (false).
    async fn scan_timeline(
        &self,
        thread: &str,
        mut visit: impl FnMut(&Value) -> bool,
    ) -> Result<bool, CallError> {
        let mut after = None;
        for _ in 0..MAX_PAGES * 5 {
            let read = self.read_page(thread, after, None).await?;
            let items = read
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in &items {
                if let Some(position) = item.get("position").and_then(Value::as_i64) {
                    after = after.max(Some(position));
                }
                if visit(item) {
                    return Ok(true);
                }
            }
            if read.get("hasMore").and_then(Value::as_bool) != Some(true) || items.is_empty() {
                return Ok(false);
            }
        }
        Err(CallError::Protocol("thread too long to read".into()))
    }

    /// Which of `sent` (Wonder message IDs) T3 Code has started in the T3
    /// thread: those its timeline has. One waiting in its queue isn't there
    /// yet. Bounded, since a history refresh waits for it.
    async fn started(&self, thread: &str, sent: &[&Sent]) -> Result<Vec<String>, CallError> {
        let timeout = self
            .inner
            .lock()
            .unwrap()
            .lookup_timeout
            .min(Duration::from_secs(5));
        let mut started = Vec::new();
        let scan = self.scan_timeline(thread, |item| {
            started.extend(
                sent.iter()
                    .filter(|sent| sent.is(item))
                    .map(|sent| sent.message.clone()),
            );
            false
        });
        tokio::time::timeout(timeout, scan)
            .await
            .unwrap_or(Err(CallError::Lost))?;
        Ok(started)
    }

    /// Sends the owner's message into the T3 thread, plain (no note that it
    /// came from Cupola). Text alone goes with `t3_thread_send` in `mode`;
    /// `client_request_id` is the Wonder message: T3 Code keys the command by
    /// it and by Cupola's grant, so the same request is never applied twice
    /// under one grant. Steer or restart into a thread whose turn already
    /// ended is queued instead, so it starts the next turn. Files go as
    /// `attachments` (from `upload`) with `t3_thread_send_attachments`,
    /// which has no mode (T3 Code steers a running turn, otherwise starts
    /// one; a restart first stops the turn, `stop_for_restart`, and a queued send waits in
    /// Cupola for the turn to end, `projects::deliver_through_t3_code`) and
    /// no retry key: a lost answer is read back by the attachments' IDs
    /// (`has_sent`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send(
        &self,
        native: &str,
        thread: &str,
        text: &str,
        client_request_id: &str,
        mode: SendMode,
        attachments: &[Value],
        chosen: Option<&Chosen>,
    ) -> Result<Value, CallError> {
        // The message's model or effort differs from T3 Code's thread: it
        // switches first, and the message goes with it. Guidance for the reply that
        // runs now can't change its model, so it goes after that reply.
        let mode = match chosen {
            Some(chosen) => match self.configure(thread, chosen).await {
                Ok(true) if mode == SendMode::Steer => SendMode::Queue,
                Ok(_) => mode,
                Err(error) => {
                    let error = error.before_sending();
                    self.settle(native, &Err(error.clone()));
                    return Err(error);
                }
            },
            None => mode,
        };
        let result = if attachments.is_empty() {
            let send = |mode: SendMode| {
                self.call(
                    "t3_thread_send",
                    json!({"threadId": thread, "message": text, "mode": mode.as_str(), "clientRequestId": client_request_id}),
                )
            };
            match send(mode).await {
                Err(CallError::Refused { code, .. })
                    if code == "thread_not_sendable" && mode != SendMode::Queue =>
                {
                    send(SendMode::Queue).await
                }
                result => result,
            }
        } else {
            let interrupted = match mode {
                SendMode::Restart => self.stop_for_restart(thread, client_request_id).await,
                _ => Ok(()),
            };
            match interrupted {
                Ok(()) => {
                    let mut arguments = json!({"threadId": thread, "attachments": attachments});
                    if !text.is_empty() {
                        arguments["message"] = text.into();
                    }
                    self.call("t3_thread_send_attachments", arguments).await
                }
                Err(error) => Err(error),
            }
        };
        self.settle(native, &result);
        result
    }

    /// Sets the T3 thread's model and options to what the phone chose, only
    /// where T3 Code's own configuration differs (`t3_thread_configure`
    /// reads when `modelSelection` is omitted; it has no retry key, so
    /// reading first keeps a retry from switching twice), then reads it back.
    /// True when it changed. A model T3 Code doesn't offer this thread's provider is
    /// refused before anything changes.
    async fn configure(&self, thread: &str, chosen: &Chosen) -> Result<bool, CallError> {
        let read = |thread: &str| {
            let arguments = json!({"threadId": thread});
            async move {
                let configured = self.call("t3_thread_configure", arguments).await?;
                configured
                    .get("modelSelection")
                    .filter(|selection| selection.is_object())
                    .cloned()
                    .ok_or_else(|| CallError::Protocol("no modelSelection".into()))
            }
        };
        let current = read(thread).await?;
        let mut wanted = current.clone();
        let already = chosen
            .models
            .iter()
            .any(|model| current["model"] == model.as_str());
        if !chosen.models.is_empty() && !already {
            let instance = current["instanceId"].as_str().unwrap_or_default();
            let capabilities = self.call("orchestrator_capabilities", json!({})).await?;
            let offered: Vec<String> = capabilities
                .get("providers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|provider| provider["providerInstanceId"] == instance)
                .flat_map(|provider| listed_models(provider).map(str::to_owned))
                .collect();
            match chosen.models.iter().find(|model| offered.contains(model)) {
                Some(model) => wanted["model"] = json!(model),
                None if chosen.own_model => {}
                None => {
                    return Err(CallError::Refused {
                        code: "model_unavailable".into(),
                        message: "T3 Code doesn't offer the chosen model for this thread.".into(),
                    })
                }
            }
        }
        for (id, value) in chosen_options(chosen) {
            set_option(&mut wanted, id, value);
        }
        let applied = |selection: &Value| {
            selection["model"] == wanted["model"]
                && chosen_options(chosen)
                    .iter()
                    .all(|(id, value)| option_value(selection, id) == Some(value))
        };
        if applied(&current) {
            return Ok(false);
        }
        self.call(
            "t3_thread_configure",
            json!({"threadId": thread, "modelSelection": wanted}),
        )
        .await?;
        if applied(&read(thread).await?) {
            Ok(true)
        } else {
            Err(CallError::Refused {
                code: "configure_not_applied".into(),
                message: "T3 Code didn't switch this thread to the chosen model.".into(),
            })
        }
    }

    /// Take over with files: T3 Code's send with files has no restart, so
    /// the running turn is interrupted and the files go only once T3 Code
    /// says that turn ended (`t3_thread_wait`, up to `RESTART_WAIT`), never
    /// into the reply that is still stopping. A turn that doesn't end in
    /// time sends nothing (`restart_timed_out`).
    async fn stop_for_restart(
        &self,
        thread: &str,
        client_request_id: &str,
    ) -> Result<(), CallError> {
        let stopped = self
            .interrupt(thread, None, &format!("{client_request_id}:restart"))
            .await
            .map_err(CallError::before_sending)?;
        if stopped["status"] != "interrupt_requested" {
            return Ok(());
        }
        let mut arguments =
            json!({"threadId": thread, "timeoutMs": RESTART_WAIT.as_millis() as u64});
        if let Some(run) = stopped["runId"].as_str() {
            arguments["runId"] = run.into();
        }
        let waited = self
            .call("t3_thread_wait", arguments)
            .await
            .map_err(CallError::before_sending)?;
        if run_over(&waited) {
            Ok(())
        } else {
            Err(CallError::Refused {
                code: RESTART_TIMED_OUT.into(),
                message: "The reply didn't stop in time.".into(),
            })
        }
    }

    /// Waits up to `timeout` for the run to end (`t3_thread_wait`); true
    /// once T3 Code says it has.
    pub(crate) async fn run_ended(
        &self,
        thread: &str,
        run: &str,
        timeout: Duration,
    ) -> Result<bool, CallError> {
        let waited = self
            .call(
                "t3_thread_wait",
                json!({"threadId": thread, "runId": run, "timeoutMs": timeout.as_millis() as u64}),
            )
            .await?;
        Ok(run_over(&waited))
    }

    /// What a send's answer means for the next one: T3 Code took it, may
    /// have it, or refused it (the thread backs off a while).
    fn settle(&self, native: &str, result: &Result<Value, CallError>) {
        let mut inner = self.inner.lock().unwrap();
        match result {
            Ok(_) => {
                inner.failed.remove(native);
            }
            // T3 Code may have it (recovery reads it back); it refused nothing.
            Err(error) if error.unconfirmed() => {}
            Err(error) => {
                inner.failed.retain(|_, at| at.elapsed() < FAILURE_BACKOFF);
                inner.failed.insert(native.to_owned(), Instant::now());
                // The thread moved or was deleted: match it again later.
                if matches!(error, CallError::Refused { code, .. } if code == "thread_not_found") {
                    if let Some(lookup) = inner.lookups.get_mut(native) {
                        lookup.found = None;
                    }
                }
            }
        }
    }

    /// Uploads each file to T3 Code (`t3_attachment_prepare_upload`, then
    /// the bytes to the signed URL it returns) and returns them as `send`
    /// takes them. Each upload gets an ID of its own, which the message
    /// carries once sent. A failed upload sent nothing.
    pub(crate) async fn upload(
        &self,
        native: &str,
        files: Vec<Upload>,
    ) -> Result<Vec<Value>, CallError> {
        let result = self
            .upload_files(files)
            .await
            .map_err(CallError::before_sending);
        if let Err(error) = &result {
            self.settle(native, &Err(error.clone()));
        }
        result
    }

    async fn upload_files(&self, files: Vec<Upload>) -> Result<Vec<Value>, CallError> {
        let origin = self
            .grant()
            .map(|grant| self.server().unwrap_or(grant.origin))
            .ok_or(CallError::NotConnected)?;
        let mut attachments = Vec::with_capacity(files.len());
        for file in files {
            let kind = file.kind();
            let (size, body) = match file.body {
                UploadBody::Bytes(bytes) => (bytes.len() as u64, reqwest::Body::from(bytes)),
                UploadBody::File { file, size } => {
                    (size, reqwest::Body::from(tokio::fs::File::from_std(file)))
                }
            };
            let upload = self
                .call(
                    "t3_attachment_prepare_upload",
                    json!({"upload": {"type": kind, "name": file.name, "mimeType": file.mime_type, "sizeBytes": size}}),
                )
                .await?;
            let (Some(id), Some(path)) = (
                upload.get("attachmentId").and_then(Value::as_str),
                upload.get("relativeUrl").and_then(Value::as_str),
            ) else {
                return Err(CallError::Protocol("no upload URL".into()));
            };
            if !path.starts_with('/') {
                return Err(CallError::Protocol("upload URL is not on T3 Code".into()));
            }
            let response = self
                .http
                .post(format!("{origin}{path}"))
                .header("content-type", "application/octet-stream")
                .header("content-length", size)
                .body(body)
                .send()
                .await
                .map_err(|_| CallError::Offline)?;
            if !response.status().is_success() {
                return Err(CallError::Refused {
                    code: "upload_failed".into(),
                    message: format!("HTTP {}", response.status()),
                });
            }
            attachments.push(json!({"type": kind, "id": id, "name": file.name,
                "mimeType": file.mime_type, "sizeBytes": size}));
        }
        Ok(attachments)
    }

    /// Stops the turn running in the T3 thread (`t3_thread_interrupt`):
    /// only `run` when given, which T3 Code answers with its final status
    /// once it ended (`thread_not_interruptible` while another runs), so a
    /// run that replaced it is never stopped. `client_request_id` makes a
    /// retry harmless. A thread with nothing running answers `no_active_run`.
    pub(crate) async fn interrupt(
        &self,
        thread: &str,
        run: Option<&str>,
        client_request_id: &str,
    ) -> Result<Value, CallError> {
        let mut arguments = json!({"threadId": thread, "clientRequestId": client_request_id});
        if let Some(run) = run {
            arguments["runId"] = run.into();
        }
        self.call("t3_thread_interrupt", arguments).await
    }

    /// The run active in the T3 thread now (`activeRunId` of
    /// `t3_thread_read`), `None` when nothing runs there.
    pub(crate) async fn active_run(&self, thread: &str) -> Result<Option<String>, CallError> {
        let page = self
            .call(
                "t3_thread_read",
                json!({"threadId": thread, "view": "messages", "limit": 1}),
            )
            .await?;
        let Some(detail) = page.get("thread") else {
            return Err(CallError::Protocol("no thread".into()));
        };
        Ok(detail
            .get("activeRunId")
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// Whether a run is active in the T3 thread. A run state that can't be
    /// read is not "running": missing evidence never shows Working.
    pub(crate) async fn run_in_progress(&self, thread: &str) -> bool {
        self.active_run(thread).await.is_ok_and(|run| run.is_some())
    }

    /// Starts a new thread in T3 Code (`t3_thread_launch`) in the T3 project
    /// whose folder is `workspace` (or in a new T3 scratch folder when the
    /// chat has no project), and returns T3 Code's answer (its `threadId`).
    /// The thread's agent starts on `message`, in plan mode when `plan`.
    /// It runs exactly `requested` (a provider and the ids T3 Code may list
    /// for its model, `t3_model_ids`; none: that provider's first model), or
    /// nothing: a provider T3 Code doesn't run (`provider_unavailable`) or a
    /// model it doesn't offer (`model_unavailable`) is refused before
    /// anything starts, never swapped for another. Without a request, the T3
    /// project's default model runs; a T3 project without one refuses
    /// before creating anything, so the launch is made once more with
    /// `fallback`'s provider, or any provider T3 Code has. T3 Code has no
    /// retry key for a launch: no other refusal and no lost answer is
    /// launched again here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn launch(
        &self,
        workspace: LaunchWorkspace<'_>,
        title: &str,
        message: &str,
        plan: bool,
        requested: Option<(AgentFamily, &[String])>,
        fallback: (AgentFamily, Option<&str>),
        choices: Option<LaunchChoices<'_>>,
    ) -> Result<Value, CallError> {
        let mut arguments = match workspace {
            LaunchWorkspace::Folder(cwd) => {
                let project = self
                    .project_for(cwd)
                    .await?
                    .ok_or_else(|| CallError::Refused {
                        code: "project_not_found".into(),
                        message: "T3 Code has no project for this folder.".into(),
                    })?;
                json!({"projectId": project, "title": title})
            }
            // T3 Code picks its own project and folder for `scratch`.
            LaunchWorkspace::Scratch => json!({"scratch": true, "title": title}),
        };
        if !message.trim().is_empty() {
            arguments["message"] = message.into();
        }
        if plan {
            arguments["interactionMode"] = "plan".into();
        }
        if let Some((family, models)) = requested {
            let mut selection = self
                .requested_selection_instance(
                    family,
                    models,
                    choices.as_ref().and_then(|c| c.instance),
                )
                .await?;
            if let Some(choices) = choices {
                let listed = self
                    .launch_capabilities(selection["model"].as_str())
                    .await?;
                launch_choices(&listed, &mut selection, &choices)?;
                if let Some(mode) = choices.runtime_mode {
                    arguments["runtimeMode"] = mode.into();
                }
            }
            arguments["modelSelection"] = selection;
            return self.call("t3_thread_launch", arguments).await;
        }
        match self.call("t3_thread_launch", arguments.clone()).await {
            Err(CallError::Refused { code, message })
                if code == "invalid_request" && message.contains("no default model") =>
            {
                let listed = self.launch_capabilities(None).await?;
                let (family, model) = fallback;
                let selection = model_selection(&listed, family, model)
                    .or_else(|| any_model_selection(&listed))
                    .ok_or_else(|| CallError::Refused {
                        code: "no_provider".into(),
                        message: "T3 Code has no provider set up.".into(),
                    })?;
                arguments["modelSelection"] = selection;
                self.call("t3_thread_launch", arguments).await
            }
            answer => answer,
        }
    }

    /// The folder of T3 Code's project `id`: where a thread launched with
    /// `scratch` works.
    pub(crate) async fn workspace_root(&self, id: &str) -> Option<String> {
        self.projects()
            .await
            .ok()?
            .into_iter()
            .find_map(|(project, root)| (project == id).then_some(root))
    }

    /// T3 Code's own scratch folders, `scratch` in its home: a chat with no
    /// Project works in a folder of its own there.
    pub(crate) fn scratch_dir(&self) -> Option<PathBuf> {
        self.home.as_ref().map(|home| home.join("scratch"))
    }

    /// The `modelSelection` that runs `family` on one of `models` in T3
    /// Code. T3 Code lists only a few models of most providers, so a model
    /// it leaves out is asked for by its id (`search`). A provider T3 Code
    /// doesn't run, or has turned off, and a model it doesn't offer are
    /// refused here, before anything starts.
    async fn requested_selection_instance(
        &self,
        family: AgentFamily,
        models: &[String],
        instance: Option<&str>,
    ) -> Result<Value, CallError> {
        let driver = t3_driver(family);
        let usable = |listed: &Value| -> Option<Value> {
            listed
                .get("providers")
                .and_then(Value::as_array)?
                .iter()
                .find(|provider| {
                    provider.get("driverKind").and_then(Value::as_str) == Some(driver)
                        && instance.is_none_or(|id| provider["providerInstanceId"] == id)
                        && provider
                            .get("constraints")
                            .and_then(Value::as_array)
                            .is_none_or(Vec::is_empty)
                })
                .cloned()
        };
        let listed = self.launch_capabilities(None).await?;
        let provider = usable(&listed).ok_or_else(|| CallError::Refused {
            code: "provider_unavailable".into(),
            message: String::new(),
        })?;
        let instance = provider
            .get("providerInstanceId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let selection = |model: &str| json!({"instanceId": instance, "model": model});
        if models.is_empty() {
            if let Some(first) = listed_models(&provider).next() {
                return Ok(selection(first));
            }
        }
        if let Some(model) = models
            .iter()
            .find(|model| listed_models(&provider).any(|id| id == model.as_str()))
        {
            return Ok(selection(model));
        }
        for model in models {
            let found = self.launch_capabilities(Some(model)).await?;
            if usable(&found).is_some_and(|provider| {
                provider["providerInstanceId"] == instance.as_str()
                    && listed_models(&provider).any(|id| id == model.as_str())
            }) {
                return Ok(selection(model));
            }
        }
        Err(CallError::Refused {
            code: "model_unavailable".into(),
            message: String::new(),
        })
    }

    /// T3 Code's providers and their models (`orchestrator_capabilities`,
    /// those matching `search` when given), read before a launch: an answer
    /// lost here launched nothing.
    async fn launch_capabilities(&self, search: Option<&str>) -> Result<Value, CallError> {
        let arguments = match search {
            Some(search) => json!({"search": search}),
            None => json!({}),
        };
        self.call("orchestrator_capabilities", arguments)
            .await
            .map_err(CallError::before_sending)
    }

    /// The T3 project whose folder is `cwd`.
    async fn project_for(&self, cwd: &str) -> Result<Option<String>, CallError> {
        Ok(self
            .projects()
            .await?
            .into_iter()
            .find(|(_, root)| root == cwd.trim_end_matches('/'))
            .map(|(id, _)| id))
    }

    /// Shorter lookup timeouts and backoff, for tests.
    #[cfg(test)]
    pub(crate) fn set_lookup_timing(&self, timeout: Duration, backoff: Duration) {
        let mut inner = self.inner.lock().unwrap();
        inner.lookup_timeout = timeout;
        inner.lookup_backoff = backoff;
    }

    /// Calls that wait as long as T3 Code takes to answer, for tests that
    /// hold T3 Code's answer and must not race the 30 s request timeout.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn without_request_timeout(mut self) -> Self {
        self.http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        self
    }

    /// Ends every send backoff, for tests.
    #[cfg(test)]
    pub(crate) fn clear_failures_for_test(&self) {
        self.inner.lock().unwrap().failed.clear();
    }

    /// Ends this thread's lookup backoff as if its wait had passed (the
    /// failures that doubled it stay counted), for tests.
    #[cfg(test)]
    pub(crate) fn end_lookup_backoff_for_test(&self, native: &str) {
        if let Some(lookup) = self.inner.lock().unwrap().lookups.get_mut(native) {
            lookup.retry_at = None;
        }
    }

    /// Whether a lookup runs for this thread, and how long until a failed
    /// one may be retried.
    #[cfg(test)]
    pub(crate) fn lookup_state(&self, native: &str) -> (bool, Option<Duration>) {
        let inner = self.inner.lock().unwrap();
        inner.lookups.get(native).map_or((false, None), |lookup| {
            (
                lookup.running,
                lookup
                    .retry_at
                    .map(|at| at.saturating_duration_since(Instant::now())),
            )
        })
    }

    /// T3 Code runs on the Mac and its last call reached it (a call that
    /// found it not running counts for `FAILURE_BACKOFF`).
    fn reachable(&self) -> bool {
        self.server().is_some()
            && !self
                .inner
                .lock()
                .unwrap()
                .offline
                .is_some_and(|at| at.elapsed() < FAILURE_BACKOFF)
    }

    /// The T3 thread a lookup matched this native thread to, however long
    /// ago: T3 Code knows it.
    fn matched(&self, native: &str) -> Option<String> {
        match &self.inner.lock().unwrap().lookups.get(native)?.found {
            Some((Some(thread), _)) => Some(thread.clone()),
            _ => None,
        }
    }

    /// What T3 Code's own store says of this Claude session or Codex thread.
    /// T3 Code keeps each provider thread's native ID in its payload and
    /// finds threads by it the same way (read in T3 Code 0.1.1: the
    /// `orchestration_v2_projection_provider_threads` table, `EventSink`'s
    /// `$.nativeThreadRef.nativeId` lookup); a T3 thread continues the
    /// provider thread it names active. The store is only read
    /// (`open_store`). Only a provider thread of `family`'s T3 Code driver counts.
    async fn kept(&self, native: &str, family: AgentFamily) -> Kept {
        let Some(mut connection) = self.open_store().await else {
            return Kept::Unknown;
        };
        let rows: Result<Vec<(String, bool)>, _> = sqlx::query_as(
            "SELECT t.thread_id, t.archived_at IS NULL AND t.deleted_at IS NULL \
             AND t.active_provider_thread_id IS p.provider_thread_id \
             FROM orchestration_v2_projection_provider_threads p \
             JOIN orchestration_v2_projection_threads t ON t.thread_id = p.thread_id \
             WHERE json_extract(p.payload_json, '$.nativeThreadRef.nativeId') = ? \
             AND p.driver = ? LIMIT 8",
        )
        .bind(native)
        .bind(t3_driver(family))
        .fetch_all(&mut connection)
        .await;
        let _ = connection.close().await;
        let Ok(rows) = rows else {
            return Kept::Unknown;
        };
        let mut open = rows
            .iter()
            .filter(|(_, open)| *open)
            .map(|(thread, _)| thread);
        match (open.next(), open.next(), rows.is_empty()) {
            (Some(thread), None, _) => Kept::Thread(thread.clone()),
            (None, _, false) => Kept::Gone,
            _ => Kept::Unknown,
        }
    }

    /// The Claude session or Codex thread T3 thread `thread` runs on
    /// `family`'s driver, from T3 Code's own store (as `kept`, the other way
    /// round): known once its agent started. Only read.
    pub(crate) async fn native_for_thread(
        &self,
        thread: &str,
        family: AgentFamily,
    ) -> Option<String> {
        let mut connection = self.open_store().await?;
        let native: Result<Option<String>, _> = sqlx::query_scalar(
            "SELECT json_extract(p.payload_json, '$.nativeThreadRef.nativeId') \
             FROM orchestration_v2_projection_threads t \
             JOIN orchestration_v2_projection_provider_threads p \
             ON p.thread_id = t.thread_id AND p.provider_thread_id = t.active_provider_thread_id \
             WHERE t.thread_id = ? AND p.driver = ? LIMIT 1",
        )
        .bind(thread)
        .bind(t3_driver(family))
        .fetch_optional(&mut connection)
        .await
        .map(Option::flatten);
        let _ = connection.close().await;
        native.ok().flatten().filter(|native| !native.is_empty())
    }

    /// Forgets everything kept in memory but the sign-in, as a restarted
    /// daemon starts, for tests.
    #[cfg(test)]
    pub(crate) fn restarted_for_test(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.lookups.clear();
        inner.failed.clear();
        inner.reasons.clear();
        inner.offline = None;
        inner.session = None;
        drop(inner);
        self.queue.inner.lock().unwrap().clear();
    }

    /// T3 Code recently did not take a message for this thread.
    pub(crate) fn backing_off(&self, native: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .failed
            .get(native)
            .is_some_and(|at| at.elapsed() < FAILURE_BACKOFF)
    }
}

/// T3 Code's own store, under its home (T3 Code's `ServerConfig`).
const T3_STORE: &str = "userdata/statev2.sqlite";

/// What T3 Code's own store says of a Claude session or Codex thread
/// (`T3Code::kept`).
#[derive(Debug, PartialEq, Eq)]
enum Kept {
    /// The T3 thread that continues it.
    Thread(String),
    /// T3 Code had it, but that thread is archived or deleted there, or now
    /// continues another session: it isn't T3 Code's any more.
    Gone,
    /// Not there, or the store can't be read.
    Unknown,
}

struct Pending {
    /// `Inner::generation` when the sign-in began.
    generation: u64,
    origin: String,
    issuer: String,
    token: String,
    client_id: String,
    redirect: String,
    verifier: String,
    state: String,
    resource: String,
    /// T3 Code's approval decision (`/oauth/mcp/decision`, next to
    /// `/oauth/mcp/authorize`), the authorization request it decides on and
    /// the approval page for the browser.
    decision: Option<String>,
    request: Value,
    page: Url,
}

fn now_ms() -> i64 {
    crate::now_ms() as i64
}

fn random() -> String {
    let mut bytes = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .expect("system randomness unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// JavaScript's `encodeURIComponent`, which T3 Code applies to each part of
/// the IDs it derives.
fn uri_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn loopback(origin: &str) -> bool {
    Url::parse(origin).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && matches!(
                url.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
            )
    })
}

fn same_origin(url: &str, origin: &str) -> bool {
    match (Url::parse(url), Url::parse(origin)) {
        (Ok(url), Ok(origin)) => url.origin() == origin.origin(),
        _ => false,
    }
}

/// Why a response body was not read.
#[derive(Debug, PartialEq, Eq)]
enum BodyError {
    /// More than the budget arrived (`MAX_BODY`).
    TooLarge,
    /// The connection failed or timed out part-way.
    Failed,
}

/// Reads `chunks` into memory, refusing the chunk that would take the body
/// past `limit` before it is appended, so memory never holds more than
/// `limit` bytes of it (nor reserves more: growth is clamped to `limit`) and
/// the rest is never read. The count is of bytes received, not of any
/// Content-Length. `retained` ends as the most bytes held at once.
async fn collect_bounded<B: AsRef<[u8]>, E>(
    chunks: impl futures_util::Stream<Item = Result<B, E>>,
    limit: usize,
    retained: &mut usize,
) -> Result<Vec<u8>, BodyError> {
    use futures_util::StreamExt;
    let mut chunks = std::pin::pin!(chunks);
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| BodyError::Failed)?;
        let chunk = chunk.as_ref();
        let needed = body.len() + chunk.len();
        if needed > limit {
            return Err(BodyError::TooLarge);
        }
        if needed > body.capacity() {
            let target = needed.max(body.capacity().saturating_mul(2)).min(limit);
            body.reserve_exact(target - body.len());
        }
        body.extend_from_slice(chunk);
        *retained = (*retained).max(body.len());
    }
    Ok(body)
}

/// The JSON-RPC response in a body sent as JSON or as server-sent events.
fn json_rpc_payload(body: &[u8], sse: bool) -> Option<Value> {
    if !sse {
        return serde_json::from_slice(body).ok();
    }
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|message| message.get("id").is_some() || message.get("result").is_some())
}

fn tool_result(payload: Value) -> Result<Value, CallError> {
    if let Some(error) = payload.get("error") {
        return Err(CallError::Protocol(
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_owned(),
        ));
    }
    let result = payload.get("result").cloned().unwrap_or_default();
    let structured = result.get("structuredContent").cloned().or_else(|| {
        result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str(text).ok())
    });
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let detail = structured.unwrap_or_default();
        return Err(CallError::Refused {
            code: detail
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("refused")
                .to_owned(),
            message: detail
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| result.pointer("/content/0/text").and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned(),
        });
    }
    structured.ok_or_else(|| CallError::Protocol("no structured result".into()))
}

/// Evidence that a T3 thread is this session: the thread's first message
/// from the owner is in the session's first prompt (T3 Code wraps some, such
/// as task notifications), and its latest is among the session's last three
/// (messages sent another way, such as by Cupola, follow it there).
/// Measured on the owner's 83 T3 Code Claude threads: with the folder and
/// uniqueness checks, 76 matched their own session, none another, 7 none.
/// Whether a `t3_thread_wait` answer says the run ended.
fn run_over(waited: &Value) -> bool {
    waited["timedOut"] == false
        && !matches!(
            waited["status"].as_str(),
            None | Some("preparing" | "starting" | "running" | "waiting")
        )
}

pub(crate) fn same_conversation(t3: &[String], prompts: &[String]) -> bool {
    let texts: Vec<&str> = t3
        .iter()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .collect();
    let (Some(first), Some(last), Some(opening)) = (texts.first(), texts.last(), prompts.first())
    else {
        return false;
    };
    opening.contains(first)
        && prompts
            .iter()
            .rev()
            .take(3)
            .any(|prompt| prompt.contains(last))
}

/// The owner's messages in a Claude transcript or Codex rollout, oldest
/// first: Claude's user entries that are neither tool results nor meta, or
/// Codex's `user_message` events (its `user` items also carry instructions).
pub(crate) fn transcript_prompts(path: &Path, family: AgentFamily) -> Vec<String> {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut prompts = Vec::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("user") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match family {
            AgentFamily::Claude => {
                if entry.get("type").and_then(Value::as_str) != Some("user")
                    || entry.get("isMeta").and_then(Value::as_bool) == Some(true)
                    || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
                {
                    continue;
                }
                match entry.pointer("/message/content") {
                    Some(Value::String(text)) => prompts.push(text.clone()),
                    Some(Value::Array(blocks)) => {
                        let tool_result = blocks.iter().any(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_result")
                        });
                        let text: Vec<&str> = blocks
                            .iter()
                            .filter(|block| {
                                block.get("type").and_then(Value::as_str) == Some("text")
                            })
                            .filter_map(|block| block.get("text").and_then(Value::as_str))
                            .collect();
                        if !tool_result && !text.is_empty() {
                            prompts.push(text.join("\n"));
                        }
                    }
                    _ => {}
                }
            }
            AgentFamily::Codex
                if entry.get("type").and_then(Value::as_str) == Some("event_msg")
                    && entry.pointer("/payload/type").and_then(Value::as_str)
                        == Some("user_message") =>
            {
                if let Some(text) = entry.pointer("/payload/message").and_then(Value::as_str) {
                    prompts.push(text.to_owned());
                }
            }
            _ => {}
        }
    }
    prompts
}

/// How a message goes into a T3 thread with a turn running there: after it
/// (the phone's Send or Queue), into it (Send now), or in place of it (Take
/// over: T3 Code interrupts the turn and starts one with the message).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendMode {
    Queue,
    Steer,
    Restart,
}

impl SendMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
            Self::Restart => "restart",
        }
    }
}

/// A file sent with a message, as T3 Code receives it.
pub(crate) struct Upload {
    pub(crate) name: String,
    pub(crate) mime_type: String,
    pub(crate) body: UploadBody,
}

pub(crate) enum UploadBody {
    Bytes(Vec<u8>),
    /// An open descriptor of the private, verified snapshot, streamed to T3's
    /// signed upload URL. Never buffer a video or reopen the stored original.
    File {
        file: std::fs::File,
        size: u64,
    },
}

impl Upload {
    /// T3 Code shows an image it can pass to the agent as one; anything
    /// else goes as a file.
    fn kind(&self) -> &'static str {
        if IMAGE_TYPES.contains(&self.mime_type.as_str()) {
            "image"
        } else {
            "file"
        }
    }
}

/// What T3 Code's send can't carry: more or larger files than it takes,
/// skills, or another model. Such a message waits for the thread as it would
/// without T3 Code (`projects::held_on_mac`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Limit {
    Attachments,
    Skills,
    Model,
}

impl Limit {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Attachments => "attachments",
            Self::Skills => "skills",
            Self::Model => "model",
        }
    }
}

/// Why T3 Code's send can't carry this message, if it can't. A detail that
/// can't be read counts as present.
pub(crate) async fn limit(
    state: &AppState,
    stored: &StoredProjectConversation,
    message: &str,
) -> Option<Limit> {
    let store = &state.store;
    let fits = |files: Vec<wonder_store::StoredConversationFile>| {
        files.len() <= MAX_FILES
            && files.iter().all(|file| {
                let image = file
                    .mime_type
                    .as_deref()
                    .is_some_and(|mime| mime.starts_with("image/"));
                let limit = if image {
                    MAX_IMAGE_BYTES
                } else {
                    MAX_FILE_BYTES
                };
                // An unknown size is checked by T3 Code on upload.
                file.byte_size
                    .is_none_or(|size| (1..=limit).contains(&size))
            })
    };
    if !store.attachments_for_message(message).await.is_ok_and(fits) {
        return Some(Limit::Attachments);
    }
    if store
        .message_codex_skills(message)
        .await
        .map_or(true, |skills| !skills.is_empty())
    {
        return Some(Limit::Skills);
    }
    // Every message records the provider and model it runs with: the
    // thread's own unless the phone chose others for it (or a review). T3
    // Code switches the thread's model and effort before the send
    // (`T3Code::configure`); another provider is a switch T3 Code's tools
    // don't make safely, so that message waits.
    let other = store
        .project_message_target(message)
        .await
        .map_or(true, |target| {
            target.is_some_and(|target| target.family.is_some_and(|family| family != stored.family))
        });
    other.then_some(Limit::Model)
}

/// The model, effort and speed a message runs with (the phone's choice or
/// the thread's own), with the ids T3 Code may know the model by (the
/// catalog's provider ids first). Always compared with T3 Code's own
/// configuration, never Cupola's record of the thread: the previous
/// message, or the owner in T3 Code's composer, may have switched it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Chosen {
    family: Option<AgentFamily>,
    models: Vec<String>,
    effort: Option<String>,
    service_tier: Option<String>,
    /// The model is the thread's own: one T3 Code doesn't offer leaves T3
    /// Code's model as it is rather than refusing the message.
    own_model: bool,
}

pub(crate) async fn chosen(
    state: &AppState,
    stored: &StoredProjectConversation,
    message: &str,
) -> Option<Chosen> {
    let target = state
        .store
        .project_message_target(message)
        .await
        .ok()
        .flatten()?;
    let mut chosen = Chosen {
        family: Some(stored.family),
        ..Chosen::default()
    };
    if let Some(model) = target.model {
        chosen.own_model = Some(&model) == stored.model.as_ref();
        chosen.models = t3_model_ids(&*state.runtime_catalog.read().await, &model);
    }
    chosen.effort = target.effort;
    chosen.service_tier = target.service_tier;
    (!chosen.models.is_empty() || chosen.effort.is_some() || chosen.service_tier.is_some())
        .then_some(chosen)
}

/// The ids T3 Code may list for Cupola's `model`, most specific first: the
/// provider's own ids for it (Claude's "claude:opus" is T3 Code's
/// "claude-opus-5-5"), then the id without Cursor's "[option=…]" suffix and
/// without Cupola's family namespace (OpenCode's
/// "opencode:opencode/deepseek-v4.1-flash-free" is T3 Code's
/// "opencode/deepseek-v4.1-flash-free").
pub(crate) fn t3_model_ids(catalog: &crate::RuntimeCatalog, model: &str) -> Vec<String> {
    let native = catalog
        .models
        .iter()
        .find(|option| option.id == model)
        .map(|option| option.native_ids.clone())
        .unwrap_or_default();
    let bare = model.split('[').next().unwrap_or(model);
    let unprefixed = bare.split_once(':').map_or(bare, |(_, rest)| rest);
    native
        .into_iter()
        .chain([bare.to_owned(), unprefixed.to_owned()])
        .fold(Vec::new(), |mut ids, id| {
            if !id.is_empty() && !ids.contains(&id) {
                ids.push(id);
            }
            ids
        })
}

/// The value of option `id` in a T3 Code `modelSelection` (its options are
/// `[{id, value}]`, or an object by id as T3 Code also reads and writes).
fn option_value<'a>(selection: &'a Value, id: &str) -> Option<&'a Value> {
    match selection.get("options")? {
        Value::Array(options) => options
            .iter()
            .find(|option| option["id"] == id)
            .map(|option| &option["value"]),
        Value::Object(options) => options.get(id),
        _ => None,
    }
}

fn set_option(selection: &mut Value, id: &str, value: Value) {
    match selection.get_mut("options") {
        Some(Value::Object(options)) => {
            options.insert(id.to_owned(), value);
        }
        Some(Value::Array(options)) => {
            options.retain(|option| option["id"] != id);
            options.push(json!({"id": id, "value": value}));
        }
        _ => selection["options"] = json!([{"id": id, "value": value}]),
    }
}

/// T3 Code's options for the chosen effort and speed: Claude's `effort`
/// and `fastMode`, Codex's `reasoningEffort` and `serviceTier` (T3 Code's
/// `claudeModelOptions.ts`, `codexModelOptions.ts`).
fn chosen_options(chosen: &Chosen) -> Vec<(&'static str, Value)> {
    let mut options = Vec::new();
    let (effort_key, speed_key) = model_option_keys(chosen.family);
    if let Some(effort) = &chosen.effort {
        options.push((effort_key, json!(effort)));
    }
    if let Some(tier) = &chosen.service_tier {
        options.push(if speed_key == "fastMode" {
            (speed_key, json!(tier == "fast"))
        } else {
            (speed_key, json!(tier))
        });
    }
    options
}

/// Native T3 option names, shared by catalog projection and configuration.
fn model_option_keys(family: Option<AgentFamily>) -> (&'static str, &'static str) {
    if family == Some(AgentFamily::Claude) {
        ("effort", "fastMode")
    } else {
        ("reasoningEffort", "serviceTier")
    }
}

/// What a message to a thread T3 Code has open does: the one answer the
/// phone's note (`heldBy.t3Code`), the send receipt and dispatch share.
/// Whether the thread is held stays `projects::held_on_mac`'s, for every
/// caller (rewind, review and provider switch refuse a held thread); only
/// sending asks this as well.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// It goes into this T3 thread through `t3_thread_send`.
    Sends(String),
    /// Connected, but the message has what T3 Code's send can't carry: it
    /// waits for the thread as without T3 Code.
    CantSend(Limit),
    /// Cupola isn't signed in to T3 Code now; why.
    Connect(Unconnected),
    /// Connected, but the message has no T3 thread to go into now; why.
    NotFound(Unrouted),
}

/// Why Cupola isn't signed in to T3 Code: the phone says what to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unconnected {
    /// Never connected, or disconnected: Connect in Settings.
    SignedOut,
    /// The grant ended or T3 Code refused it, and Cupola couldn't sign in
    /// again by itself: Connect again.
    Expired,
    /// Cupola is signing in again by itself (`renew_if_due`).
    Renewing,
    /// The owner removed Cupola in T3 Code: Cupola asks before it connects
    /// again.
    Revoked,
    /// The saved sign-in couldn't be read from the Keychain.
    Unreadable,
}

/// Why a connected Cupola has no T3 thread for a message now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unrouted {
    /// Its T3 thread is being looked for (`route`).
    Finding,
    /// No single T3 thread is this conversation (none, or several alike).
    Unmatched,
    /// T3 Code didn't answer the lookup; it is retried after a wait. For a
    /// thread nothing on the Mac has open, T3 Code isn't running or
    /// answering: Cupola runs the reply itself.
    Unreachable,
    /// T3 Code just refused a message to it; sends wait a while.
    Refused,
}

impl Delivery {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Sends(_) => "sends",
            Self::CantSend(_) => "cantSend",
            Self::Connect(_) => "connect",
            Self::NotFound(_) => "notFound",
        }
    }

    /// Why a message doesn't go through T3 Code (`heldBy.t3CodeReason`).
    pub(crate) fn reason(&self) -> Option<&'static str> {
        Some(match self {
            Self::Sends(_) | Self::CantSend(_) => return None,
            Self::Connect(Unconnected::SignedOut) => "signedOut",
            Self::Connect(Unconnected::Expired) => "expired",
            Self::Connect(Unconnected::Renewing) => "renewing",
            Self::Connect(Unconnected::Revoked) => "revoked",
            Self::Connect(Unconnected::Unreadable) => "unreadable",
            Self::NotFound(Unrouted::Finding) => "finding",
            Self::NotFound(Unrouted::Unmatched) => "unmatched",
            Self::NotFound(Unrouted::Unreachable) => "unreachable",
            Self::NotFound(Unrouted::Refused) => "refused",
        })
    }

    pub(crate) fn limit(&self) -> Option<Limit> {
        match self {
            Self::CantSend(limit) => Some(*limit),
            _ => None,
        }
    }

    pub(crate) fn sends(&self) -> bool {
        matches!(self, Self::Sends(_))
    }

    /// For a thread nothing on the Mac has open: whether its message waits
    /// for T3 Code rather than Cupola running the reply. Only a thread T3
    /// Code owns ever does: after T3 Code just refused a message to it, and
    /// while T3 Code runs but its send can't carry the message (the phone
    /// says why and what to do, `unheld_note`), never silently without T3
    /// Code. Any other thread's message never waits for T3 Code.
    pub(crate) fn waits_unheld(&self) -> bool {
        matches!(self, Self::NotFound(Unrouted::Refused) | Self::CantSend(_))
    }
}

/// `Delivery` for `message` (the conversation's next waiting message when
/// `None`: dispatch sends that one next, and a later one never overtakes it).
/// A change in why messages don't go through T3 Code is logged once.
pub(crate) async fn delivery(
    state: &AppState,
    stored: &StoredProjectConversation,
    message: Option<&str>,
) -> Delivery {
    delivery_seen(state, stored, message, Seen::Unread).await
}

/// Who has the thread open on the Mac: already read for this request, so
/// the checks it makes share one scan of the Mac's Claude Code processes, or
/// still to be read.
#[derive(Clone, Copy)]
pub(crate) enum Seen<'a> {
    Unread,
    Read(Option<&'a crate::desktop_activity::Holder>),
}

/// `delivery`, with the holder the caller has already read.
pub(crate) async fn delivery_seen(
    state: &AppState,
    stored: &StoredProjectConversation,
    message: Option<&str>,
    seen: Seen<'_>,
) -> Delivery {
    let delivery = deliver(state, stored, message, seen).await;
    let reason = delivery.reason();
    if state
        .t3_code
        .reason_changed(&stored.conversation_id, reason)
    {
        let _ = state.logger.record(
            "info",
            "t3_code_delivery",
            json!({"conversationId": stored.conversation_id, "delivery": delivery.as_str(),
                "reason": reason}),
        );
    }
    delivery
}

async fn deliver(
    state: &AppState,
    stored: &StoredProjectConversation,
    message: Option<&str>,
    seen: Seen<'_>,
) -> Delivery {
    state.t3_code.log_to(&state.logger);
    state.t3_code.renew_if_due();
    if !state.t3_code.connected() {
        return Delivery::Connect(state.t3_code.unconnected());
    }
    let thread = match find(state, stored, true, seen).await {
        Ok(thread) => thread,
        Err(why) => return Delivery::NotFound(why),
    };
    let next = match message {
        Some(message) => Some(message.to_owned()),
        None => state
            .store
            .pending_queue(&stored.conversation_id)
            .await
            .ok()
            .and_then(|queue| queue.into_iter().next())
            .map(|next| next.id),
    };
    let limit = match next {
        Some(next) => limit(state, stored, &next).await,
        None => None,
    };
    match limit {
        Some(limit) => Delivery::CantSend(limit),
        None => Delivery::Sends(thread),
    }
}

/// Where T3 Code has a message whose answer was lost (`has_sent`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// In the timeline: T3 Code started it.
    Started,
    /// Waiting in the thread's queue as this queued run (`t3QueuedRunId`).
    Queued(String),
    /// Nowhere: it never arrived.
    Missing,
}

/// A message Cupola sent into a T3 thread, as T3 Code's timeline names it.
struct Sent {
    /// The Wonder message, which was the send's `clientRequestId`.
    message: String,
    /// T3 Code's ID for it, from its answer (`t3MessageId`).
    t3_message: Option<String>,
    /// The files T3 Code issued for it.
    attachments: Vec<String>,
}

impl Sent {
    /// Whether this item of T3 Code's timeline is the message: the user
    /// message with the ID T3 Code answered. Without that answer (it was
    /// lost), a text is the one T3 Code named by its `clientRequestId`
    /// (`message:mcp:<grant>:thread-send:<clientRequestId>`), and a message
    /// with files the one carrying the files T3 Code issued for it; no other
    /// message has either. Never by its words.
    fn is(&self, item: &Value) -> bool {
        if item.get("type").and_then(Value::as_str) != Some("user_message") {
            return false;
        }
        let id = item.get("messageId").and_then(Value::as_str);
        if let Some(known) = &self.t3_message {
            return id == Some(known.as_str());
        }
        if self.attachments.is_empty() {
            let suffix = format!(":thread-send:{}", uri_component(&self.message));
            return id.is_some_and(|id| id.starts_with("message:mcp:") && id.ends_with(&suffix));
        }
        let ids: Vec<&str> = item
            .get("attachments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|file| file.get("id").and_then(Value::as_str))
            .collect();
        self.attachments.iter().all(|id| ids.contains(&id.as_str()))
    }
}

/// A message Cupola sends through T3 Code shows once (owner, Oct 21: the
/// phone showed it twice). T3 Code runs the turn, so its agent records the
/// message as a user message of its own, which nothing ties to Cupola's
/// message: T3 Code gives the agent neither its ID nor Cupola's. Cupola's
/// message always shows, with its files and delivery state, and its
/// agent's record of it is that same message (`echoes`) when all of these
/// hold (lead, final design):
///
/// - T3 Code started the message: its timeline has it, by T3 Code's own ID
///   for it (`Sent::is`, asked by `reconcile_sends`);
/// - the record was saved after the message's boundary (`t3After`, the
///   newest history row saved before the send);
/// - the record's words are the message's (`Delivered::words`);
/// - no message sent before it was paired with that record.
///
/// Every other record shows as it is, so words typed in T3 Code always
/// show. The one mistake left: the agent never records the message (it
/// crashed) and the owner types exactly its words in T3 Code; the two then
/// show as one.
///
/// After each history read, T3 Code is asked whether it started each
/// message that has a record with its words after its boundary. When it
/// hasn't, the boundary moves to the newest row saved before that answer:
/// T3 Code starts a message before its agent records it, so none of those
/// rows is its record. T3 Code is read only for a new record with a
/// message's words, never because time passed, and Cupola never stops
/// looking. A lookup that fails is retried at doubling intervals
/// (`ASK_BACKOFF`). When T3 Code started one, the phone is told which
/// records are now its (`MESSAGE_RECORDS`).
pub(crate) async fn reconcile_sends(state: &AppState, conversation: &str) {
    queue::check_soon(state, conversation);
    let Ok(sends) = state.store.t3_code_sent_messages(conversation).await else {
        return;
    };
    let now = now_ms();
    let asked: Vec<Delivered> = delivered(sends)
        .into_iter()
        .filter(|send| !send.started && now >= send.retry_at)
        .collect();
    let Some(after) = asked.iter().map(|send| send.after).min() else {
        return;
    };
    if !state.t3_code.connected() {
        return;
    }
    let records = user_records(state, conversation, after).await;
    let mut threads = std::collections::BTreeMap::<&str, Vec<&Delivered>>::new();
    for send in &asked {
        if records
            .iter()
            .any(|record| record.row > send.after && send.words(&record.text, &record.taken))
        {
            threads.entry(&send.thread).or_default().push(send);
        }
    }
    if threads.is_empty() {
        return;
    }
    // Read before T3 Code answers, so no row up to it is the record of a
    // message T3 Code hadn't started.
    let saved = state
        .store
        .conversation_history_mark(conversation)
        .await
        .map(|(_, newest)| newest);
    let mut started = false;
    let mut patches = Vec::new();
    for (thread, sends) in threads {
        let sent: Vec<&Sent> = sends.iter().map(|send| &send.sent).collect();
        let found = state.t3_code.started(thread, &sent).await;
        for send in sends {
            let patch = match (&found, &saved) {
                (Ok(found), _) if found.contains(&send.sent.message) => {
                    started = true;
                    json!({"t3Started": true, "t3Failures": null, "t3RetryAtMs": null})
                }
                (Ok(_), Ok(saved)) => {
                    json!({"t3After": saved, "t3Failures": null, "t3RetryAtMs": null})
                }
                (Ok(_), Err(_)) => continue,
                (Err(_), _) => json!({
                    "t3Failures": send.failures + 1,
                    "t3RetryAtMs": now + ask_backoff(send.failures + 1),
                }),
            };
            patches.push((&send.sent.message, patch));
        }
    }
    let before = if started {
        Some(echoes(state, conversation).await)
    } else {
        None
    };
    for (message, patch) in patches {
        let _ = state.store.update_t3_code_intent(message, &patch).await;
    }
    let Some(before) = before else {
        return;
    };
    let records = relabelled(&before, &echoes(state, conversation).await);
    if records.is_empty() {
        return;
    }
    // Named in the event, not its context: no turn's history changes.
    let _ = crate::publish_event_with_context(
        state,
        crate::WonderEvent::Activity {
            category: MESSAGE_RECORDS.into(),
            state: "updated".into(),
            detail: Some(json!({ "records": records }).to_string()),
        },
        crate::EventContext {
            conversation_id: Some(conversation.to_owned()),
            ..crate::EventContext::default()
        },
    )
    .await;
}

/// The live event telling the phone which saved records are now a sent
/// message's (`echoes`). The phone has often saved a record already, in a
/// read made before T3 Code started its message, and keeps it when a newer
/// read leaves it out; so it labels the record it saved, without reading
/// the conversation again. A phone that missed the event labels it from the
/// next read's `messageRecords` (`message_records`).
pub(crate) const MESSAGE_RECORDS: &str = "message_records";

/// Every record of `echoes` as the phone is told of it, for a history read:
/// the phone labels each record it saved, also outside the page read.
pub(crate) fn message_records(echoes: &HashMap<(String, String), String>) -> Vec<Value> {
    relabelled(&HashMap::new(), echoes)
}

/// The records whose message changed from `before` to `after`, as the phone
/// is told of them. A record a message was paired with stays paired:
/// starting another message only moves later messages to later records.
fn relabelled(
    before: &HashMap<(String, String), String>,
    after: &HashMap<(String, String), String>,
) -> Vec<Value> {
    let mut records: Vec<_> = after
        .iter()
        .filter(|(record, client)| before.get(*record) != Some(*client))
        .collect();
    records.sort();
    records
        .into_iter()
        .map(|((turn, item), client)| json!({"turnId": turn, "itemId": item, "clientId": client}))
        .collect()
}

/// The wait before asking T3 Code again after `failures` lookups in a row
/// failed: doubling from `ASK_BACKOFF` up to `ASK_BACKOFF_MAX`.
fn ask_backoff(failures: u64) -> i64 {
    let first = ASK_BACKOFF.as_millis() as i64;
    first
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(ASK_BACKOFF_MAX.as_millis() as i64)
}

/// The records of messages Cupola sent through T3 Code (`reconcile_sends`):
/// by the (turn ID, item ID) of each, its message's client ID. Worked out
/// incrementally projected from saved history; mutations reset the projection.
pub(crate) async fn echoes(
    state: &AppState,
    conversation: &str,
) -> HashMap<(String, String), String> {
    let Ok(sends) = state.store.t3_code_sent_messages(conversation).await else {
        return HashMap::new();
    };
    let started: Vec<Delivered> = delivered(sends)
        .into_iter()
        .filter(|send| send.started)
        .collect();
    let Some(after) = started.iter().map(|send| send.after).min() else {
        return HashMap::new();
    };
    // Send order and boundaries are part of greedy matching. If a formerly
    // queued earlier send starts later, recompute rather than freezing a later
    // send's ambiguous match. Stable inputs reuse confirmed records.
    let signature = hex::encode(Sha256::digest(
        serde_json::to_vec(
            &started
                .iter()
                .map(|send| {
                    (
                        &send.sent.message,
                        &send.client,
                        &send.body,
                        &send.files,
                        &send.thread,
                        send.after,
                    )
                })
                .collect::<Vec<_>>(),
        )
        .unwrap(),
    ));
    let Ok(cache) = state.store.t3_echo_cache(conversation).await else {
        return pair(&started, &user_records(state, conversation, after).await);
    };
    let saved: Option<Vec<EchoMatch>> = (cache.signature.as_deref() == Some(&signature))
        .then(|| serde_json::from_str(&cache.matches_json).ok())
        .flatten();

    let boundary = if saved.is_some() {
        cache.last_sequence.max(after)
    } else {
        after
    };
    let Ok(rows) = state
        .store
        .t3_code_user_records(conversation, boundary)
        .await
    else {
        return echo_map(saved.as_deref().unwrap_or_default());
    };
    let last_sequence = rows.last().map_or(boundary, |row| row.0);
    let matches = pair_incremental(
        &started,
        saved.as_deref().unwrap_or_default(),
        &parse_user_records(rows),
    );
    if saved.is_none() || last_sequence != cache.last_sequence {
        let _ = state
            .store
            .save_t3_echo_cache(
                conversation,
                cache.revision,
                &signature,
                last_sequence,
                &serde_json::to_string(&matches).unwrap(),
            )
            .await;
    }
    echo_map(&matches)
}

/// Pairs each message, in the order sent, with the first record after its
/// boundary with its words that no message before it was paired with.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct EchoMatch {
    message: String,
    row: i64,
    turn: String,
    item: String,
    client: String,
}

fn echo_map(matches: &[EchoMatch]) -> HashMap<(String, String), String> {
    matches
        .iter()
        .map(|matched| {
            (
                (matched.turn.clone(), matched.item.clone()),
                matched.client.clone(),
            )
        })
        .collect()
}

fn pair(sends: &[Delivered], records: &[Record]) -> HashMap<(String, String), String> {
    echo_map(&pair_incremental(sends, &[], records))
}

fn pair_incremental(
    sends: &[Delivered],
    saved: &[EchoMatch],
    records: &[Record],
) -> Vec<EchoMatch> {
    let mut paired = vec![false; records.len()];
    let mut matches = saved.to_vec();
    let confirmed: std::collections::HashSet<_> = saved
        .iter()
        .map(|matched| matched.message.as_str())
        .collect();
    for send in sends {
        if confirmed.contains(send.sent.message.as_str()) {
            continue;
        }
        let Some(index) = (0..records.len()).find(|&index| {
            !paired[index]
                && records[index].row > send.after
                && send.words(&records[index].text, &records[index].taken)
        }) else {
            continue;
        };
        paired[index] = true;
        let record = &records[index];
        matches.push(EchoMatch {
            message: send.sent.message.clone(),
            row: record.row,
            turn: record.turn.clone(),
            item: record.item.clone(),
            client: send.client.clone(),
        });
    }
    // Preserve original send order even when an earlier unmatched send gets
    // a later record. Duplicate turn/item events retain full-pair semantics.
    let order: HashMap<_, _> = sends
        .iter()
        .enumerate()
        .map(|(i, send)| (send.sent.message.as_str(), i))
        .collect();
    matches.sort_by_key(|matched| {
        order
            .get(matched.message.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
    matches
}

/// A message Cupola sent through T3 Code that shows (delivered, or
/// possibly delivered).
struct Delivered {
    sent: Sent,
    /// The Wonder message's client ID: its bubble on the phone.
    client: String,
    body: String,
    /// The (type, name) of each file it carried, in order.
    files: Vec<(String, String)>,
    thread: String,
    /// The newest history row that can't be its agent's record.
    after: i64,
    /// T3 Code started it.
    started: bool,
    /// Lookups that failed in a row, and when to ask again.
    failures: u64,
    retry_at: i64,
}

impl Delivered {
    /// Whether a record's words are this message's: the same words,
    /// whitespace aside, then the note T3 Code adds to the agent's message
    /// for each file, in order (`[Attached file "notes.txt" is saved at:
    /// <path>]`).
    fn words(&self, record: &str, taken: &[(String, String)]) -> bool {
        let mut rest = record.trim_end();
        // The notes `tool_media` took out of the record's text (it shows the
        // files themselves), oldest first: each stands for its note.
        let mut taken = taken;
        for (kind, name) in self.files.iter().rev() {
            let note = format!("[Attached {kind} \"{name}\" is saved at: ");
            let Some(start) = rest.rfind(&note) else {
                match taken.split_last() {
                    Some(((k, n), before)) if k == kind && n == name => {
                        taken = before;
                        continue;
                    }
                    _ => return false,
                }
            };
            let path = &rest[start + note.len()..];
            if !path.ends_with(']') || path.contains('\n') {
                return false;
            }
            rest = rest[..start].trim_end();
        }
        rest.split_whitespace().eq(self.body.split_whitespace())
    }
}

/// The conversation's messages sent through T3 Code that show, in the
/// order sent.
fn delivered(sends: Vec<wonder_store::T3CodeSend>) -> Vec<Delivered> {
    sends
        .into_iter()
        .filter(|send| {
            matches!(
                send.state.as_str(),
                "dispatching_to_codex" | "completed" | "uncertain"
            )
        })
        .filter_map(|send| {
            let intent = serde_json::from_str::<Value>(&send.intent).ok()?;
            let ids = |key: &str| -> Vec<String> {
                intent[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|id| id.as_str().map(str::to_owned))
                    .collect()
            };
            let body = intent["providerText"]
                .as_str()
                .unwrap_or(&send.body)
                .to_owned();
            Some(Delivered {
                sent: Sent {
                    message: send.id,
                    t3_message: intent["t3MessageId"].as_str().map(str::to_owned),
                    attachments: ids("attachmentIds"),
                },
                client: send.client_message_id,
                // As a record of it would show.
                body: crate::history::shown_text(
                    "",
                    json!({"type": "userMessage", "content": [{"type": "text", "text": body}]}),
                )
                .unwrap_or(body),
                files: intent["t3Files"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|file| {
                        Some((
                            file["type"].as_str()?.to_owned(),
                            file["name"].as_str()?.to_owned(),
                        ))
                    })
                    .collect(),
                thread: intent["t3ThreadId"].as_str()?.to_owned(),
                after: intent["t3After"].as_i64()?,
                started: intent["t3Started"] == true,
                failures: intent["t3Failures"].as_u64().unwrap_or(0),
                retry_at: intent["t3RetryAtMs"].as_i64().unwrap_or(0),
            })
        })
        .collect()
}

/// A user message an agent recorded itself.
struct Record {
    /// Its history row.
    row: i64,
    turn: String,
    item: String,
    /// The words it shows.
    text: String,
    /// The (type, name) of each file note taken out of `text`
    /// (`tool_media::T3_NOTES_TAKEN`).
    taken: Vec<(String, String)>,
}

/// The user messages agents recorded in the conversation after history
/// row `after`, oldest first.
async fn user_records(state: &AppState, conversation: &str, after: i64) -> Vec<Record> {
    parse_user_records(
        state
            .store
            .t3_code_user_records(conversation, after)
            .await
            .unwrap_or_default(),
    )
}

fn parse_user_records(rows: Vec<(i64, String, String, String)>) -> Vec<Record> {
    rows.into_iter()
        .filter_map(|(row, turn, item, saved)| {
            let saved: Value = serde_json::from_str(&saved).ok()?;
            let taken = saved[crate::tool_media::T3_NOTES_TAKEN]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|note| {
                    Some((
                        note["kind"].as_str()?.to_owned(),
                        note["name"].as_str()?.to_owned(),
                    ))
                })
                .collect();
            let text = crate::history::shown_text(&turn, saved)?;
            Some(Record {
                row,
                turn,
                item,
                text,
                taken,
            })
        })
        .collect()
}

/// T3 Code on the Mac, when it has this conversation's thread open.
pub(crate) async fn holder(
    stored: &StoredProjectConversation,
) -> Option<crate::desktop_activity::Holder> {
    let native = stored.native_session_id.as_deref()?;
    let holder = match stored.family {
        AgentFamily::Claude => crate::desktop_activity::claude_holder(native).await,
        AgentFamily::Codex => crate::desktop_activity::codex_holder(native).await,
        _ => None,
    };
    holder.filter(holds)
}

pub(crate) fn holds(holder: &crate::desktop_activity::Holder) -> bool {
    holder.app == APP && holder.kind == crate::desktop_activity::HolderKind::App
}

/// The T3 thread whose running turn a steer or Stop goes to: T3 Code has it
/// open (a turn it runs holds the thread), Cupola is connected, T3 Code took
/// the last message (or a while has passed), and a lookup found exactly one
/// T3 thread. Never waits for T3 Code: it answers from the last lookup and
/// starts the next one in a task of its own (see the module comment); until
/// a lookup finds the thread, there is none.
pub(crate) async fn route(state: &AppState, stored: &StoredProjectConversation) -> Option<String> {
    route_seen(state, stored, Seen::Unread).await
}

/// The T3 thread that continues this conversation's session, by exact ID
/// only (`owner`), whether or not T3 Code has it open right now. An agent's
/// own turn runs in Cupola, so T3 Code never holds the session then and
/// `route` (which answers where a message goes) finds nothing.
pub(crate) async fn thread_of(
    state: &AppState,
    stored: &StoredProjectConversation,
) -> Option<String> {
    if !state.t3_code.connected() {
        return None;
    }
    if let Some(thread) = native_thread(state, stored).await {
        return Some(thread);
    }
    let native = stored.native_session_id.as_deref()?;
    owner(state, stored, native).await
}

/// `route`, with the holder the caller has already read.
pub(crate) async fn route_seen(
    state: &AppState,
    stored: &StoredProjectConversation,
    seen: Seen<'_>,
) -> Option<String> {
    if !state.t3_code.connected() {
        return None;
    }
    find(state, stored, false, seen).await.ok()
}

/// `route` for a connected Cupola, or why there is no T3 thread. A message
/// (`sending`) goes into a T3 thread T3 Code doesn't have open as well, while
/// T3 Code runs and answers, when T3 Code owns it (`owner`); any other goes
/// at once, without waiting for T3 Code.
async fn find(
    state: &AppState,
    stored: &StoredProjectConversation,
    sending: bool,
    seen: Seen<'_>,
) -> Result<String, Unrouted> {
    let t3 = &state.t3_code;
    if let Some(thread) = native_thread(state, stored).await {
        return if t3.backing_off(&format!("t3:{thread}")) {
            Err(Unrouted::Refused)
        } else if t3.reachable() {
            Ok(thread)
        } else {
            Err(Unrouted::Unreachable)
        };
    }
    let native = stored
        .native_session_id
        .as_deref()
        .ok_or(Unrouted::Unmatched)?;
    let read = |seen: Seen<'_>| match seen {
        Seen::Read(holder) => Some(holder.cloned()),
        Seen::Unread => None,
    };
    let (holder, transcript) = match stored.family {
        AgentFamily::Claude => (
            match read(seen) {
                Some(holder) => holder,
                None => crate::desktop_activity::claude_holder(native).await,
            },
            crate::desktop_activity::claude_session_transcript(native, &stored.cwd),
        ),
        AgentFamily::Codex => (
            match read(seen) {
                Some(holder) => holder,
                None => crate::desktop_activity::codex_holder(native).await,
            },
            crate::desktop_activity::codex_rollout(native),
        ),
        _ => return Err(Unrouted::Unmatched),
    };
    // Another app on the Mac that has the thread open keeps it as without
    // T3 Code (`projects::held_on_mac`).
    let held = holder.as_ref().is_some_and(holds);
    if !held && (holder.is_some() || !sending) {
        return Err(Unrouted::Unmatched);
    }
    if !held && !t3.reachable() {
        return Err(Unrouted::Unreachable);
    }
    if t3.backing_off(native) {
        return Err(Unrouted::Refused);
    }
    if !held {
        return owner(state, stored, native)
            .await
            .ok_or(Unrouted::Unmatched);
    }
    // T3 Code's own store names the thread by the session's exact ID; the
    // messages' words are only asked about a session it doesn't have there
    // (its first message is a link in T3 Code, a compacted session opens
    // with a summary, a long thread outgrows the pages read).
    if let Kept::Thread(thread) = t3.kept(native, stored.family).await {
        return Ok(thread);
    }
    let transcript = transcript.ok_or(Unrouted::Unmatched)?;
    let (thread, start) = t3.known(native);
    if let Some(epoch) = start {
        let state = state.clone();
        let (family, native, cwd) = (stored.family, native.to_owned(), stored.cwd.clone());
        let conversation = stored.conversation_id.clone();
        tokio::spawn(async move {
            let t3 = &state.t3_code;
            let timeout = t3.inner.lock().unwrap().lookup_timeout;
            let lookup = async {
                let prompts =
                    tokio::task::spawn_blocking(move || transcript_prompts(&transcript, family))
                        .await
                        .unwrap_or_default();
                t3.thread_for(family, &cwd, &prompts).await
            };
            let found = tokio::time::timeout(timeout, lookup)
                .await
                .unwrap_or(Err(CallError::Lost));
            if let Err(error) = &found {
                let _ = state.logger.record(
                    "info",
                    "t3_code_match_failed",
                    json!({"conversationId": conversation, "error": format!("{error:?}")}),
                );
            }
            // T3 Code owns it from now on, also after a restart.
            if let Ok(Some(thread)) = &found {
                let _ = state
                    .store
                    .set_t3_code_thread(
                        &conversation,
                        &native,
                        thread,
                        &crate::projects::now_text(),
                    )
                    .await;
            }
            // A message waiting for this thread can go now.
            if t3.finish_lookup(&native, epoch, &found) {
                state.dispatch_wake.notify_one();
            }
        });
    }
    thread
}

/// The T3 thread an OpenCode or Cursor conversation lives in: those have no
/// native session, T3 Code's thread is the whole conversation.
pub(crate) async fn native_thread(
    state: &AppState,
    stored: &StoredProjectConversation,
) -> Option<String> {
    if !matches!(stored.family, AgentFamily::OpenCode | AgentFamily::Cursor) {
        return None;
    }
    state
        .store
        .t3_native_thread(&stored.conversation_id)
        .await
        .ok()
        .flatten()
}

/// The T3 thread T3 Code owns for this conversation's native session, by
/// exact IDs only: T3 Code's own store has the session (`T3Code::kept`),
/// which covers a thread started in T3 Code or with Start in T3 Code even
/// before Cupola saw it open there; otherwise a lookup matched it while T3
/// Code had it open (in memory, or before a restart in Cupola's store).
async fn owner(
    state: &AppState,
    stored: &StoredProjectConversation,
    native: &str,
) -> Option<String> {
    match state.t3_code.kept(native, stored.family).await {
        Kept::Thread(thread) => return Some(thread),
        Kept::Gone => return None,
        Kept::Unknown => {}
    }
    if let Some(thread) = state.t3_code.matched(native) {
        return Some(thread);
    }
    state
        .store
        .t3_code_thread(&stored.conversation_id, native)
        .await
        .ok()
        .flatten()
}

/// What the phone says about a thread T3 Code owns that nothing on the Mac
/// has open (`unheld_note`).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct UnheldNote {
    /// T3 Code isn't running or answering, so Cupola runs the next reply
    /// itself (`runsWithoutT3Code`).
    pub(crate) runs_without: bool,
    /// T3 Code runs but its send can't carry the waiting message, which
    /// waits (`t3CodeCantSend`).
    pub(crate) cant_send: Option<Limit>,
}

/// `UnheldNote` for a thread nothing on the Mac has open.
pub(crate) async fn unheld_note(
    state: &AppState,
    stored: &StoredProjectConversation,
    seen: Seen<'_>,
) -> UnheldNote {
    if !state.t3_code.connected() {
        return UnheldNote::default();
    }
    let held = match seen {
        Seen::Read(holder) => holder.is_some_and(holds),
        Seen::Unread => holder(stored).await.is_some(),
    };
    if held {
        return UnheldNote::default();
    }
    match delivery_seen(state, stored, None, seen).await {
        Delivery::NotFound(Unrouted::Unreachable) => UnheldNote {
            runs_without: match stored.native_session_id.as_deref() {
                Some(native) => owner(state, stored, native).await.is_some(),
                None => false,
            },
            cant_send: None,
        },
        Delivery::CantSend(limit) => UnheldNote {
            runs_without: false,
            cant_send: Some(limit),
        },
        _ => UnheldNote::default(),
    }
}

#[derive(Default, Deserialize)]
pub(crate) struct CatalogQuery {
    #[serde(default)]
    refresh: bool,
    /// Only the new-chat picker needs the live model inventory. Ordinary
    /// status reads must not wait for T3's capabilities tool.
    #[serde(default)]
    catalog: bool,
    search: Option<String>,
}

pub(crate) async fn status(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Query(query): Query<CatalogQuery>,
) -> Response {
    if query
        .search
        .as_ref()
        .is_some_and(|search| search.chars().count() > 200)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Search models with up to 200 characters."})),
        )
            .into_response();
    }
    if query.refresh {
        state.t3_code.check_reachability().await;
    }
    let mut status = state.t3_code.status();
    if (query.catalog || query.search.is_some()) && state.t3_code.connected() {
        match state
            .t3_code
            .launch_capabilities(
                query
                    .search
                    .as_deref()
                    .filter(|search| !search.trim().is_empty()),
            )
            .await
        {
            Ok(listed) => status["launchCatalog"] = picker_catalog(&listed),
            Err(_) => {
                status["launchCatalogError"] = "Couldn’t read T3 Code’s models. Try again.".into()
            }
        }
        // A removed grant during the read must not leave a connected catalog.
        if !state.t3_code.connected() {
            status = state.t3_code.status();
        }
    }
    Json(status).into_response()
}

#[derive(Default, Deserialize)]
pub(crate) struct ConnectChoice {
    #[serde(default)]
    browser: bool,
}

pub(crate) async fn connect(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Query(choice): Query<ConnectChoice>,
) -> Response {
    state.t3_code.log_to(&state.logger);
    let result = if choice.browser {
        state.t3_code.connect_using(true).await
    } else {
        state.t3_code.connect().await
    };
    match result {
        Ok(status) => Json(status).into_response(),
        Err((status, detail)) => (status, Json(json!({ "detail": detail }))).into_response(),
    }
}

pub(crate) async fn disconnect(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    state.t3_code.log_to(&state.logger);
    match state.t3_code.disconnect().await {
        Ok(status) => Json(status).into_response(),
        Err(detail) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "detail": detail })),
        )
            .into_response(),
    }
}

/// A stand-in T3 Code server: its OAuth sign-in (approved by the test, as
/// the owner would in a browser) and the MCP tools Cupola uses, answered as
/// T3 Code does (server-sent events, `isError` failures).
#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use axum::{
        body::Bytes,
        extract::State as AxumState,
        http::HeaderMap,
        routing::{get, post},
        Router,
    };

    /// A T3 thread: its list item, its detail, its user messages (T3 Code's
    /// message ID, text) and the messages waiting in its queue.
    pub(crate) struct Thread {
        item: Value,
        detail: Value,
        messages: Vec<(String, String)>,
        /// Messages waiting for the running turn: (ID, text), by their
        /// queued run's ID.
        queued: Vec<(String, (String, String))>,
        /// The (ID, name) of each file a user message carried, by its ID.
        files: HashMap<String, Vec<(String, String)>>,
    }

    /// How the next `t3_thread_send` goes wrong, as a timeout or a crash
    /// would leave it.
    #[derive(Clone, Copy, Debug, PartialEq)]
    #[allow(clippy::enum_variant_names)] // each names when, relative to applying, the call is lost
    pub(crate) enum Lose {
        /// T3 Code receives the call and never answers or applies it.
        BeforeApplying,
        /// T3 Code applies it and never answers.
        AfterApplying,
        /// T3 Code applies it, then fails the request (HTTP 502).
        ErrorAfterApplying,
    }

    type Issued = (String, u64, Option<Vec<u8>>);

    #[derive(Default)]
    pub(crate) struct Fake {
        pub(crate) origin: String,
        codes: Mutex<HashMap<String, (String, String, String)>>,
        pub(crate) tokens: Mutex<Vec<String>>,
        /// One-time pairing codes T3 Code's command issued, unused yet.
        pairing: Mutex<Vec<String>>,
        missing_decision: std::sync::atomic::AtomicBool,
        decision_reply: Mutex<std::collections::VecDeque<(StatusCode, Value)>>,
        decision_calls: std::sync::atomic::AtomicUsize,
        /// The access each approval here gave (`/oauth/mcp/decision`).
        pub(crate) approved: Mutex<Vec<String>>,
        pub(crate) revoked: std::sync::atomic::AtomicBool,
        /// While set, every MCP request is answered HTTP 403 with this body
        /// (something in front of T3 Code; T3 Code itself answers 401).
        pub(crate) forbidden: Mutex<Option<Value>>,
        /// T3 Code's sessions: each sign-in's `sid`, and those revoked (in
        /// T3 Code's Settings or with `t3 auth session revoke`).
        pub(crate) sessions: Mutex<Vec<String>>,
        pub(crate) ended: Mutex<Vec<String>>,
        pub(crate) projects: Mutex<Vec<Value>>,
        pub(crate) threads: Mutex<Vec<Thread>>,
        /// Every `t3_thread_send` call T3 Code received.
        pub(crate) sends: Mutex<Vec<Value>>,
        /// T3 Code's command receipts: (grant, clientRequestId) -> result.
        /// A command T3 Code already has is not applied again.
        receipts: Mutex<HashMap<(String, String), Value>>,
        /// Thread lookups that reached T3 Code (`orchestrator_capabilities`).
        pub(crate) lookups: std::sync::atomic::AtomicUsize,
        /// Timeline reads that reached T3 Code (`t3_thread_read`).
        pub(crate) reads: std::sync::atomic::AtomicUsize,
        pub(crate) activity_items: Mutex<HashMap<String, Vec<Value>>>,
        /// Every tool call fails with a server error, or is not answered
        /// while `hanging` holds `true`; held calls then fail.
        pub(crate) failing: std::sync::atomic::AtomicBool,
        pub(crate) hanging: tokio::sync::watch::Sender<bool>,
        /// While set, `t3_thread_list` pages after the first are not answered
        /// (until `hanging_pages` is cleared; then they fail).
        pub(crate) hanging_pages: tokio::sync::watch::Sender<bool>,
        /// Sends go to the thread's queue (a turn is running there).
        pub(crate) queue_sends: std::sync::atomic::AtomicBool,
        pub(crate) lose: Mutex<Option<Lose>>,
        /// `t3_thread_send` calls that arrived, answered or not.
        pub(crate) received: std::sync::atomic::AtomicUsize,
        /// Every `t3_queue_cancel`, `t3_queue_edit` and
        /// `t3_queue_promote_to_steer` call: (tool, arguments).
        pub(crate) queue_changes: Mutex<Vec<(String, Value)>>,
        /// Every `t3_thread_send_attachments` and `t3_thread_interrupt` call.
        pub(crate) attachment_sends: Mutex<Vec<Value>>,
        pub(crate) interrupts: Mutex<Vec<Value>>,
        /// Every `t3_thread_organize` call; the test files its effect.
        pub(crate) organized: Mutex<Vec<Value>>,
        /// Every `t3_thread_update` (rename) call.
        pub(crate) renamed: Mutex<Vec<Value>>,
        /// Uploads T3 Code issued: ID -> (name, size, bytes once posted).
        uploads: Mutex<HashMap<String, Issued>>,
        /// Every `t3_thread_launch` call.
        pub(crate) launches: Mutex<Vec<Value>>,
        /// Each thread's `modelSelection` once set, and every configuration write.
        pub(crate) configured: Mutex<HashMap<String, Value>>,
        pub(crate) configures: Mutex<Vec<Value>>,
        /// Tool requests in arrival order, including configuration reads.
        pub(crate) calls: Mutex<Vec<(String, Value)>>,
        /// Its projects have no default model; it lists no providers.
        pub(crate) no_default_model: std::sync::atomic::AtomicBool,
        pub(crate) no_providers: std::sync::atomic::AtomicBool,
        pub(crate) cursor_enabled: std::sync::atomic::AtomicBool,
        /// Its Claude provider is turned off ("Provider instance is disabled.").
        pub(crate) claude_disabled: std::sync::atomic::AtomicBool,
        /// Launches are refused with this message (`invalid_request`).
        pub(crate) refuse_launch: Mutex<Option<String>>,
        /// Which run is active while `queue_sends` (`run-N`); a read bumps
        /// it once when `replace_after_read`, as another run replaced it.
        pub(crate) run: std::sync::atomic::AtomicUsize,
        pub(crate) replace_after_read: std::sync::atomic::AtomicBool,
        /// An interrupted run doesn't end within a `t3_thread_wait`.
        pub(crate) slow_stop: std::sync::atomic::AtomicBool,
        /// An interrupt was requested for the running run: a wait ends it.
        stopping: std::sync::atomic::AtomicBool,
        /// Every `t3_thread_wait` call.
        pub(crate) waits: Mutex<Vec<Value>>,
        /// Runs once as the next upload arrives (the reply changes meanwhile).
        #[allow(clippy::type_complexity)]
        pub(crate) during_upload: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// The questions T3 Code's agents ask now (`{threadId, requestId,
        /// questions}`), the permission requests waiting besides (which its
        /// tools don't list), and every `t3_pending_request_respond` call.
        pub(crate) questions: Mutex<Vec<Value>>,
        pub(crate) consolidated_requests: std::sync::atomic::AtomicBool,
        pub(crate) missing_tool_variant: std::sync::atomic::AtomicUsize,
        pub(crate) permissions: std::sync::atomic::AtomicUsize,
        pub(crate) answers: Mutex<Vec<Value>>,
        /// The next answer is applied, then its reply lost (HTTP 502).
        pub(crate) lose_answer: std::sync::atomic::AtomicBool,
        server: Mutex<Option<tokio::task::AbortHandle>>,
        /// T3 Code's folder (its own store, `T3Code::kept`).
        pub(crate) home: T3Home,
    }

    pub(crate) struct T3Home(pub(crate) tempfile::TempDir);

    impl Default for T3Home {
        fn default() -> Self {
            Self(tempfile::tempdir().unwrap())
        }
    }

    /// The tables of T3 Code 0.1.1's store that `T3Code::kept` reads, as T3
    /// Code creates them (`OrchestrationV2` migrations).
    const T3_STORE_TABLES: [&str; 4] = [
        "CREATE TABLE IF NOT EXISTS orchestration_v2_projection_threads (
          thread_id TEXT PRIMARY KEY, project_id TEXT NOT NULL, title TEXT NOT NULL,
          default_provider TEXT NOT NULL, runtime_mode TEXT NOT NULL,
          interaction_mode TEXT NOT NULL, active_provider_thread_id TEXT,
          created_at TEXT NOT NULL, updated_at TEXT NOT NULL, archived_at TEXT,
          deleted_at TEXT, payload_json TEXT NOT NULL, provider_instance_id TEXT)",
        "CREATE TABLE IF NOT EXISTS orchestration_v2_projection_provider_threads (
          provider_thread_id TEXT PRIMARY KEY, thread_id TEXT, owner_node_id TEXT,
          provider TEXT NOT NULL, provider_session_id TEXT, status TEXT NOT NULL,
          first_run_ordinal INTEGER, last_run_ordinal INTEGER,
          updated_at TEXT NOT NULL, payload_json TEXT NOT NULL,
          driver TEXT, provider_instance_id TEXT)",
        "CREATE INDEX IF NOT EXISTS orchestration_v2_projection_provider_threads_thread_idx
          ON orchestration_v2_projection_provider_threads(thread_id)",
        "CREATE INDEX IF NOT EXISTS orchestration_v2_projection_threads_project_updated_idx
          ON orchestration_v2_projection_threads(project_id, updated_at)",
    ];

    impl Fake {
        pub(crate) async fn start() -> Arc<Self> {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let fake = Arc::new(Self {
                origin: format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port()),
                ..Self::default()
            });
            let app = Router::new()
                .route("/.well-known/oauth-protected-resource/mcp", get(resource))
                .route("/.well-known/oauth-authorization-server", get(server))
                .route("/oauth/mcp/register", post(register))
                .route("/oauth/mcp/token", post(token))
                .route("/oauth/mcp/decision", post(decision))
                .route("/mcp", post(mcp))
                .route(
                    "/api/attachments/upload/{id}",
                    post(upload).layer(axum::extract::DefaultBodyLimit::max(
                        MAX_FILE_BYTES as usize,
                    )),
                )
                .with_state(fake.clone());
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            *fake.server.lock().unwrap() = Some(task.abort_handle());
            // T3 Code names its server in its folder while it runs.
            let userdata = fake.home.0.path().join("userdata");
            std::fs::create_dir_all(&userdata).unwrap();
            std::fs::write(
                userdata.join("server-runtime.json"),
                json!({"pid": std::process::id(), "origin": fake.origin}).to_string(),
            )
            .unwrap();
            fake
        }

        /// T3 Code's own store, written as T3 Code writes it (write-ahead
        /// log; T3 Code requires it).
        pub(crate) async fn store(&self) -> sqlx::SqliteConnection {
            let dir = self.home.0.path().join("userdata");
            std::fs::create_dir_all(&dir).unwrap();
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(dir.join("statev2.sqlite"))
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
            let mut store = sqlx::SqliteConnection::connect_with(&options)
                .await
                .unwrap();
            for table in T3_STORE_TABLES {
                sqlx::query(table).execute(&mut store).await.unwrap();
            }
            store
        }

        /// T3 Code's thread `thread` continues the Claude session or Codex
        /// thread `native` (T3 Code started it, or Cupola's Start in T3 Code
        /// did), as its own store says.
        pub(crate) async fn keeps(&self, thread: &str, driver: &str, native: &str) {
            let provider_thread =
                format!("provider-thread:provider:{driver}:native-thread:{native}");
            let mut store = self.store().await;
            sqlx::query(
                "INSERT INTO orchestration_v2_projection_threads VALUES \
                 (?, 'project', ?, ?, 'full-access', 'default', ?, ?, ?, NULL, NULL, '{}', NULL) \
                 ON CONFLICT(thread_id) DO UPDATE SET \
                 active_provider_thread_id = excluded.active_provider_thread_id",
            )
            .bind(thread)
            .bind(thread)
            .bind(driver)
            .bind(&provider_thread)
            .bind("2026-10-21T10:00:00.000Z")
            .bind("2026-10-21T10:00:00.000Z")
            .execute(&mut store)
            .await
            .unwrap();
            let payload = json!({"id": provider_thread, "driver": driver,
                "providerInstanceId": driver, "appThreadId": thread,
                "nativeThreadRef": {"driver": driver, "nativeId": native, "strength": "strong"},
                "status": "idle"});
            sqlx::query(
                "INSERT INTO orchestration_v2_projection_provider_threads VALUES \
                 (?, ?, NULL, ?, NULL, 'idle', 1, 1, ?, ?, ?, ?)",
            )
            .bind(&provider_thread)
            .bind(thread)
            .bind(driver)
            .bind("2026-10-21T10:00:00.000Z")
            .bind(payload.to_string())
            .bind(driver)
            .bind(driver)
            .execute(&mut store)
            .await
            .unwrap();
            store.close().await.unwrap();
        }

        /// T3 Code's thread `thread` is a `delegate_task` child of T3 thread
        /// `parent`, as its store's lineage says.
        pub(crate) async fn child_of(&self, thread: &str, parent: &str) {
            let mut store = self.store().await;
            sqlx::query(
                "UPDATE orchestration_v2_projection_threads SET payload_json = \
                 json_set(payload_json, '$.lineage', json(?)) WHERE thread_id = ?",
            )
            .bind(
                json!({"parentThreadId": parent, "relationshipToParent": "subagent",
                    "rootThreadId": parent})
                .to_string(),
            )
            .bind(thread)
            .execute(&mut store)
            .await
            .unwrap();
            store.close().await.unwrap();
        }

        /// The owner deletes `thread` in T3 Code's store.
        pub(crate) async fn remove(&self, thread: &str) {
            let mut store = self.store().await;
            sqlx::query(
                "UPDATE orchestration_v2_projection_threads SET deleted_at = ? WHERE thread_id = ?",
            )
            .bind("2026-10-21T11:00:00.000Z")
            .bind(thread)
            .execute(&mut store)
            .await
            .unwrap();
            store.close().await.unwrap();
        }

        /// The owner archived `thread` in T3 Code.
        pub(crate) async fn archive(&self, thread: &str) {
            let mut store = self.store().await;
            sqlx::query(
                "UPDATE orchestration_v2_projection_threads SET archived_at = ? WHERE thread_id = ?",
            )
            .bind("2026-10-21T11:00:00.000Z")
            .bind(thread)
            .execute(&mut store)
            .await
            .unwrap();
            store.close().await.unwrap();
        }

        /// T3 Code quits.
        pub(crate) fn stop(&self) {
            if let Some(server) = self.server.lock().unwrap().take() {
                server.abort();
            }
            let _ = std::fs::remove_file(self.home.0.path().join("userdata/server-runtime.json"));
        }

        /// What the owner's browser does on T3 Code's approval page: approve
        /// (or decline), then follow the redirect back to Cupola.
        pub(crate) async fn answer(&self, authorize: &str, approve: bool) {
            let url = Url::parse(authorize).unwrap();
            let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
            assert_eq!(query["code_challenge_method"], "S256");
            assert_eq!(query["resource"], format!("{}/mcp", self.origin));
            let mut back = Url::parse(&query["redirect_uri"]).unwrap();
            if approve {
                let code = random();
                self.codes.lock().unwrap().insert(
                    code.clone(),
                    (
                        query["client_id"].clone(),
                        query["redirect_uri"].clone(),
                        query["code_challenge"].clone(),
                    ),
                );
                back.query_pairs_mut()
                    .append_pair("code", &code)
                    .append_pair("state", &query["state"])
                    .append_pair("iss", &self.origin);
            } else {
                back.query_pairs_mut()
                    .append_pair("error", "access_denied")
                    .append_pair("state", &query["state"]);
            }
            let page = reqwest::get(back).await.unwrap().text().await.unwrap();
            assert!(page.contains("Cupola"), "{page}");
        }

        /// T3 Code's command on this Mac: each call issues a one-time
        /// pairing code with the scopes `ACCESS` needs.
        pub(crate) fn issuer(self: &Arc<Self>) -> CodeIssuer {
            let fake = self.clone();
            Arc::new(move || {
                let code = random();
                fake.pairing.lock().unwrap().push(code.clone());
                Box::pin(async move { Ok(code) })
            })
        }

        /// T3 Code's `t3 auth session revoke` on this Mac.
        pub(crate) fn revoker(self: &Arc<Self>) -> SessionRevoker {
            let fake = self.clone();
            Arc::new(move |session: String| {
                fake.ended.lock().unwrap().push(session);
                Box::pin(async { Ok(()) })
            })
        }

        /// The owner's Revoke on Cupola's row in T3 Code's Settings.
        pub(crate) fn revoke(&self, token: &str) {
            let sid = token_session(token).expect("T3 Code's token names its session");
            self.ended.lock().unwrap().push(sid);
        }

        /// The Cupola rows T3 Code's Authorized clients list: sessions
        /// issued and not revoked.
        pub(crate) fn listed(&self) -> usize {
            let ended = self.ended.lock().unwrap();
            self.sessions
                .lock()
                .unwrap()
                .iter()
                .filter(|sid| !ended.contains(sid))
                .count()
        }

        pub(crate) fn project(&self, id: &str, root: &str) {
            self.projects
                .lock()
                .unwrap()
                .push(json!({"id": id, "title": id, "workspaceRoot": root}));
        }

        pub(crate) fn thread(
            &self,
            id: &str,
            project: &str,
            driver: &str,
            worktree: Option<&str>,
            messages: &[&str],
        ) {
            let mut threads = self.threads.lock().unwrap();
            let updated = format!("2026-10-19T00:00:{:02}Z", threads.len());
            threads.push(Thread {
                item: json!({"threadId": id, "title": id, "providerInstanceId": driver,
                    "status": "idle", "updatedAt": updated}),
                detail: json!({"threadId": id, "projectId": project, "worktreePath": worktree,
                    "providerInstanceId": driver, "itemCount": messages.len() * 2}),
                messages: messages
                    .iter()
                    .enumerate()
                    .map(|(n, text)| (format!("message:{id}:{n}"), (*text).to_owned()))
                    .collect(),
                queued: Vec::new(),
                files: HashMap::new(),
            });
        }

        /// A T3 `delegate_task` child of `parent`, with its brief and result.
        pub(crate) fn subtask(
            &self,
            id: &str,
            parent: &str,
            project: &str,
            status: &str,
            messages: &[&str],
        ) {
            self.thread(id, project, "claudeAgent", None, messages);
            let mut threads = self.threads.lock().unwrap();
            let thread = threads
                .iter_mut()
                .find(|candidate| candidate.item["threadId"] == id)
                .unwrap();
            thread.item["parentThreadId"] = parent.into();
            thread.item["relationshipToParent"] = "subagent".into();
            thread.item["status"] = status.into();
        }

        /// The owner types `text` into the T3 thread in T3 Code: it waits
        /// in the queue while `queued` (a turn runs there).
        pub(crate) fn typed(&self, thread: &str, text: &str, queued: bool) {
            let mut threads = self.threads.lock().unwrap();
            let thread = threads
                .iter_mut()
                .find(|candidate| candidate.item["threadId"] == thread)
                .unwrap();
            let message = (format!("message:{}", random()), text.to_owned());
            if queued {
                thread
                    .queued
                    .push((format!("queued-{}", random()), message));
            } else {
                thread.messages.push(message);
            }
        }

        /// T3 Code starts the next message waiting in the T3 thread.
        pub(crate) fn start_next(&self, thread: &str) {
            let mut threads = self.threads.lock().unwrap();
            let thread = threads
                .iter_mut()
                .find(|candidate| candidate.item["threadId"] == thread)
                .unwrap();
            let (_, next) = thread.queued.remove(0);
            thread.messages.push(next);
        }

        /// The owner cancels the first message waiting in the T3 thread's
        /// queue in T3 Code, or edits it to `text`.
        pub(crate) fn change_queued_here(&self, thread: &str, text: Option<&str>) {
            let mut threads = self.threads.lock().unwrap();
            let thread = threads
                .iter_mut()
                .find(|candidate| candidate.item["threadId"] == thread)
                .unwrap();
            match text {
                Some(text) => thread.queued[0].1 .1 = text.to_owned(),
                None => {
                    thread.queued.remove(0);
                }
            }
        }

        /// The owner deletes the T3 thread in T3 Code.
        pub(crate) fn delete_thread(&self, thread: &str) {
            self.threads
                .lock()
                .unwrap()
                .retain(|candidate| candidate.item["threadId"] != thread);
        }

        /// The queued runs waiting in the T3 thread, in order.
        pub(crate) fn queued_runs(&self, thread: &str) -> Vec<String> {
            let threads = self.threads.lock().unwrap();
            let thread = threads
                .iter()
                .find(|candidate| candidate.item["threadId"] == thread)
                .unwrap();
            thread.queued.iter().map(|(run, _)| run.clone()).collect()
        }

        /// What was posted to an upload T3 Code issued.
        pub(crate) fn uploaded(&self, name: &str) -> Option<Vec<u8>> {
            self.uploads
                .lock()
                .unwrap()
                .values()
                .find(|(file, _, _)| file == name)
                .and_then(|(_, _, bytes)| bytes.clone())
        }

        /// Sends T3 Code applied (not the calls it received).
        pub(crate) fn delivered(&self) -> usize {
            self.receipts.lock().unwrap().len()
        }

        /// The T3 thread's user messages, oldest first, with what waits in
        /// its queue after them.
        pub(crate) fn texts(&self, thread: &str) -> Vec<String> {
            let threads = self.threads.lock().unwrap();
            let thread = threads
                .iter()
                .find(|candidate| candidate.item["threadId"] == thread)
                .unwrap();
            thread
                .messages
                .iter()
                .map(|(_, text)| text.clone())
                .chain(thread.queued.iter().map(|(_, (_, text))| text.clone()))
                .collect()
        }

        /// `t3_thread_send` as T3 Code runs it: the command is keyed by the
        /// caller's grant and `clientRequestId`, and one it already has
        /// returns the first result without being applied again.
        fn send(&self, grant: &str, arguments: &Value) -> Result<Value, (String, String)> {
            let key = arguments["clientRequestId"].as_str().unwrap_or_default();
            let receipt = (grant.to_owned(), key.to_owned());
            if let Some(result) = self.receipts.lock().unwrap().get(&receipt) {
                return Ok(result.clone());
            }
            let mut threads = self.threads.lock().unwrap();
            let thread = threads
                .iter_mut()
                .find(|thread| thread.item["threadId"] == arguments["threadId"])
                .ok_or(("thread_not_found".to_owned(), "No such thread.".to_owned()))?;
            let message = format!(
                "message:mcp:{}:thread-send:{}",
                uri_component(&format!("client:{grant}")),
                uri_component(key)
            );
            let text = arguments["message"].as_str().unwrap_or_default().to_owned();
            // `queue_sends`: a turn runs there. Steer and restart need one.
            let running = self.queue_sends.load(std::sync::atomic::Ordering::SeqCst);
            let mode = arguments["mode"].as_str().unwrap_or("auto");
            let delivery = match (mode, running) {
                ("steer", true) => "steered",
                ("restart", true) => "restarted",
                ("steer" | "restart", false) => {
                    return Err((
                        "thread_not_sendable".into(),
                        "The thread has no steerable run.".into(),
                    ))
                }
                ("auto", true) => "steered",
                (_, true) => "queued",
                (_, false) => "started",
            };
            let queued = delivery == "queued";
            // A queued message is a queued run of its own (T3 Code's
            // `queuedRunId`); the others join or start the active run.
            let run = if queued {
                let run = format!("queued-{}", random());
                thread.queued.push((run.clone(), (message.clone(), text)));
                run
            } else {
                thread.messages.push((message.clone(), text));
                "run".to_owned()
            };
            let result = json!({"threadId": arguments["threadId"], "messageId": message,
                "runId": run, "status": if queued { "queued" } else { "running" },
                "delivery": delivery});
            self.receipts
                .lock()
                .unwrap()
                .insert(receipt, result.clone());
            Ok(result)
        }

        /// The run active in every thread, while a turn runs.
        fn active_run(&self) -> Option<String> {
            use std::sync::atomic::Ordering::SeqCst;
            self.queue_sends
                .load(SeqCst)
                .then(|| format!("run-{}", self.run.load(SeqCst)))
        }

        /// `t3_thread_send_attachments` as T3 Code runs it: every file must
        /// be an upload it issued and received in full; each call is a new
        /// message (no retry key).
        fn send_attachments(&self, arguments: &Value) -> Result<Value, (String, String)> {
            let uploads = self.uploads.lock().unwrap();
            let mut names = Vec::new();
            for file in arguments["attachments"].as_array().into_iter().flatten() {
                let id = file["id"].as_str().unwrap_or_default();
                match uploads.get(id) {
                    Some((name, size, Some(bytes)))
                        if bytes.len() as u64 == *size && file["name"] == name.as_str() =>
                    {
                        names.push((id.to_owned(), name.clone()))
                    }
                    _ => {
                        return Err((
                            "invalid_request".into(),
                            "Attachments must be pending uploads.".into(),
                        ))
                    }
                }
            }
            if names.is_empty() {
                return Err(("invalid_request".into(), "No attachments.".into()));
            }
            let mut threads = self.threads.lock().unwrap();
            let thread = threads
                .iter_mut()
                .find(|thread| thread.item["threadId"] == arguments["threadId"])
                .ok_or(("thread_not_found".to_owned(), "No such thread.".to_owned()))?;
            let message = format!("message:{}", random());
            let text = arguments["message"].as_str().unwrap_or_default().to_owned();
            thread.messages.push((message.clone(), text));
            thread.files.insert(message.clone(), names);
            Ok(
                json!({"threadId": arguments["threadId"], "messageId": message,
                "runId": "run", "status": "running"}),
            )
        }

        fn tool(
            &self,
            grant: &str,
            name: &str,
            arguments: &Value,
        ) -> Result<Value, (String, String)> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_owned(), arguments.clone()));
            let consolidated = self
                .consolidated_requests
                .load(std::sync::atomic::Ordering::SeqCst);
            if (name.starts_with("t3_pending_request_") && consolidated)
                || (name == "t3_pending_request" && !consolidated)
            {
                let message = match self
                    .missing_tool_variant
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    1 => format!("Tool '{name}' not found. Available tools: t3_pending_request"),
                    2 => format!("Tool \"{name}\" not found"),
                    3 => format!("Tool \"{name}\" not found. Available tools: t3_pending_request"),
                    _ => format!("Tool '{name}' not found"),
                };
                return Err(("tool_not_found".into(), message));
            }
            let mut arguments = arguments.clone();
            let name = if name == "t3_pending_request" {
                match arguments
                    .as_object_mut()
                    .and_then(|a| a.remove("action"))
                    .as_ref()
                    .and_then(Value::as_str)
                {
                    Some("list") => "t3_pending_request_list",
                    Some("read") => "t3_pending_request_read",
                    Some("respond") => "t3_pending_request_respond",
                    _ => return Err(("invalid_request".into(), "action".into())),
                }
            } else {
                name
            };
            let arguments = &arguments;
            // T3 Code's schema takes only a non-negative position.
            if arguments
                .get("afterPosition")
                .is_some_and(|after| after.as_u64().is_none())
            {
                return Err(("invalid_request".into(), "afterPosition".into()));
            }
            match name {
                "orchestrator_capabilities" => {
                    if self.no_providers.load(std::sync::atomic::Ordering::SeqCst) {
                        return Ok(json!({"providers": []}));
                    }
                    // As T3 Code lists them: a few models per provider, the
                    // rest (Codex's "gpt-6.1-pro") only to a `search` for them.
                    let search = arguments["search"].as_str();
                    let models = |listed: &[&str], omitted: &[&str]| -> Vec<Value> {
                        listed.iter().chain(if search.is_some() { omitted } else { &[] })
                            .filter(|id| search.is_none_or(|query| id.contains(query)))
                            .map(|id| json!({"id": id, "options": [
                                {"id": "effort", "values": ["low", "medium", "high"], "default": "medium"},
                                {"id": "reasoningEffort", "values": ["low", "medium", "high"], "default": "medium"},
                                {"id": "fastMode", "values": [false, true], "default": false},
                                {"id": "serviceTier", "values": ["default", "fast"], "default": "default"}]})).collect()
                    };
                    let claude_constraints: Vec<&str> = if self.claude_disabled.load(std::sync::atomic::Ordering::SeqCst) {
                        vec!["Provider instance is disabled."]
                    } else {
                        vec![]
                    };
                    Ok(json!({"providers": [
                        {"providerInstanceId": "claudeAgent", "driverKind": "claudeAgent",
                            "runtimeModes": ["approval-required", "auto-accept-edits", "full-access"],
                            "models": models(&["claude-opus-5-5", "claude-sonnet-5-5"], &[]),
                            "constraints": claude_constraints},
                        {"providerInstanceId": "codex", "driverKind": "codex",
                            "runtimeModes": ["approval-required", "full-access"],
                            "models": models(&["gpt-6.1", "gpt-6.1-mini"], &["gpt-6.1-pro"]),
                            "constraints": []},
                        {"providerInstanceId": "opencode", "driverKind": "opencode",
                            "models": models(&["opencode/deepseek-v4.1-flash-free"], &[]),
                            "constraints": []},
                        {"providerInstanceId": "cursor", "driverKind": "cursor",
                            "models": models(&["grok-4.7"], &[]),
                            "constraints": if self.cursor_enabled.load(std::sync::atomic::Ordering::SeqCst) {
                                Vec::<&str>::new()
                            } else { vec!["Provider instance is disabled."] }}]}))
                }
                "t3_project_list" => {
                    Ok(json!({"projects": *self.projects.lock().unwrap(), "nextCursor": null}))
                }
                "t3_thread_list" => {
                    // Pages by offset, as T3 Code does.
                    let listed: Vec<Value> = self
                        .threads
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|thread| thread.detail["projectId"] == arguments["projectId"])
                        .map(|thread| thread.item.clone())
                        .collect();
                    let from = arguments["cursor"].as_u64().unwrap_or(0) as usize;
                    let to = (from + arguments["limit"].as_u64().unwrap_or(100) as usize)
                        .min(listed.len());
                    Ok(json!({"threads": listed[from.min(to)..to],
                        "nextCursor": (to < listed.len()).then_some(to)}))
                }
                "t3_thread_read" => {
                    let threads = self.threads.lock().unwrap();
                    let thread = threads
                        .iter()
                        .find(|thread| thread.item["threadId"] == arguments["threadId"])
                        .ok_or(("thread_not_found".to_owned(), "No such thread.".to_owned()))?;
                    // A user message and a reply per turn, 3 items a page.
                    let after = arguments["afterPosition"].as_i64().unwrap_or(-1);
                    let items: Vec<Value> = if arguments["view"] == "activity" {
                        self.activity_items.lock().unwrap().get(arguments["threadId"].as_str().unwrap_or_default()).cloned()
                    } else { None }.unwrap_or_else(|| thread
                        .messages
                        .iter()
                        .enumerate()
                        .flat_map(|(turn, (id, text))| {
                            let files: Vec<Value> = thread.files.get(id).into_iter().flatten()
                                .map(|(file, name)| json!({"type": "file", "id": file, "name": name}))
                                .collect();
                            [
                                json!({"position": turn * 2, "type": "user_message", "messageId": id, "text": text,
                                    "attachments": files}),
                                json!({"position": turn * 2 + 1, "type": "assistant_message", "text": "ok"}),
                            ]
                        })
                        .collect()).into_iter()
                        .filter(|item| item["position"].as_i64() > Some(after))
                        .collect();
                    let mut detail = thread.detail.clone();
                    detail["activeRunId"] = self.active_run().into();
                    // T3 Code counts every request waiting, questions or not.
                    detail["pendingRequestCount"] = (self
                        .questions
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|question| question["threadId"] == arguments["threadId"])
                        .count()
                        + self.permissions.load(std::sync::atomic::Ordering::SeqCst))
                    .into();
                    if self
                        .replace_after_read
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        self.run.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(
                        json!({"thread": detail, "items": items.iter().take(3).collect::<Vec<_>>(),
                        "hasMore": items.len() > 3}),
                    )
                }
                "t3_pending_request_list" => Ok(json!({"requestIds": self.questions.lock().unwrap()
                    .iter()
                    .filter(|question| question["threadId"] == arguments["threadId"])
                    .map(|question| question["requestId"].clone())
                    .collect::<Vec<_>>()})),
                "t3_pending_request_read" => self
                    .questions
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|question| question["requestId"] == arguments["requestId"])
                    .map(|question| json!({"requestId": question["requestId"], "questions": question["questions"]}))
                    .ok_or(("invalid_request".into(), "The pending user-input request was not found.".into())),
                "t3_pending_request_respond" => {
                    self.answers.lock().unwrap().push(arguments.clone());
                    let mut questions = self.questions.lock().unwrap();
                    let before = questions.len();
                    questions.retain(|question| question["requestId"] != arguments["requestId"]);
                    if questions.len() == before {
                        return Err(("invalid_request".into(), "The user-input request was already answered.".into()));
                    }
                    Ok(json!({"sequence": 1}))
                }
                "t3_queue_list" => {
                    let threads = self.threads.lock().unwrap();
                    let thread = threads
                        .iter()
                        .find(|thread| thread.item["threadId"] == arguments["threadId"])
                        .ok_or(("thread_not_found".to_owned(), "No such thread.".to_owned()))?;
                    Ok(json!({"items": thread.queued.iter()
                        .map(|(run, (_, text))| json!({"queuedRunId": run, "text": text, "truncated": false}))
                        .collect::<Vec<_>>(), "nextCursor": null}))
                }
                "t3_queue_cancel" | "t3_queue_edit" | "t3_queue_promote_to_steer" => {
                    self.queue_changes.lock().unwrap().push((name.to_owned(), arguments.clone()));
                    let active = self.active_run();
                    let mut threads = self.threads.lock().unwrap();
                    let thread = threads
                        .iter_mut()
                        .find(|thread| thread.item["threadId"] == arguments["threadId"])
                        .ok_or(("thread_not_found".to_owned(), "No such thread.".to_owned()))?;
                    // T3 Code refuses a run that is no longer queued.
                    let index = thread
                        .queued
                        .iter()
                        .position(|(run, _)| arguments["queuedRunId"] == run.as_str())
                        .ok_or(("orchestration_error".to_owned(), "The run is not queued.".to_owned()))?;
                    match name {
                        "t3_queue_cancel" => {
                            thread.queued.remove(index);
                        }
                        "t3_queue_edit" => {
                            thread.queued[index].1 .1 =
                                arguments["text"].as_str().unwrap_or_default().to_owned();
                        }
                        _ => {
                            if active.is_none() || arguments["targetRunId"] != active.as_deref().unwrap_or_default() {
                                return Err(("orchestration_error".into(), "The target run is not active.".into()));
                            }
                            let (_, message) = thread.queued.remove(index);
                            thread.messages.push(message);
                        }
                    }
                    Ok(json!({"sequence": 1}))
                }
                "t3_thread_send" => {
                    self.sends.lock().unwrap().push(arguments.clone());
                    self.send(grant, arguments)
                }
                "t3_attachment_prepare_upload" => {
                    let upload = &arguments["upload"];
                    let id = format!("pending-{}", random());
                    self.uploads.lock().unwrap().insert(
                        id.clone(),
                        (
                            upload["name"].as_str().unwrap_or_default().to_owned(),
                            upload["sizeBytes"].as_u64().unwrap_or_default(),
                            None,
                        ),
                    );
                    Ok(
                        json!({"attachmentId": id, "relativeUrl": format!("/api/attachments/upload/{id}"),
                        "expiresAt": now_ms() + 600_000}),
                    )
                }
                "t3_thread_send_attachments" => {
                    self.attachment_sends
                        .lock()
                        .unwrap()
                        .push(arguments.clone());
                    self.send_attachments(arguments)
                }
                "t3_thread_organize" => {
                    self.organized.lock().unwrap().push(arguments.clone());
                    Ok(json!({"threadId": arguments["threadId"], "action": arguments["action"]}))
                }
                "t3_thread_update" => {
                    self.renamed.lock().unwrap().push(arguments.clone());
                    Ok(json!({"threadId": arguments["threadId"], "title": arguments["title"]}))
                }
                "t3_thread_interrupt" => {
                    self.interrupts.lock().unwrap().push(arguments.clone());
                    let active = self.active_run();
                    match arguments["runId"].as_str() {
                        // A run that ended answers its final status.
                        Some(run) if active.as_deref() != Some(run) => Ok(
                            json!({"threadId": arguments["threadId"], "runId": run, "status": "completed"}),
                        ),
                        _ => {
                            self.stopping
                                .store(active.is_some(), std::sync::atomic::Ordering::SeqCst);
                            Ok(json!({"threadId": arguments["threadId"], "runId": active,
                            "status": if active.is_some() { "interrupt_requested" } else { "no_active_run" }}))
                        }
                    }
                }
                "t3_thread_wait" => {
                    use std::sync::atomic::Ordering::SeqCst;
                    self.waits.lock().unwrap().push(arguments.clone());
                    // Waiting changes nothing: only a run asked to stop ends.
                    let running = self.queue_sends.load(SeqCst);
                    if running && (self.slow_stop.load(SeqCst) || !self.stopping.load(SeqCst)) {
                        return Ok(
                            json!({"threadId": arguments["threadId"], "runId": arguments["runId"],
                            "status": "running", "timedOut": true}),
                        );
                    }
                    self.queue_sends.store(false, SeqCst);
                    self.stopping.store(false, SeqCst);
                    Ok(
                        json!({"threadId": arguments["threadId"], "runId": arguments["runId"],
                        "status": if running { "interrupted" } else { "completed" }, "timedOut": false}),
                    )
                }
                "t3_thread_configure" if arguments.get("modelSelection").is_none() => {
                    let thread = arguments["threadId"].as_str().unwrap_or_default();
                    let threads = self.threads.lock().unwrap();
                    let item = &threads
                        .iter()
                        .find(|candidate| candidate.item["threadId"] == thread)
                        .ok_or(("thread_not_found".to_owned(), "No such thread.".to_owned()))?
                        .item;
                    let instance = item["providerInstanceId"].clone();
                    let model = if instance == "codex" {
                        "gpt-6.1"
                    } else {
                        "claude-sonnet-5-5"
                    };
                    let selection = self
                        .configured
                        .lock()
                        .unwrap()
                        .get(thread)
                        .cloned()
                        .unwrap_or_else(|| {
                            json!({"instanceId": instance, "model": model,
                            "options": {"effort": "medium"}})
                        });
                    Ok(json!({"threadId": thread, "modelSelection": selection,
                        "runtimeMode": "full-access", "interactionMode": "default"}))
                }
                "t3_thread_configure" => {
                    self.configures.lock().unwrap().push(arguments.clone());
                    let thread = arguments["threadId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    // T3 Code keeps the options and answers them by id.
                    let mut selection = arguments["modelSelection"].clone();
                    if let Some(options) = selection["options"].as_array().cloned() {
                        selection["options"] = options
                            .iter()
                            .map(|option| {
                                (
                                    option["id"].as_str().unwrap_or_default().to_owned(),
                                    option["value"].clone(),
                                )
                            })
                            .collect::<serde_json::Map<_, _>>()
                            .into();
                    }
                    self.configured
                        .lock()
                        .unwrap()
                        .insert(thread.clone(), selection);
                    Ok(json!({"threadId": thread}))
                }
                "t3_thread_launch" => {
                    self.launches.lock().unwrap().push(arguments.clone());
                    let scratch = arguments["scratch"].as_bool() == Some(true);
                    if scratch && arguments.get("projectId").is_some() {
                        return Err((
                            "invalid_request".into(),
                            "scratch:true picks its own project and folder; omit projectId and workspaceStrategy."
                                .into(),
                        ));
                    }
                    let project = if scratch {
                        "t3-scratch-project"
                    } else {
                        arguments["projectId"].as_str().unwrap_or_default()
                    };
                    if !scratch
                        && !self
                            .projects
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|p| p["id"] == project)
                    {
                        return Err((
                            "invalid_request".into(),
                            "The project was not found.".into(),
                        ));
                    }
                    if let Some(message) = self.refuse_launch.lock().unwrap().clone() {
                        return Err(("invalid_request".into(), message));
                    }
                    let selection = match arguments.get("modelSelection") {
                        Some(selection) => selection.clone(),
                        None if self
                            .no_default_model
                            .load(std::sync::atomic::Ordering::SeqCst) =>
                        {
                            return Err(("invalid_request".into(), "Pass modelSelection: the project \
                                has no default model. orchestrator_capabilities lists providers and models."
                                .into()));
                        }
                        None => json!({"instanceId": "claudeAgent", "model": "default"}),
                    };
                    if scratch {
                        // T3 Code makes a project and folder of its own.
                        self.project(project, "/t3-home/scratch/2026-11-06-no-project-ab12cd34");
                    }
                    let id = format!("t3-launched-{}", self.launches.lock().unwrap().len());
                    let message = arguments["message"].as_str();
                    self.thread(
                        &id,
                        project,
                        selection["instanceId"].as_str().unwrap_or_default(),
                        None,
                        &message.into_iter().collect::<Vec<_>>(),
                    );
                    Ok(json!({"threadId": id, "projectId": project,
                        "modelSelection": selection,
                        "runId": message.map(|_| "run"), "status": message.map(|_| "running")}))
                }
                _ => Err(("tool_not_found".into(), format!("Tool '{name}' not found"))),
            }
        }
    }

    /// T3 Code's signed upload URL: the exact bytes it was issued for.
    async fn upload(
        AxumState(fake): AxumState<Arc<Fake>>,
        axum::extract::Path(id): axum::extract::Path<String>,
        body: Bytes,
    ) -> StatusCode {
        let during = fake.during_upload.lock().unwrap().take();
        if let Some(during) = during {
            during();
        }
        match fake.uploads.lock().unwrap().get_mut(&id) {
            Some((_, size, bytes)) if body.len() as u64 == *size => {
                *bytes = Some(body.to_vec());
                StatusCode::NO_CONTENT
            }
            Some(_) => StatusCode::BAD_REQUEST,
            None => StatusCode::NOT_FOUND,
        }
    }

    async fn resource(AxumState(fake): AxumState<Arc<Fake>>) -> Json<Value> {
        Json(
            json!({"resource": format!("{}/mcp", fake.origin), "authorization_servers": [fake.origin],
            "scopes_supported": ["orchestration:read", "orchestration:operate"], "bearer_methods_supported": ["header"], "resource_name": "T3 Code"}),
        )
    }

    async fn server(AxumState(fake): AxumState<Arc<Fake>>) -> Json<Value> {
        let o = &fake.origin;
        Json(
            json!({"issuer": o, "authorization_endpoint": format!("{o}/oauth/mcp/{}", if fake.missing_decision.load(std::sync::atomic::Ordering::SeqCst) { "consent" } else { "authorize" }),
            "token_endpoint": format!("{o}/oauth/mcp/token"), "registration_endpoint": format!("{o}/oauth/mcp/register"),
            "response_types_supported": ["code"], "grant_types_supported": ["authorization_code"],
            "code_challenge_methods_supported": ["S256"], "token_endpoint_auth_methods_supported": ["none"],
            "authorization_response_iss_parameter_supported": true}),
        )
    }

    async fn register(Json(body): Json<Value>) -> Response {
        let redirect = body["redirect_uris"][0].as_str().unwrap_or_default();
        if !redirect.starts_with("http://127.0.0.1:")
            || body["token_endpoint_auth_method"] != "none"
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid_redirect_uri"})),
            )
                .into_response();
        }
        (StatusCode::CREATED, Json(json!({"client_id": format!("signed:{redirect}"), "client_name": body["client_name"],
            "redirect_uris": body["redirect_uris"], "grant_types": ["authorization_code"],
            "response_types": ["code"], "token_endpoint_auth_method": "none"}))).into_response()
    }

    async fn token(AxumState(fake): AxumState<Arc<Fake>>, body: Bytes) -> Response {
        let form: HashMap<String, String> =
            Url::parse(&format!("http://f/?{}", String::from_utf8_lossy(&body)))
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect();
        let issued = fake
            .codes
            .lock()
            .unwrap()
            .remove(form.get("code").map_or("", |c| c));
        let valid = issued.is_some_and(|(client, redirect, challenge)| {
            form.get("grant_type").map(String::as_str) == Some("authorization_code")
                && form.get("client_id") == Some(&client)
                && form.get("redirect_uri") == Some(&redirect)
                && form.get("code_verifier").is_some_and(|verifier| {
                    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == challenge
                })
        });
        if !valid {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid_grant", "error_description": "bad code"})),
            )
                .into_response();
        }
        // T3 Code's token: its claims, then their signature.
        let sid = format!("session-{}", random());
        let claims = json!({"v": 1, "kind": "session", "sid": sid, "sub": "mcp-client",
            "exp": now_ms() + 2_592_000_000_i64});
        let access = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(claims.to_string()),
            random()
        );
        fake.sessions.lock().unwrap().push(sid);
        fake.tokens.lock().unwrap().push(access.clone());
        Json(json!({"access_token": access, "token_type": "Bearer", "expires_in": 2_592_000, "scope": "orchestration:read orchestration:operate"})).into_response()
    }

    /// T3 Code's approval page's decision, with a pairing code: the code
    /// is used up, and T3 Code sends the redirect with its code back.
    async fn decision(AxumState(fake): AxumState<Arc<Fake>>, Json(body): Json<Value>) -> Response {
        fake.decision_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some((status, reply)) = fake.decision_reply.lock().unwrap().pop_front() {
            if reply == json!("not_json") {
                return (status, "secret-invalid-response").into_response();
            }
            let reply = if reply == json!("wrong_state") {
                json!({"redirectTo": format!("{}?state=wrong", body["authorization"]["redirect_uri"].as_str().unwrap())})
            } else if reply == json!("missing_code") {
                json!({"redirectTo": format!("{}?state={}", body["authorization"]["redirect_uri"].as_str().unwrap(), body["authorization"]["state"].as_str().unwrap())})
            } else {
                reply
            };
            return (status, Json(reply)).into_response();
        }
        let request = &body["authorization"];
        let decision = &body["decision"];
        let code = decision["code"].as_str().unwrap_or_default().to_owned();
        let mut pairing = fake.pairing.lock().unwrap();
        let Some(index) = pairing.iter().position(|issued| *issued == code) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"_tag": "AuthMcpApprovalError",
                "message": "That code is unknown or already used."})),
            )
                .into_response();
        };
        pairing.remove(index);
        assert_eq!(decision["_tag"], "pairing-code");
        assert_eq!(request["code_challenge_method"], "S256");
        assert_eq!(request["resource"], format!("{}/mcp", fake.origin));
        let redirect = request["redirect_uri"].as_str().unwrap();
        let issued = random();
        fake.codes.lock().unwrap().insert(
            issued.clone(),
            (
                request["client_id"].as_str().unwrap().to_owned(),
                redirect.to_owned(),
                request["code_challenge"].as_str().unwrap().to_owned(),
            ),
        );
        fake.approved
            .lock()
            .unwrap()
            .push(decision["access"].as_str().unwrap_or_default().to_owned());
        let mut back = Url::parse(redirect).unwrap();
        back.query_pairs_mut()
            .append_pair("code", &issued)
            .append_pair("state", request["state"].as_str().unwrap())
            .append_pair("iss", &fake.origin);
        Json(json!({"redirectTo": back.as_str()})).into_response()
    }

    async fn mcp(
        AxumState(fake): AxumState<Arc<Fake>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        let bearer = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();
        if let Some(body) = fake.forbidden.lock().unwrap().clone() {
            return (StatusCode::FORBIDDEN, Json(body)).into_response();
        }
        if fake.revoked.load(std::sync::atomic::Ordering::SeqCst)
            || token_session(bearer).is_some_and(|sid| fake.ended.lock().unwrap().contains(&sid))
            || !fake
                .tokens
                .lock()
                .unwrap()
                .iter()
                .any(|token| token == bearer)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let id = body.get("id").cloned();
        match body["method"].as_str() {
            Some("initialize") => (
                [("mcp-session-id", "session-1")],
                Json(json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": PROTOCOL, "capabilities": {"tools": {}}, "serverInfo": {"name": "t3"}}})),
            )
                .into_response(),
            Some("tools/call") => {
                assert_eq!(headers.get("mcp-session-id").unwrap(), "session-1");
                let params = &body["params"];
                let name = params["name"].as_str().unwrap_or_default();
                if name == "orchestrator_capabilities" {
                    fake.lookups
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                if name == "t3_thread_read" {
                    fake.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                let mut hanging = fake.hanging.subscribe();
                if *hanging.borrow_and_update() {
                    let _ = hanging.wait_for(|hanging| !hanging).await;
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                if name == "t3_thread_list" && !params["arguments"]["cursor"].is_null() {
                    let mut pages = fake.hanging_pages.subscribe();
                    if *pages.borrow_and_update() {
                        let _ = pages.wait_for(|hanging| !hanging).await;
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                }
                if fake.failing.load(std::sync::atomic::Ordering::SeqCst) {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                if name == "t3_thread_send" || name == "t3_thread_send_attachments" {
                    fake.received
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let lose = fake.lose.lock().unwrap().take();
                    let sends = match name {
                        "t3_thread_send" => &fake.sends,
                        _ => &fake.attachment_sends,
                    };
                    if lose.is_some() {
                        sends.lock().unwrap().push(params["arguments"].clone());
                    }
                    match lose {
                        Some(Lose::BeforeApplying) => std::future::pending::<()>().await,
                        Some(lose) => {
                            let _ = match name {
                                "t3_thread_send" => fake.send(bearer, &params["arguments"]),
                                _ => fake.send_attachments(&params["arguments"]),
                            };
                            if lose == Lose::AfterApplying {
                                std::future::pending::<()>().await;
                            }
                            return StatusCode::BAD_GATEWAY.into_response();
                        }
                        None => {}
                    }
                }
                if ((name == "t3_pending_request_respond" && !fake.consolidated_requests.load(std::sync::atomic::Ordering::SeqCst))
                    || (name == "t3_pending_request" && params["arguments"]["action"] == "respond"))
                    && fake
                        .lose_answer
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = fake.tool(bearer, name, &params["arguments"]);
                    return StatusCode::BAD_GATEWAY.into_response();
                }
                let result = match fake.tool(bearer, name, &params["arguments"]) {
                    Ok(value) => json!({"content": [{"type": "text", "text": value.to_string()}], "structuredContent": value}),
                    Err((code, message)) if code == "tool_not_found" => {
                        let event = json!({"jsonrpc": "2.0", "id": id,
                            "error": {"code": -32602, "message": message}});
                        return ([("content-type", "text/event-stream")],
                            format!("event: message\ndata: {event}\n\n")).into_response();
                    }
                    Err((code, message)) => json!({"isError": true, "content": [{"type": "text", "text": json!({"_tag": "OrchestratorMcpFailure", "code": code, "message": message}).to_string()}]}),
                };
                let event = json!({"jsonrpc": "2.0", "id": id, "result": result});
                ([("content-type", "text/event-stream")], format!("event: message\ndata: {event}\n\n")).into_response()
            }
            _ => StatusCode::ACCEPTED.into_response(),
        }
    }

    /// A Cupola connected to `fake`, as the owner connects it in Settings.
    pub(crate) async fn connected(fake: &Arc<Fake>) -> (Arc<T3Code>, Arc<MemoryGrantStore>) {
        connected_with(fake, |t3| t3).await
    }

    /// `connected`, with the client adjusted before it connects.
    pub(crate) async fn connected_with(
        fake: &Arc<Fake>,
        adjust: impl FnOnce(T3Code) -> T3Code,
    ) -> (Arc<T3Code>, Arc<MemoryGrantStore>) {
        let (opened, mut urls) = tokio::sync::mpsc::unbounded_channel::<String>();
        let store = Arc::new(MemoryGrantStore::default());
        let mut t3 = T3Code::new(
            store.clone(),
            None,
            immediate_opener(move |url: &str| {
                opened.send(url.to_owned()).unwrap();
                Ok(())
            }),
        );
        // T3 Code's folder, where Cupola finds its server as on the Mac;
        // never the owner's.
        t3.home = Some(fake.home.0.path().to_owned());
        let t3 = Arc::new(adjust(t3));
        assert_eq!(t3.status()["state"], "disconnected");
        assert_eq!(t3.connect_using(true).await.unwrap()["state"], "waiting");
        fake.answer(&urls.recv().await.unwrap(), true).await;
        wait_for(&t3, "connected").await;
        (t3, store)
    }

    async fn wait_for(t3: &T3Code, state: &str) {
        // A hang guard only (about 120 s), so a loaded machine never fails it.
        for _ in 0..12_000 {
            if t3.status()["state"] == state {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never {state}: {}", t3.status());
    }

    #[test]
    fn provider_without_listed_modes_accepts_every_mode() {
        let listed = json!({"providers": [
            {"providerInstanceId": "claudeAgent", "driverKind": "claudeAgent", "models": [{"id": "m"}], "constraints": []}]});
        let modes = picker_catalog(&listed)["providers"][0]["runtimeModes"].clone();
        assert_eq!(
            modes,
            json!([
                "approval-required",
                "auto-accept-edits",
                "auto",
                "full-access"
            ])
        );
    }

    // Contract: Connect T3 Code signs Cupola in with T3 Code's own OAuth
    // (registration, PKCE, the owner's approval in the browser), keeps the
    // grant in the store, and uses it until T3 Code refuses it; then the
    // phone is told to connect again. Declining or disconnecting leaves no
    // usable grant.
    // Contract: a run state T3 Code can't give (failing server, no such
    // thread) is not "running"; only an active run is.
    #[tokio::test]
    async fn an_unreadable_run_state_is_not_running() {
        let fake = Fake::start().await;
        let (t3, _store) = connected(&fake).await;
        assert!(!t3.run_in_progress("missing-thread").await);
        fake.failing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!t3.run_in_progress("t3-thread").await);
    }

    #[tokio::test]
    async fn connecting_keeps_t3_codes_grant_until_t3_code_refuses_it() {
        let fake = Fake::start().await;
        let (t3, store) = connected(&fake).await;
        let saved: Grant = serde_json::from_str(&store.read().unwrap().unwrap()).unwrap();
        assert_eq!(saved.origin, fake.origin);
        assert!(fake.tokens.lock().unwrap().contains(&saved.access_token));
        assert!(saved.expires_at_ms > now_ms() + 29 * 24 * 3600 * 1000);
        assert!(t3.status()["expiresAt"].as_str().is_some());
        // A Mac restart reads it back from the store.
        let reopened = T3Code::new(
            store.clone(),
            Some(fake.origin.clone()),
            immediate_opener(|_| Ok(())),
        );
        assert!(reopened.connected());
        assert!(reopened
            .call("orchestrator_capabilities", json!({}))
            .await
            .is_ok());

        // T3 Code no longer accepts it (removed there, or expired early).
        fake.revoked
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            t3.call("orchestrator_capabilities", json!({})).await,
            Err(CallError::Unauthorized)
        );
        let status = t3.status();
        assert_eq!(status["state"], "expired");
        assert_eq!(status["revoked"], true, "refused before it ended");
        assert!(status["error"].as_str().unwrap().contains("Connect again"));
        crate::tests::validate_http_contract("t3CodeStatus", &status);
        assert_eq!(
            t3.call("t3_thread_send", json!({})).await,
            Err(CallError::NotConnected),
            "an expired grant is not sent again"
        );

        // The owner declines the next approval: still not connected.
        let (opened, mut urls) = tokio::sync::mpsc::unbounded_channel::<String>();
        let declined = Arc::new(T3Code::new(
            Arc::new(MemoryGrantStore::default()),
            Some(fake.origin.clone()),
            immediate_opener(move |url: &str| {
                opened.send(url.to_owned()).unwrap();
                Ok(())
            }),
        ));
        declined.connect_using(true).await.unwrap();
        fake.answer(&urls.recv().await.unwrap(), false).await;
        wait_for(&declined, "disconnected").await;
        assert_eq!(
            declined.status()["error"],
            "Cupola wasn't approved in T3 Code."
        );

        let disconnected = t3.disconnect().await.unwrap();
        assert_eq!(disconnected["state"], "disconnected");
        crate::tests::validate_http_contract("t3CodeStatus", &disconnected);
        assert_eq!(store.read().unwrap(), None);
        fake.stop();
        let offline = T3Code::in_memory(
            Some("http://127.0.0.1:9".into()),
            immediate_opener(|_| Ok(())),
        );
        assert_eq!(
            Arc::new(offline).connect().await.unwrap()["pairingFailure"],
            "setup",
            "T3 Code not answering is a failure the phone shows"
        );
    }

    /// The grant the store holds.
    fn saved(store: &MemoryGrantStore) -> Grant {
        serde_json::from_str(&store.read().unwrap().unwrap()).unwrap()
    }

    pub(crate) async fn until(what: &str, done: impl Fn() -> bool) {
        // A hang guard only (about 120 s), so a loaded machine never fails it.
        for _ in 0..12_000 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never {what}");
    }

    // Contract (owner, Oct 21): Connect from the phone is one tap. With T3
    // Code's command on the Mac, Cupola approves itself with a one-time code
    // issued there, at full access (T3 Code refuses sends into a thread
    // with broader access than Cupola's), and opens no browser. A refusal
    // offers browser approval only after the owner's explicit choice.
    #[tokio::test]
    async fn connecting_is_one_tap_with_no_browser_and_no_code_to_copy() {
        let fake = Fake::start().await;
        let opened = Arc::new(Mutex::new(Vec::<String>::new()));
        let opener = |opened: &Arc<Mutex<Vec<String>>>| -> UrlOpener {
            let opened = opened.clone();
            immediate_opener(move |url: &str| {
                opened.lock().unwrap().push(url.to_owned());
                Ok(())
            })
        };
        let t3 = Arc::new(
            T3Code::new(
                Arc::new(MemoryGrantStore::default()),
                Some(fake.origin.clone()),
                opener(&opened),
            )
            .with_code_issuer(fake.issuer()),
        );
        assert_eq!(t3.connect().await.unwrap()["state"], "connected");
        assert!(opened.lock().unwrap().is_empty(), "no browser");
        assert_eq!(*fake.approved.lock().unwrap(), ["full-access"]);
        assert!(fake.pairing.lock().unwrap().is_empty(), "the code is used");
        assert!(t3
            .call("orchestrator_capabilities", json!({}))
            .await
            .is_ok());

        let unknown: CodeIssuer = Arc::new(|| Box::pin(async { Ok("not-issued".to_owned()) }));
        let fallback = Arc::new(
            T3Code::new(
                Arc::new(MemoryGrantStore::default()),
                Some(fake.origin.clone()),
                opener(&opened),
            )
            .with_code_issuer(unknown),
        );
        assert_eq!(
            fallback.connect().await.unwrap()["pairingFailure"],
            "refused_post"
        );
        assert!(opened.lock().unwrap().is_empty());
        assert_eq!(
            fallback.connect_using(true).await.unwrap()["state"],
            "waiting"
        );
        let page = opened
            .lock()
            .unwrap()
            .pop()
            .expect("the approval page opened");
        fake.answer(&page, true).await;
        wait_for(&fallback, "connected").await;
    }

    // Reproduced before this fix: all eight original classes opened one browser,
    // returned waiting/error=null and logged nothing (reproduction.log).
    #[tokio::test]
    async fn pairing_failures_explain_why_without_opening_a_browser() {
        use std::sync::atomic::Ordering::SeqCst;
        for (class, expected, attempts) in [
            ("no_home", PairingError::NoHome, 1),
            ("no_binary", PairingError::NoBinary, 1),
            ("timeout", PairingError::Timeout, 2),
            ("cli_error", PairingError::CliError, 1),
            ("transport", PairingError::Transport, 2),
            ("missing_decision", PairingError::MissingDecision, 1),
            ("refused_post", PairingError::RefusedPost, 1),
            ("server_busy", PairingError::ServerBusy, 2),
            ("rate_limit", PairingError::ServerBusy, 2),
            ("invalid_response", PairingError::InvalidResponse, 1),
            ("missing_redirect", PairingError::InvalidResponse, 1),
            ("redirect", PairingError::Redirect, 1),
            ("http_redirect", PairingError::Redirect, 1),
            ("state_mismatch", PairingError::StateMismatch, 1),
            ("finish", PairingError::Finish, 1),
        ] {
            let fake = Fake::start().await;
            let opened = Arc::new(Mutex::new(Vec::<String>::new()));
            let store = Arc::new(MemoryGrantStore::default());
            let mut t3 = T3Code::new(store.clone(), Some(fake.origin.clone()), {
                let opened = opened.clone();
                immediate_opener(move |url| {
                    opened.lock().unwrap().push(url.to_owned());
                    Ok(())
                })
            });
            t3.home = Some(fake.home.0.path().to_owned());
            let issues = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            t3.issue_code = match class {
                "no_home" => {
                    t3.home = None;
                    None
                }
                "no_binary" => cli_issuer(fake.home.0.path()),
                "timeout" | "cli_error" | "transport" => {
                    let issues = issues.clone();
                    Some(Arc::new(move || {
                        issues.fetch_add(1, SeqCst);
                        Box::pin(async move { Err(expected) })
                    }))
                }
                _ => Some(fake.issuer()),
            };
            if class == "missing_decision" {
                fake.missing_decision.store(true, SeqCst);
            }
            let reply = match class {
                "refused_post" => {
                    Some((StatusCode::FORBIDDEN, json!({"message": "secret-refusal"})))
                }
                "server_busy" => Some((
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({"secret": "server-token"}),
                )),
                "rate_limit" => Some((
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({"secret": "server-token"}),
                )),
                "http_redirect" => Some((StatusCode::FOUND, json!({}))),
                "invalid_response" => Some((StatusCode::OK, json!("not_json"))),
                "missing_redirect" => Some((StatusCode::OK, json!({"secret": "server-token"}))),
                "redirect" => Some((
                    StatusCode::OK,
                    json!({"redirectTo": "http://elsewhere/t3-code/callback?state=secret"}),
                )),
                "state_mismatch" => Some((StatusCode::OK, json!("wrong_state"))),
                "finish" => Some((StatusCode::OK, json!("missing_code"))),
                _ => None,
            };
            if let Some(reply) = reply {
                for _ in 0..attempts {
                    fake.decision_reply.lock().unwrap().push_back(reply.clone());
                }
            }
            let logs = tempfile::tempdir().unwrap();
            t3.log_to(&Arc::new(
                crate::logging::JsonlLogger::new(logs.path(), "pairing.jsonl").unwrap(),
            ));
            let t3 = Arc::new(t3);
            let status = t3.connect().await.unwrap();
            assert_eq!(status["state"], "disconnected", "{class}");
            assert_eq!(status["pairingFailure"], expected.class(), "{class}");
            assert_eq!(status["error"], expected.reason(), "{class}");
            assert!(opened.lock().unwrap().is_empty(), "{class}");
            assert!(store.read().unwrap().is_none(), "{class}");
            let log = std::fs::read_to_string(logs.path().join("pairing.jsonl")).unwrap();
            let entries: Vec<Value> = log
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(entries.len(), attempts, "{class}: {log}");
            assert!(log.contains(expected.class()), "{class}");
            for secret in [
                "secret-refusal",
                "server-token",
                "redirectTo",
                "access_token",
                "code_verifier",
                "secret-invalid-response",
                &fake.origin,
            ] {
                assert!(!log.contains(secret), "{class}: leaked {secret}");
            }
            if matches!(class, "timeout" | "cli_error" | "transport") {
                assert_eq!(issues.load(SeqCst), attempts);
            }
            if reply_class_has_post(class) {
                assert_eq!(fake.decision_calls.load(SeqCst), attempts);
            }
            crate::tests::validate_http_contract("t3CodeStatus", &status);
            t3.disconnect().await.unwrap();
            assert!(t3.status()["pairingFailure"].is_null());
        }
    }

    fn reply_class_has_post(class: &str) -> bool {
        !matches!(
            class,
            "no_home" | "no_binary" | "timeout" | "cli_error" | "transport" | "missing_decision"
        )
    }

    #[tokio::test]
    async fn transient_pairing_failure_retries_once_and_can_connect() {
        let fake = Fake::start().await;
        fake.decision_reply
            .lock()
            .unwrap()
            .push_back((StatusCode::SERVICE_UNAVAILABLE, json!({})));
        let t3 = one_tap(&fake, &Arc::new(MemoryGrantStore::default()));
        let status = t3.connect().await.unwrap();
        assert_eq!(status["state"], "connected");
        assert!(status["pairingFailure"].is_null());
        assert!(status["error"].is_null());
        assert_eq!(
            fake.decision_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[tokio::test]
    async fn unreadable_decision_reply_explains_the_response_failure() {
        let fake = Fake::start().await;
        fake.decision_reply
            .lock()
            .unwrap()
            .push_back((StatusCode::OK, json!("not_json")));
        let t3 = one_tap(&fake, &Arc::new(MemoryGrantStore::default()));
        let status = t3.connect().await.unwrap();
        assert_eq!(status["pairingFailure"], "invalid_response");
        assert_eq!(
            status["error"],
            "T3 Code returned an unreadable approval reply. Update or reopen T3 Code, then try again."
        );
    }

    #[tokio::test]
    async fn refused_connection_is_a_transport_failure() {
        let fake = Fake::start().await;
        let (authorized, _) = connected(&fake).await;
        authorized.check_reachability().await;
        assert_eq!(authorized.status()["reachable"], Value::Null);

        let t3 = one_tap(&fake, &Arc::new(MemoryGrantStore::default()));
        let (pending, _listener) = t3.sign_in(0).await.unwrap();
        fake.stop();
        authorized.check_reachability().await;
        assert_eq!(
            authorized.status()["state"],
            "connected",
            "authorization remains saved"
        );
        assert_eq!(
            authorized.status()["reachable"],
            false,
            "a saved grant is not reachability"
        );
        crate::tests::validate_http_contract("t3CodeStatus", &authorized.status());
        assert_eq!(
            t3.approve_here(&fake.issuer(), &pending).await,
            Err(PairingError::Transport)
        );
    }

    #[tokio::test]
    async fn disconnect_during_command_failure_discards_the_late_reason() {
        let fake = Fake::start().await;
        let (release, held) = tokio::sync::watch::channel(false);
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let issuer: CodeIssuer = {
            let asked = asked.clone();
            Arc::new(move || {
                let (mut held, asked) = (held.clone(), asked.clone());
                Box::pin(async move {
                    asked.store(true, std::sync::atomic::Ordering::SeqCst);
                    let _ = held.wait_for(|released| *released).await;
                    Err(PairingError::Timeout)
                })
            })
        };
        let t3 = Arc::new(
            T3Code::new(
                Arc::new(MemoryGrantStore::default()),
                Some(fake.origin.clone()),
                immediate_opener(|_| panic!("no automatic browser")),
            )
            .with_code_issuer(issuer),
        );
        let task = {
            let t3 = t3.clone();
            tokio::spawn(async move { t3.connect().await })
        };
        until("pairing command started", || {
            asked.load(std::sync::atomic::Ordering::SeqCst)
        })
        .await;
        t3.disconnect().await.unwrap();
        release.send_replace(true);
        let status = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(status["state"], "disconnected");
        assert!(status["pairingFailure"].is_null());
        assert!(status["error"].is_null());
    }

    #[test]
    fn t3_home_and_command_are_found_on_a_windows_account() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| std::ffi::OsString::from(value))
            }
        };
        // Windows sets USERPROFILE, not HOME; T3CODE_HOME wins; HOME wins over USERPROFILE.
        let expected = |home: &str| Some(PathBuf::from(home).join(".t3"));
        assert_eq!(
            default_home(env(&[("USERPROFILE", "C:/Users/a")])),
            expected("C:/Users/a")
        );
        assert_eq!(
            default_home(env(&[("HOME", "/h"), ("USERPROFILE", "/u")])),
            expected("/h")
        );
        assert_eq!(
            default_home(env(&[("HOME", ""), ("USERPROFILE", "/u")])),
            expected("/u")
        );
        assert_eq!(
            default_home(env(&[("T3CODE_HOME", "/t3"), ("HOME", "/h")])),
            Some(PathBuf::from("/t3"))
        );
        assert_eq!(default_home(env(&[])), None);
        // The command is whichever of the candidate names exists, in order.
        let bin = tempfile::tempdir().unwrap();
        assert_eq!(first_file(bin.path(), &["t3.exe", "t3.cmd"]), None);
        std::fs::write(bin.path().join("t3.cmd"), "").unwrap();
        assert_eq!(
            first_file(bin.path(), &["t3.exe", "t3.cmd"]),
            Some(bin.path().join("t3.cmd"))
        );
        std::fs::write(bin.path().join("t3.exe"), "").unwrap();
        assert_eq!(
            first_file(bin.path(), &["t3.exe", "t3.cmd"]),
            Some(bin.path().join("t3.exe"))
        );
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("bin")).unwrap();
        std::fs::write(home.path().join("bin").join(CLI_NAMES[0]), "").unwrap();
        assert!(cli_path(home.path()).is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn installing_the_command_then_retrying_can_connect() {
        use std::os::unix::fs::PermissionsExt;
        let fake = Fake::start().await;
        let home = fake.home.0.path();
        let t3 = Arc::new(
            T3Code::new(
                Arc::new(MemoryGrantStore::default()),
                Some(fake.origin.clone()),
                immediate_opener(|_| panic!("no automatic browser")),
            )
            .with_code_issuer(resolving_cli_issuer(home.to_owned())),
        );
        assert_eq!(t3.connect().await.unwrap()["pairingFailure"], "no_binary");
        // The installed command now issues a code from the owned T3 fixture.
        let code = fake.issuer()().await.unwrap();
        std::fs::create_dir_all(home.join("bin")).unwrap();
        let cli = home.join("bin/t3");
        std::fs::write(
            &cli,
            format!("#!/bin/sh\necho '{{\"credential\":\"{code}\"}}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let status = t3.connect().await.unwrap();
        assert_eq!(status["state"], "connected");
        assert!(status["pairingFailure"].is_null());
        assert!(status["error"].is_null());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn installed_command_contract_uses_owned_home_and_classifies_errors() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("bin")).unwrap();
        let cli = home.path().join("bin/t3");
        let arguments = home.path().join("arguments");
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        let succeeds = format!(
            "#!/bin/sh\n[ \"$T3CODE_HOME\" = {} ] || exit 3\nprintf '%s\\n' \"$@\" > {}\necho '{{\"credential\":\"owned-test-code\"}}'\n",
            quote(home.path()), quote(&arguments)
        );
        for (script, expected) in [
            (
                "#!/bin/sh\necho secret-output >&2\nexit 1\n",
                Err(PairingError::CliError),
            ),
            ("#!/bin/sh\nexec sleep 30\n", Err(PairingError::Timeout)),
            (succeeds.as_str(), Ok("owned-test-code".into())),
        ] {
            std::fs::write(&cli, script).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(
                cli_issuer_with_timeout(home.path(), Duration::from_secs(10)).unwrap()().await,
                expected
            );
        }
        let arguments = std::fs::read_to_string(arguments).unwrap();
        assert_eq!(
            arguments.lines().collect::<Vec<_>>(),
            vec![
                "auth",
                "pairing",
                "create",
                "--ttl",
                "5m",
                "--label",
                "Cupola",
                "--json",
                "--scope",
                "orchestration:read",
                "--scope",
                "orchestration:operate",
                "--scope",
                "settings:write",
                "--scope",
                "source-control:write",
            ]
        );
    }

    /// A Cupola that approves itself with T3 Code's command and revokes
    /// sessions with it, as the installed daemon does.
    pub(crate) fn one_tap(fake: &Arc<Fake>, store: &Arc<MemoryGrantStore>) -> Arc<T3Code> {
        Arc::new(
            T3Code::new(
                store.clone(),
                Some(fake.origin.clone()),
                immediate_opener(|_| Err("no browser".into())),
            )
            .with_code_issuer(fake.issuer())
            .with_session_revoker(fake.revoker()),
        )
    }

    /// The sign-in ended and T3 Code refuses it now; `one_tap`'s Cupola
    /// renews it in the background.
    pub(crate) fn end_sign_in(fake: &Fake, t3: &T3Code, store: &MemoryGrantStore) {
        let grant = ending(t3, store, now_ms() - 1000);
        fake.tokens
            .lock()
            .unwrap()
            .retain(|token| *token != grant.access_token);
        t3.inner.lock().unwrap().expired = true;
    }

    /// The grant saved in `store`, with its end moved to `expires_at_ms`.
    fn ending(t3: &T3Code, store: &MemoryGrantStore, expires_at_ms: i64) -> Grant {
        let grant = Grant {
            expires_at_ms,
            ..saved(store)
        };
        store
            .write(&serde_json::to_string(&grant).unwrap())
            .unwrap();
        t3.inner.lock().unwrap().grant = Some(Some(grant.clone()));
        grant
    }

    // Contract (owner, Oct 21): the sign-in renews itself. A grant near its
    // end that T3 Code still takes, or one T3 Code refuses after it ended,
    // is replaced in the background the same way, once at a time, and calls
    // work again; until then the phone hears it is renewing. The session it
    // replaces is revoked, so T3 Code's Authorized clients list one Cupola.
    // Without T3 Code's command it stays expired and the phone is asked to
    // connect again.
    #[tokio::test]
    async fn the_sign_in_renews_itself_before_it_ends_and_once_it_ended() {
        let fake = Fake::start().await;
        let store = Arc::new(MemoryGrantStore::default());
        let t3 = one_tap(&fake, &store);
        t3.connect().await.unwrap();
        let first = saved(&store);
        assert_eq!(first.session_id, token_session(&first.access_token));
        assert!(first.session_id.is_some(), "the session is kept");
        assert_eq!(fake.listed(), 1);
        t3.renew_if_due();
        assert!(
            t3.inner.lock().unwrap().renewing.is_none(),
            "a fresh grant waits"
        );

        // Two days left: renewed once, however often asked.
        ending(&t3, &store, now_ms() + 2 * 24 * 3600 * 1000);
        t3.renew_if_due();
        t3.renew_if_due();
        until("renewed", || {
            saved(&store).access_token != first.access_token
        })
        .await;
        until("settled", || t3.inner.lock().unwrap().renewing.is_none()).await;
        assert_eq!(fake.approved.lock().unwrap().len(), 2);
        assert!(saved(&store).expires_at_ms > now_ms() + 29 * 24 * 3600 * 1000);
        assert_eq!(fake.listed(), 1, "the replaced session is revoked");
        assert!(fake
            .ended
            .lock()
            .unwrap()
            .contains(first.session_id.as_ref().unwrap()));
        assert!(t3
            .call("orchestrator_capabilities", json!({}))
            .await
            .is_ok());

        // The grant ended and T3 Code refuses it: renewed, calls work again.
        let second = ending(&t3, &store, now_ms() - 1000);
        fake.tokens
            .lock()
            .unwrap()
            .retain(|token| *token != second.access_token);
        t3.inner.lock().unwrap().expired = true;
        t3.renew_if_due();
        assert_eq!(t3.unconnected(), Unconnected::Renewing);
        until("connected again", || t3.connected()).await;
        assert!(t3
            .call("orchestrator_capabilities", json!({}))
            .await
            .is_ok());
        assert_eq!(fake.approved.lock().unwrap().len(), 3);

        let (plain, _) = connected(&fake).await;
        fake.tokens.lock().unwrap().clear();
        assert_eq!(
            plain.call("orchestrator_capabilities", json!({})).await,
            Err(CallError::Unauthorized)
        );
        plain.renew_if_due();
        assert_eq!(plain.status()["state"], "expired");
    }

    #[tokio::test]
    async fn failed_reconnect_resumes_the_cancelled_grant_renewal() {
        use std::sync::atomic::Ordering::SeqCst;
        let fake = Fake::start().await;
        let store = Arc::new(MemoryGrantStore::default());
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let issuer: CodeIssuer = {
            let (asked, issue) = (asked.clone(), fake.issuer());
            Arc::new(move || {
                let attempt = asked.fetch_add(1, SeqCst);
                let code = issue();
                Box::pin(async move {
                    match attempt {
                        1 => std::future::pending().await, // Renewal cancelled by Connect.
                        2 => Err(PairingError::CliError),
                        _ => code.await,
                    }
                })
            })
        };
        let t3 = Arc::new(
            T3Code::new(
                store.clone(),
                Some(fake.origin.clone()),
                immediate_opener(|_| panic!("no automatic browser")),
            )
            .with_code_issuer(issuer)
            .with_session_revoker(fake.revoker()),
        );
        t3.connect().await.unwrap();
        let original = ending(&t3, &store, now_ms() + 2 * 24 * 3600 * 1000);
        t3.renew_if_due();
        until("renewal started", || asked.load(SeqCst) == 2).await;
        let status = t3.connect().await.unwrap();
        assert_eq!(status["state"], "connected");
        assert_eq!(status["pairingFailure"], "cli_error");
        assert!(
            t3.inner.lock().unwrap().renewing.is_some()
                || saved(&store).access_token != original.access_token,
            "failed Connect must resume renewal of the retained grant"
        );
        until("retained grant renewed after failed Connect", || {
            saved(&store).access_token != original.access_token
        })
        .await;
        until("renewal settled", || {
            t3.inner.lock().unwrap().renewing.is_none()
        })
        .await;
        assert_eq!(asked.load(SeqCst), 4);
        assert_eq!(fake.listed(), 1, "superseded grant revoked");
        assert!(t3.connected());
        assert!(t3.status()["pairingFailure"].is_null());
        assert!(t3.status()["error"].is_null());
    }

    // Contract (owner, Oct 22: "ask on phone"): T3 Code refusing a grant
    // before it ends means the owner removed Cupola there (measured on the
    // owner's Mac, Oct 21: Cupola signed itself in again 14 s after a
    // revoke). The revoke sticks: no renewal, also after a restart and once
    // the grant nears its end, and the phone is told so (`revoked`) until
    // the owner taps Connect.
    #[tokio::test]
    async fn a_revoke_in_t3_code_sticks_until_the_owner_connects_again() {
        let fake = Fake::start().await;
        let store = Arc::new(MemoryGrantStore::default());
        let t3 = one_tap(&fake, &store);
        t3.connect().await.unwrap();
        assert_eq!(t3.status()["revoked"], false);
        fake.revoke(&saved(&store).access_token);
        assert_eq!(
            t3.call("orchestrator_capabilities", json!({})).await,
            Err(CallError::Unauthorized)
        );
        let status = t3.status();
        assert_eq!(status["state"], "expired", "{status}");
        assert_eq!(status["revoked"], true, "{status}");
        assert!(
            status["error"].as_str().unwrap().contains("removed Cupola"),
            "{status}"
        );
        assert_eq!(t3.unconnected(), Unconnected::Revoked);
        assert_eq!(
            Delivery::Connect(t3.unconnected()).reason(),
            Some("revoked")
        );
        // The phone reads it as the protocol and its fixture say.
        crate::tests::validate_http_contract("t3CodeStatus", &status);
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../packages/protocol/fixtures/t3-code-status-v1.json"
        ))
        .unwrap();
        crate::tests::validate_http_contract("t3CodeStatus", &fixture);
        let keys = |value: &Value| {
            value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&fixture), keys(&status));
        assert_eq!(fixture["revoked"], status["revoked"]);
        for _ in 0..3 {
            t3.renew_if_due();
        }
        assert!(t3.inner.lock().unwrap().renewing.is_none(), "no renewal");
        assert_eq!(fake.approved.lock().unwrap().len(), 1, "never re-approved");
        assert_eq!(fake.listed(), 0);

        // A restart reads the revoke back and still doesn't renew, also
        // once the grant nears its end.
        let reopened = one_tap(&fake, &store);
        assert!(!reopened.connected());
        assert_eq!(reopened.status()["revoked"], true);
        ending(&reopened, &store, now_ms() + 3600 * 1000);
        reopened.renew_if_due();
        assert!(reopened.inner.lock().unwrap().renewing.is_none());
        assert_eq!(fake.approved.lock().unwrap().len(), 1);

        // The owner's Connect signs in again.
        assert_eq!(reopened.connect().await.unwrap()["state"], "connected");
        assert_eq!(reopened.status()["revoked"], false);
        assert!(!saved(&store).revoked);
        assert_eq!(fake.listed(), 1);
    }

    // Contract (lead, Oct 22): an HTTP 403 is a revoke only when it says
    // so. Any other 403 (T3 Code itself answers a removed grant with 401)
    // ends the grant and Cupola signs in again by itself, as before.
    #[tokio::test]
    async fn only_a_403_that_says_revoked_stops_renewing() {
        let fake = Fake::start().await;
        let store = Arc::new(MemoryGrantStore::default());
        let t3 = one_tap(&fake, &store);
        t3.connect().await.unwrap();
        *fake.forbidden.lock().unwrap() =
            Some(json!({"error": "forbidden", "message": "Not allowed from here."}));
        assert_eq!(
            t3.call("orchestrator_capabilities", json!({})).await,
            Err(CallError::Unauthorized)
        );
        let status = t3.status();
        assert_eq!(status["state"], "expired", "{status}");
        assert_eq!(status["revoked"], false, "{status}");
        assert!(!saved(&store).revoked);
        t3.renew_if_due();
        until("renewed", || t3.inner.lock().unwrap().renewing.is_none()).await;
        assert_eq!(fake.approved.lock().unwrap().len(), 2, "signed in again");
        *fake.forbidden.lock().unwrap() = None;
        assert_eq!(t3.status()["state"], "connected");
        assert_eq!(fake.listed(), 1);

        *fake.forbidden.lock().unwrap() =
            Some(json!({"error": "session_revoked", "message": "Session token revoked."}));
        assert_eq!(
            t3.call("orchestrator_capabilities", json!({})).await,
            Err(CallError::Unauthorized)
        );
        assert_eq!(t3.status()["revoked"], true);
        assert!(saved(&store).revoked, "kept for a restart");
        t3.renew_if_due();
        assert!(t3.inner.lock().unwrap().renewing.is_none(), "no renewal");
        assert_eq!(fake.approved.lock().unwrap().len(), 2);
    }

    // Contract (owner, Oct 22): a revoke Cupola hasn't seen yet (no call
    // since) is found before the renewal near the grant's end, which then
    // doesn't sign Cupola in again.
    #[tokio::test]
    async fn a_renewal_first_checks_t3_code_still_takes_the_grant() {
        let fake = Fake::start().await;
        let store = Arc::new(MemoryGrantStore::default());
        let t3 = one_tap(&fake, &store);
        t3.connect().await.unwrap();
        fake.revoke(&saved(&store).access_token);
        ending(&t3, &store, now_ms() + 2 * 24 * 3600 * 1000);
        t3.renew_if_due();
        until("settled", || t3.inner.lock().unwrap().renewing.is_none()).await;
        assert_eq!(fake.approved.lock().unwrap().len(), 1, "not renewed");
        assert_eq!(t3.status()["revoked"], true);
        assert!(saved(&store).revoked, "kept for a restart");
        assert_eq!(fake.listed(), 0);
    }

    // Contract (owner, Oct 22): Disconnect removes Cupola from T3 Code's
    // Authorized clients too; when T3 Code's command can't, Cupola is still
    // disconnected and the phone says where to remove it.
    #[tokio::test]
    async fn disconnecting_revokes_the_session_in_t3_code() {
        let fake = Fake::start().await;
        let store = Arc::new(MemoryGrantStore::default());
        let t3 = one_tap(&fake, &store);
        t3.connect().await.unwrap();
        assert_eq!(fake.listed(), 1);
        let status = t3.disconnect().await.unwrap();
        assert_eq!(status["state"], "disconnected");
        assert_eq!(status["error"], Value::Null, "{status}");
        assert_eq!(fake.listed(), 0, "no Cupola row left in T3 Code");

        let failing: SessionRevoker =
            Arc::new(|_| Box::pin(async { Err("command failed".to_owned()) }));
        let t3 = Arc::new(
            T3Code::new(
                store.clone(),
                Some(fake.origin.clone()),
                immediate_opener(|_| Err("no browser".into())),
            )
            .with_code_issuer(fake.issuer())
            .with_session_revoker(failing),
        );
        t3.connect().await.unwrap();
        let status = t3.disconnect().await.unwrap();
        assert_eq!(status["state"], "disconnected");
        assert!(
            status["error"]
                .as_str()
                .unwrap()
                .contains("Remove Cupola in T3 Code"),
            "{status}"
        );
        assert_eq!(store.read().unwrap(), None);
    }

    // Current T3 contract: configure without modelSelection reads. Even
    // an unchanged choice must reach the queue without a configuration write.
    #[tokio::test]
    async fn a_chosen_model_send_reads_configuration_before_queueing_once() {
        let fake = Fake::start().await;
        fake.thread("t3-claude", "project", "claudeAgent", None, &["hi"]);
        fake.configured.lock().unwrap().insert(
            "t3-claude".into(),
            json!({"instanceId": "claudeAgent", "model": "claude-opus-5-5",
                "options": [{"id": "effort", "value": "medium"},
                    {"id": "fastMode", "value": false}]}),
        );
        fake.queue_sends
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (t3, _) = connected(&fake).await;
        fake.calls.lock().unwrap().clear();
        let chosen = Chosen {
            family: Some(AgentFamily::Claude),
            models: vec!["claude-opus-5-5".into()],
            effort: Some("medium".into()),
            ..Chosen::default()
        };
        let send = || {
            t3.send(
                "native",
                "t3-claude",
                "go",
                "durable-message",
                SendMode::Queue,
                &[],
                Some(&chosen),
            )
        };
        let result = send()
            .await
            .expect("chosen model/effort must reach T3's queue");
        assert_eq!(result["delivery"], "queued");
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![
                (
                    "t3_thread_configure".into(),
                    json!({"threadId": "t3-claude"})
                ),
                (
                    "t3_thread_send".into(),
                    json!({"threadId": "t3-claude", "message": "go",
                    "mode": "queue", "clientRequestId": "durable-message"})
                ),
            ],
        );
        assert!(fake.configures.lock().unwrap().is_empty(), "no write");
        assert_eq!(
            send().await.unwrap(),
            result,
            "same durable receipt on retry"
        );
        assert_eq!(
            fake.threads.lock().unwrap()[0].queued.len(),
            1,
            "no duplicate"
        );
    }

    // Contract (owner, Oct 22): before a send with the phone's model or
    // effort, the T3 thread is switched only where T3 Code's configuration
    // differs, with T3 Code's own option names, and read back. Send now
    // with a switch goes after the reply running now. A model T3 Code
    // doesn't offer is refused before anything changes or is sent.
    #[tokio::test]
    async fn a_send_switches_the_t3_thread_to_the_chosen_model_first() {
        let fake = Fake::start().await;
        fake.thread("t3-codex", "project", "codex", None, &["hi"]);
        let (t3, _) = connected(&fake).await;
        fake.calls.lock().unwrap().clear();
        let send = |id: &'static str, mode, chosen: Chosen| {
            let t3 = t3.clone();
            async move {
                t3.send("native", "t3-codex", "go", id, mode, &[], Some(&chosen))
                    .await
            }
        };
        let codex = |models: &[&str], effort: Option<&str>, tier: Option<&str>| Chosen {
            family: Some(AgentFamily::Codex),
            models: models.iter().map(|m| (*m).to_owned()).collect(),
            effort: effort.map(str::to_owned),
            service_tier: tier.map(str::to_owned),
            own_model: false,
        };
        fake.queue_sends
            .store(true, std::sync::atomic::Ordering::SeqCst);
        send(
            "m1",
            SendMode::Steer,
            codex(
                &["codex:gpt-6.1-mini", "gpt-6.1-mini"],
                Some("high"),
                Some("priority"),
            ),
        )
        .await
        .unwrap();
        let configures = fake.configures.lock().unwrap().clone();
        assert_eq!(configures.len(), 1);
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(
            calls
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            [
                "t3_thread_configure",
                "orchestrator_capabilities",
                "t3_thread_configure",
                "t3_thread_configure",
                "t3_thread_send"
            ],
            "read, compare with offered models, write, verify, then send",
        );
        assert_eq!(calls[0].1, json!({"threadId": "t3-codex"}));
        assert_eq!(calls[3].1, json!({"threadId": "t3-codex"}));
        assert_eq!(calls[4].1["clientRequestId"], "m1");
        let selection = &configures[0]["modelSelection"];
        assert_eq!(selection["instanceId"], "codex");
        assert_eq!(selection["model"], "gpt-6.1-mini", "T3 Code's id for it");
        assert_eq!(
            option_value(selection, "reasoningEffort"),
            Some(&json!("high"))
        );
        assert_eq!(
            option_value(selection, "serviceTier"),
            Some(&json!("priority"))
        );
        assert_eq!(
            fake.sends.lock().unwrap()[0]["mode"],
            "queue",
            "after the running reply"
        );

        // Already so (T3 Code answers its options by id): nothing switches,
        // and Send now steers.
        send(
            "m2",
            SendMode::Steer,
            codex(&["gpt-6.1-mini"], Some("high"), None),
        )
        .await
        .unwrap();
        assert_eq!(fake.configures.lock().unwrap().len(), 1);
        assert_eq!(fake.sends.lock().unwrap()[1]["mode"], "steer");

        let refused = send(
            "m3",
            SendMode::Queue,
            codex(&["claude-opus-5-5"], None, None),
        )
        .await;
        assert!(
            matches!(&refused, Err(CallError::Refused { code, .. }) if code == "model_unavailable"),
            "{refused:?}"
        );
        assert_eq!(fake.configures.lock().unwrap().len(), 1);
        assert_eq!(fake.sends.lock().unwrap().len(), 2, "nothing sent");

        // The thread's own model, which T3 Code doesn't list: T3 Code's
        // model stays, and the message goes.
        let own = Chosen {
            own_model: true,
            ..codex(&["codex:retired"], None, None)
        };
        send("m4", SendMode::Queue, own).await.unwrap();
        assert_eq!(fake.configures.lock().unwrap().len(), 1);
        assert_eq!(fake.sends.lock().unwrap().len(), 3);

        // Claude's names: `effort`, and its speed as `fastMode`.
        let claude = Chosen {
            family: Some(AgentFamily::Claude),
            effort: Some("max".into()),
            service_tier: Some("fast".into()),
            ..Chosen::default()
        };
        assert_eq!(
            chosen_options(&claude),
            [("effort", json!("max")), ("fastMode", json!(true))]
        );
    }

    // Contract (review, Oct 21): Disconnect is final. A renewal or one-tap
    // sign-in still running (or a renewal about to start) when the owner
    // disconnects keeps no grant, so Cupola stays disconnected however late
    // T3 Code answers.
    #[tokio::test]
    async fn disconnecting_wins_over_a_sign_in_still_running() {
        let fake = Fake::start().await;
        // T3 Code's command, answering only once released.
        let (hold, held) = tokio::sync::watch::channel(false);
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let issue = fake.issuer();
        let slow: CodeIssuer = {
            let asked = asked.clone();
            Arc::new(move || {
                asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (mut held, code) = (held.clone(), issue());
                Box::pin(async move {
                    let _ = held.wait_for(|held| !*held).await;
                    code.await
                })
            })
        };
        let opened = Arc::new(Mutex::new(Vec::<String>::new()));
        let store = Arc::new(MemoryGrantStore::default());
        let t3 = Arc::new(
            T3Code::new(store.clone(), Some(fake.origin.clone()), {
                let opened = opened.clone();
                immediate_opener(move |url: &str| {
                    opened.lock().unwrap().push(url.to_owned());
                    Ok(())
                })
            })
            .with_code_issuer(slow),
        );
        t3.connect().await.unwrap();
        let near = Grant {
            expires_at_ms: now_ms() + 2 * 24 * 3600 * 1000,
            ..saved(&store)
        };
        store.write(&serde_json::to_string(&near).unwrap()).unwrap();
        t3.inner.lock().unwrap().grant = Some(Some(near));

        // A renewal waits on T3 Code; the owner disconnects meanwhile.
        hold.send_replace(true);
        t3.renew_if_due();
        until("renewal asked", || {
            asked.load(std::sync::atomic::Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(t3.disconnect().await.unwrap()["state"], "disconnected");
        hold.send_replace(false);
        // The renewal task holds the client until it ends (or is stopped).
        until("renewal ended", || Arc::strong_count(&t3) == 1).await;
        assert_eq!(store.read().unwrap(), None);
        assert_eq!(t3.status()["state"], "disconnected");

        // Connect waits on T3 Code; the owner disconnects meanwhile.
        hold.send_replace(true);
        let connecting = tokio::spawn({
            let t3 = t3.clone();
            async move { t3.connect().await }
        });
        until("connect asked", || {
            asked.load(std::sync::atomic::Ordering::SeqCst) == 3
        })
        .await;
        assert_eq!(t3.disconnect().await.unwrap()["state"], "disconnected");
        hold.send_replace(false);
        assert_eq!(connecting.await.unwrap().unwrap()["state"], "disconnected");
        assert_eq!(store.read().unwrap(), None);
        assert!(opened.lock().unwrap().is_empty(), "no approval page either");
        assert_eq!(t3.status()["state"], "disconnected");

        // The owner disconnects as a renewal is about to start.
        t3.connect().await.unwrap();
        let near = Grant {
            expires_at_ms: now_ms() + 2 * 24 * 3600 * 1000,
            ..saved(&store)
        };
        store.write(&serde_json::to_string(&near).unwrap()).unwrap();
        t3.inner.lock().unwrap().grant = Some(Some(near));
        // Disconnect's part under the lock (its revoke in T3 Code follows).
        *t3.before_renewal.lock().unwrap() = Some(Box::new(|t3| {
            t3.forget().unwrap();
            assert_eq!(t3.status()["state"], "disconnected");
        }));
        t3.renew_if_due();
        assert!(t3.inner.lock().unwrap().renewing.is_none(), "no renewal");
        until("renewal ended", || Arc::strong_count(&t3) == 1).await;
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert_eq!(store.read().unwrap(), None);
        assert_eq!(t3.status()["state"], "disconnected");
    }

    /// A Keychain that can't be read until unlocked.
    #[derive(Default)]
    struct Locked(std::sync::atomic::AtomicBool, MemoryGrantStore);

    impl GrantStore for Locked {
        fn read(&self) -> Result<Option<String>, String> {
            if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("locked".into());
            }
            self.1.read()
        }
        fn write(&self, grant: &str) -> Result<(), String> {
            self.1.write(grant)
        }
        fn remove(&self) -> Result<(), String> {
            self.1.remove()
        }
    }

    // Contract (owner, Oct 23): the model the phone chose reaches T3 Code by
    // the id T3 Code lists it under, for every provider's id shape.
    #[test]
    fn a_chosen_model_is_named_as_t3_code_lists_it() {
        let option = |id: &str, native: &[&str]| crate::ModelOption {
            agent_family: AgentFamily::Claude,
            capabilities: crate::ModelCapabilities::for_family(AgentFamily::Claude),
            id: id.into(),
            display_name: id.into(),
            description: None,
            model_specialty: None,
            hidden: false,
            reasoning_efforts: vec![],
            default_reasoning_effort: None,
            service_tiers: vec![],
            default_service_tier: None,
            is_default: false,
            provider_default: None,
            native_ids: native.iter().map(|id| (*id).to_owned()).collect(),
        };
        let catalog = crate::RuntimeCatalog {
            models: vec![option("claude:opus", &["claude-opus-5-5"])],
            ..Default::default()
        };
        let ids = |model: &str| t3_model_ids(&catalog, model);
        assert_eq!(ids("gpt-6.1-sol"), ["gpt-6.1-sol"]);
        assert_eq!(
            ids("claude:opus"),
            ["claude-opus-5-5", "claude:opus", "opus"]
        );
        assert_eq!(
            ids("opencode:opencode/deepseek-v4.1-flash-free"),
            [
                "opencode:opencode/deepseek-v4.1-flash-free",
                "opencode/deepseek-v4.1-flash-free"
            ]
        );
        assert_eq!(
            ids("cursor:claude-opus-5-5[context=300k,effort=medium]"),
            ["cursor:claude-opus-5-5", "claude-opus-5-5"]
        );
    }

    // Contract: a sign-in that can't be read (a locked Keychain) is not
    // taken as never connected for good: the phone hears why, and it is
    // read again a while later.
    #[test]
    fn a_sign_in_that_cant_be_read_is_read_again_later() {
        let store = Arc::new(Locked::default());
        let grant = Grant {
            origin: "http://127.0.0.1:9".into(),
            access_token: "token".into(),
            expires_at_ms: now_ms() + 24 * 3600 * 1000,
            session_id: None,
            revoked: false,
        };
        store
            .write(&serde_json::to_string(&grant).unwrap())
            .unwrap();
        store.0.store(true, std::sync::atomic::Ordering::SeqCst);
        let t3 = T3Code::new(
            store.clone(),
            Some(grant.origin.clone()),
            immediate_opener(|_| Ok(())),
        );
        assert!(!t3.connected());
        assert_eq!(t3.unconnected(), Unconnected::Unreadable);
        let status = t3.status();
        assert_eq!(status["state"], "disconnected");
        assert!(
            status["error"].as_str().unwrap().contains("Keychain"),
            "{status}"
        );
        store.0.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(!t3.connected(), "not read again at once");
        t3.inner.lock().unwrap().unreadable = Some(Instant::now() - UNREADABLE_RETRY);
        assert!(t3.connected());
        assert_eq!(t3.status()["error"], Value::Null);
    }

    // Contract: why a message has no T3 thread is known without waiting for
    // T3 Code: still looking, no single thread matched, or T3 Code didn't
    // answer the lookup. A thread once matched stays T3 Code's when Cupola
    // ran replies while T3 Code was closed (its latest message is then
    // behind the transcript's).
    #[test]
    fn a_lookup_says_why_there_is_no_t3_thread() {
        let t3 = T3Code::disabled();
        let (thread, start) = t3.known("session");
        assert_eq!(thread, Err(Unrouted::Finding));
        let epoch = start.unwrap();
        assert!(!t3.finish_lookup("session", epoch, &Err(CallError::Offline)));
        assert_eq!(t3.known("session").0, Err(Unrouted::Unreachable));
        assert!(!t3.finish_lookup("session", epoch, &Ok(None)));
        assert_eq!(t3.known("session").0, Err(Unrouted::Unmatched));
        assert!(t3.finish_lookup("session", epoch, &Ok(Some("t3-thread".into()))));
        assert_eq!(t3.known("session").0, Ok("t3-thread".to_owned()));
        assert!(!t3.finish_lookup("session", epoch, &Ok(None)));
        assert_eq!(t3.known("session").0, Ok("t3-thread".to_owned()), "kept");
        assert_eq!(t3.matched("session"), Some("t3-thread".to_owned()));
        assert!(t3.reason_changed("chat", Some("finding")));
        assert!(!t3.reason_changed("chat", Some("finding")), "logged once");
        assert!(t3.reason_changed("chat", None));
    }

    // Contract (lead, Oct 21): whether T3 Code owns a Claude session or Codex
    // thread is read from T3 Code's own store by its exact ID: the T3 thread
    // that continues it; none once that thread is archived there or
    // continues another session; unknown for a session T3 Code never had or
    // a store that isn't there. Reading writes nothing beside T3 Code's
    // store, whether T3 Code is closed or runs (and has newer rows only in
    // its write-ahead log).
    #[tokio::test]
    async fn t3_codes_own_store_says_which_thread_a_session_is() {
        let fake = Fake::default();
        let mut t3 = T3Code::disabled();
        assert_eq!(
            t3.kept("claude-1", AgentFamily::Claude).await,
            Kept::Unknown,
            "no T3 Code"
        );
        t3.home = Some(fake.home.0.path().to_owned());
        assert_eq!(
            t3.kept("claude-1", AgentFamily::Claude).await,
            Kept::Unknown,
            "no store"
        );
        fake.keeps("t3-a", "claudeAgent", "claude-1").await;
        fake.keeps("t3-b", "codex", "codex-1").await;
        let userdata = fake.home.0.path().join("userdata");
        let files = || {
            let mut names: Vec<String> = std::fs::read_dir(&userdata)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(files(), ["statev2.sqlite"], "T3 Code closed");
        assert_eq!(
            t3.kept("claude-1", AgentFamily::Claude).await,
            Kept::Thread("t3-a".into())
        );
        assert_eq!(
            t3.kept("codex-1", AgentFamily::Codex).await,
            Kept::Thread("t3-b".into())
        );
        assert_eq!(
            t3.kept("claude-2", AgentFamily::Claude).await,
            Kept::Unknown
        );
        // Only the family's own driver: a Codex thread with that ID isn't it.
        assert_eq!(t3.kept("codex-1", AgentFamily::Claude).await, Kept::Unknown);
        assert_eq!(t3.kept("claude-1", AgentFamily::Codex).await, Kept::Unknown);
        // The other way round: the session a launched T3 thread runs.
        let native = |thread: &'static str, family| {
            let t3 = &t3;
            async move { t3.native_for_thread(thread, family).await }
        };
        assert_eq!(
            native("t3-a", AgentFamily::Claude).await.as_deref(),
            Some("claude-1")
        );
        assert_eq!(
            native("t3-b", AgentFamily::Codex).await.as_deref(),
            Some("codex-1")
        );
        assert_eq!(native("t3-a", AgentFamily::Codex).await, None);
        assert_eq!(native("t3-c", AgentFamily::Claude).await, None);
        assert_eq!(files(), ["statev2.sqlite"], "nothing written");

        // T3 Code runs: what it wrote last is still in its log.
        let running = fake.store().await;
        fake.archive("t3-a").await;
        fake.keeps("t3-b", "codex", "codex-2").await;
        assert_eq!(
            native("t3-b", AgentFamily::Codex).await.as_deref(),
            Some("codex-2"),
            "the session it continues now"
        );
        let open = files();
        assert_eq!(
            open,
            ["statev2.sqlite", "statev2.sqlite-shm", "statev2.sqlite-wal"]
        );
        assert_eq!(
            t3.kept("claude-1", AgentFamily::Claude).await,
            Kept::Gone,
            "archived"
        );
        assert_eq!(
            t3.kept("codex-1", AgentFamily::Codex).await,
            Kept::Gone,
            "another session"
        );
        assert_eq!(
            t3.kept("codex-2", AgentFamily::Codex).await,
            Kept::Thread("t3-b".into())
        );
        assert_eq!(files(), open);
        running.close().await.unwrap();
    }

    #[test]
    fn the_pairing_code_is_read_from_t3_codes_answer() {
        let answer = br#"{
  "id": "pairing-1",
  "credential": "ABCDEFGHJKLM",
  "label": "Cupola",
  "scopes": ["orchestration:read"],
  "expiresAt": "2026-10-21T09:00:00.000Z"
}
"#;
        assert_eq!(pairing_credential(answer).as_deref(), Some("ABCDEFGHJKLM"));
        assert_eq!(pairing_credential(b"Issued client pairing token x."), None);
    }

    // Contract: a Claude session maps to the single T3 thread in its folder,
    // for its provider, whose first message opens the transcript and whose
    // latest is among its latest; a fork with the same messages makes it
    // ambiguous, and an ambiguous or missing match routes nothing.
    #[tokio::test]
    async fn a_session_maps_only_to_the_one_t3_thread_that_is_it() {
        let fake = Fake::start().await;
        fake.project("p", "/work/Held");
        fake.project("other", "/work/Other");
        let opening = "Build the parser for the log format";
        fake.thread(
            "forked-earlier",
            "p",
            "claudeAgent",
            None,
            &[opening, "Add tests"],
        );
        fake.thread(
            "it",
            "p",
            "claudeAgent",
            None,
            &[
                opening,
                "Add tests",
                "Fix the flaky one",
                "Run them",
                "Ship it",
            ],
        );
        fake.thread(
            "codex-twin",
            "p",
            "codex",
            None,
            &[
                opening,
                "Add tests",
                "Fix the flaky one",
                "Run them",
                "Ship it",
            ],
        );
        fake.thread(
            "elsewhere",
            "p",
            "claudeAgent",
            Some("/work/Held-wt"),
            &[opening, "Ship it"],
        );
        fake.thread(
            "other-folder",
            "other",
            "claudeAgent",
            None,
            &[opening, "Ship it"],
        );
        let (t3, _) = connected(&fake).await;
        // T3 Code wraps a task notification around its summary. The fork
        // ended ("Add tests") before the session's last three prompts.
        let prompts: Vec<String> = [
            opening,
            "Add tests",
            "Fix the flaky one",
            "Run them",
            "<task-notification><summary>Ship it</summary></task-notification>",
        ]
        .map(str::to_owned)
        .into();
        assert_eq!(
            t3.thread_for(AgentFamily::Claude, "/work/Held/", &prompts)
                .await,
            Ok(Some("it".into()))
        );

        // Both "it" and the fork end among the session's last three prompts.
        let both: Vec<String> = [opening, "Add tests", "Ship it", "Add tests"]
            .map(str::to_owned)
            .into();
        assert_eq!(
            t3.thread_for(AgentFamily::Claude, "/work/Held", &both)
                .await,
            Ok(None)
        );
        // Another session in the folder: nothing in T3 Code opens with it.
        let unrelated: Vec<String> = ["Fix the login page".to_owned()].into();
        assert_eq!(
            t3.thread_for(AgentFamily::Claude, "/work/Held", &unrelated)
                .await,
            Ok(None)
        );
        // The Codex thread of the same conversation is matched for Codex only.
        assert_eq!(
            t3.thread_for(AgentFamily::Codex, "/work/Held", &prompts)
                .await,
            Ok(Some("codex-twin".into()))
        );
        // A worktree thread is found by its own folder.
        let worktree: Vec<String> = [opening, "Ship it"].map(str::to_owned).into();
        assert_eq!(
            t3.thread_for(AgentFamily::Claude, "/work/Held-wt", &worktree)
                .await,
            Ok(Some("elsewhere".into()))
        );
    }

    // Contract: the owner's prompts are read from a Claude transcript (not
    // tool results, meta entries or side chains) and a Codex rollout (its
    // `user_message` events, not injected instructions), oldest first.
    #[test]
    fn transcript_prompts_are_the_owners_messages() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude.jsonl");
        std::fs::write(
            &claude,
            [
                json!({"type":"user","isMeta":true,"message":{"content":"<command-name>/model</command-name>"}}),
                json!({"type":"user","message":{"content":"First"}}),
                json!({"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}),
                json!({"type":"user","message":{"content":[{"type":"tool_result","content":"user data"}]}}),
                json!({"type":"user","isSidechain":true,"message":{"content":"agent prompt"}}),
                json!({"type":"user","message":{"content":[{"type":"text","text":"Second"},{"type":"image"}]}}),
            ]
            .map(|line| line.to_string())
            .join("\n"),
        )
        .unwrap();
        assert_eq!(
            transcript_prompts(&claude, AgentFamily::Claude),
            ["First", "Second"]
        );
        let codex = dir.path().join("rollout.jsonl");
        std::fs::write(
            &codex,
            [
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"}]}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":"Fix the build"}}),
            ]
            .map(|line| line.to_string())
            .join("\n"),
        )
        .unwrap();
        assert_eq!(
            transcript_prompts(&codex, AgentFamily::Codex),
            ["Fix the build"]
        );
    }

    // Contract (lead, fix round 3): T3 Code started a message Cupola sent
    // when its timeline has the user message with T3 Code's ID for it: the
    // one T3 Code answered, or, the answer lost, the ID it derives from the
    // `clientRequestId` (a text) or the files it issued for it. The same
    // words typed in T3 Code, or another send's ID, never are it.
    #[test]
    fn t3_code_started_a_message_by_its_id_never_its_words() {
        let entry = |id: &str, files: &[&str]| {
            json!({"type": "user_message", "messageId": id, "text": "Go on",
                "attachments": files.iter().map(|id| json!({"type": "file", "id": id})).collect::<Vec<_>>()})
        };
        let answered = Sent {
            message: "m".into(),
            t3_message: Some("message:mcp:client%3Ag:thread-send:m".into()),
            attachments: vec![],
        };
        let lost = Sent {
            message: "m".into(),
            t3_message: None,
            attachments: vec![],
        };
        let files = Sent {
            message: "f".into(),
            t3_message: None,
            attachments: vec!["file-1".into()],
        };
        for (sent, item, is) in [
            (
                &answered,
                entry("message:mcp:client%3Ag:thread-send:m", &[]),
                true,
            ),
            (
                &answered,
                entry("message:mcp:client%3Aother:thread-send:m", &[]),
                false,
            ),
            (&answered, entry("message:typed", &[]), false),
            (
                &lost,
                entry("message:mcp:client%3Ag:thread-send:m", &[]),
                true,
            ),
            (
                &lost,
                entry("message:mcp:client%3Ag:thread-send:mm", &[]),
                false,
            ),
            (&lost, entry("message:typed:thread-send:m", &[]), false),
            (&files, entry("message:any", &["file-1", "file-2"]), true),
            (
                &files,
                entry("message:mcp:client%3Ag:thread-send:f", &[]),
                false,
            ),
            (&files, entry("message:typed", &["file-2"]), false),
        ] {
            assert_eq!(sent.is(&item), is, "{} is {item}", sent.message);
        }
        let reply = json!({"type": "assistant_message", "messageId": "message:mcp:client%3Ag:thread-send:m"});
        assert!(!answered.is(&reply));
    }

    // Contract (lead, final design): an agent's record is a message Cupola
    // sent through T3 Code when its words are the message's, whitespace
    // aside, then T3 Code's note for each of the message's files, in order.
    // Each message, in the order sent, is paired with the first such record
    // after its boundary that no message before it was paired with.
    #[test]
    fn a_record_is_a_message_sent_through_t3_code_by_its_words_after_it() {
        let message = |client: &str, after: i64, files: &[(&str, &str)]| Delivered {
            sent: Sent {
                message: client.into(),
                t3_message: None,
                attachments: vec![],
            },
            client: client.into(),
            body: "Fix  the\nbuild".into(),
            files: files
                .iter()
                .map(|(kind, name)| ((*kind).into(), (*name).into()))
                .collect(),
            thread: "t3-thread".into(),
            after,
            started: true,
            failures: 0,
            retry_at: 0,
        };
        let note = |kind: &str, name: &str| {
            format!("[Attached {kind} \"{name}\" is saved at: /t3/{name}]")
        };
        let image = note("image", "a.png");
        let file = note("file", "b.txt");
        let text = message("text", 0, &[]);
        let files = message("files", 0, &[("image", "a.png"), ("file", "b.txt")]);
        for (sent, record, is) in [
            (&text, "Fix the build".to_owned(), true),
            (&text, " Fix the build \n".to_owned(), true),
            (&text, "Fix the build!".to_owned(), false),
            (&text, "fix the build".to_owned(), false),
            (&text, format!("Fix the build\n\n{file}"), false),
            (&files, format!("Fix the build\n\n{image}\n\n{file}"), true),
            (&files, format!("Fix the build\n\n{file}\n\n{image}"), false),
            (&files, format!("Fix the build\n\n{file}"), false),
            (&files, "Fix the build".to_owned(), false),
            (
                &files,
                format!("Fix the build\n\n{image}\n\n{file} too"),
                false,
            ),
        ] {
            assert_eq!(sent.words(&record, &[]), is, "{} {record:?}", sent.client);
        }
        // The notes were taken out of the saved record's text
        // (`tool_media`): the record says which, and only those stand in.
        let taken = |notes: &[(&str, &str)]| -> Vec<(String, String)> {
            notes
                .iter()
                .map(|(kind, name)| ((*kind).to_owned(), (*name).to_owned()))
                .collect()
        };
        let both = taken(&[("image", "a.png"), ("file", "b.txt")]);
        assert!(files.words("Fix the build", &both));
        assert!(!files.words("Fix the build!", &both));
        assert!(
            !files.words("Fix the build", &[]),
            "no file, no message with one"
        );
        assert!(!files.words("Fix the build", &both[1..]));
        assert!(!files.words(
            "Fix the build",
            &taken(&[("file", "b.txt"), ("image", "a.png")])
        ));
        let record = |row: i64, id: &str, text: &str| Record {
            row,
            turn: format!("turn-{id}"),
            item: id.into(),
            text: text.into(),
            taken: vec![],
        };
        let records = [
            record(1, "before", "Fix the build"),
            record(3, "first", "Fix the build"),
            record(4, "other", "Other words"),
            record(5, "second", "Fix the build"),
        ];
        let sent = [
            message("a", 2, &[]),
            message("b", 2, &[]),
            message("c", 2, &[]),
        ];
        let echo =
            |id: &str, client: &str| ((format!("turn-{id}"), id.to_owned()), client.to_owned());
        for split in 0..=records.len() {
            let saved = pair_incremental(&sent, &[], &records[..split]);
            assert_eq!(
                echo_map(&pair_incremental(&sent, &saved, &records[split..])),
                pair(&sent, &records)
            );
        }
        // Client IDs are device-scoped; persisted confirmation must identify
        // the actual send even if another paired device reused its client ID.
        let mut cross_device = [
            message("same-client", 2, &[]),
            message("same-client", 2, &[]),
        ];
        cross_device[0].sent.message = "device-a-message".into();
        cross_device[1].sent.message = "device-b-message".into();
        let saved = pair_incremental(&cross_device, &[], &records[..2]);
        assert_eq!(
            echo_map(&pair_incremental(&cross_device, &saved, &records[2..])),
            pair(&cross_device, &records)
        );
        let repeated = [
            record(3, "same", "Fix the build"),
            record(5, "same", "Fix the build"),
            record(6, "third", "Fix the build"),
        ];
        let saved = pair_incremental(&sent, &[], &repeated[..2]);
        assert_eq!(
            echo_map(&pair_incremental(&sent, &saved, &repeated[2..])),
            pair(&sent, &repeated)
        );
        assert_eq!(
            pair(&sent, &records),
            [echo("first", "a"), echo("second", "b")].into()
        );
        assert_eq!(
            pair(&[message("late", 3, &[]), message("a", 0, &[])], &records),
            [echo("second", "late"), echo("before", "a")].into()
        );
    }
}

/// Bounded T3 Code response reads (AUD-T3-BOUNDS, rust-robustness #1) and a
/// browser launcher that cannot stall the connection state (#5).
// Uses the unix-only `tests` fake T3 server.
#[cfg(all(test, unix))]
mod bounds_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    /// How a hostile T3 Code frames a body, none of which the client may trust.
    #[derive(Clone, Copy, Debug)]
    enum Framing {
        /// No Content-Length, chunked.
        Chunked,
        /// No Content-Length, ends when the server closes.
        UntilClose,
        /// A Content-Length far beyond what is sent first.
        DeclaredHuge,
        /// A true Content-Length (the body is as long as it says).
        Exact,
    }
    const FRAMINGS: [Framing; 4] = [
        Framing::Chunked,
        Framing::UntilClose,
        Framing::DeclaredHuge,
        Framing::Exact,
    ];
    /// Framings under which a body can complete.
    const COMPLETE: [Framing; 3] = [Framing::Chunked, Framing::UntilClose, Framing::Exact];

    /// One answer: `prefix`, filler, `suffix`, exactly `total` bytes.
    struct Reply {
        status: u16,
        headers: Vec<(&'static str, String)>,
        prefix: Vec<u8>,
        suffix: Vec<u8>,
        total: usize,
        framing: Framing,
    }

    impl Reply {
        fn new(status: u16, body: &str) -> Self {
            Self {
                status,
                headers: vec![("content-type", "application/json".into())],
                prefix: body.as_bytes().to_vec(),
                suffix: Vec::new(),
                total: body.len(),
                framing: Framing::Chunked,
            }
        }

        /// A JSON document of exactly `total` bytes: `{"pad":"aaa…"}`.
        fn padded(status: u16, total: usize, framing: Framing) -> Self {
            Self::wrapped(status, r#"{"pad":""#, r#""}"#, total, framing)
        }

        fn wrapped(
            status: u16,
            prefix: &str,
            suffix: &str,
            total: usize,
            framing: Framing,
        ) -> Self {
            Self {
                prefix: prefix.as_bytes().to_vec(),
                suffix: suffix.as_bytes().to_vec(),
                total,
                framing,
                ..Self::new(status, "")
            }
        }

        fn piece(&self, from: usize, to: usize) -> Vec<u8> {
            let mut piece = vec![b'a'; to - from];
            for (at, byte) in self.prefix.iter().enumerate() {
                if (from..to).contains(&at) {
                    piece[at - from] = *byte;
                }
            }
            let tail = self.total - self.suffix.len();
            for (at, byte) in self.suffix.iter().enumerate() {
                if (from..to).contains(&(tail + at)) {
                    piece[tail + at - from] = *byte;
                }
            }
            piece
        }
    }

    struct Request {
        path: String,
        body: String,
        host: String,
    }

    impl Request {
        fn host_origin(&self) -> String {
            format!("http://{}", self.host)
        }
    }

    /// A T3 Code stand-in that answers as `script` says and counts the body
    /// bytes it managed to hand to the client before the client hung up.
    struct Hostile {
        origin: String,
        written: Arc<AtomicUsize>,
        settled: Arc<AtomicUsize>,
    }

    impl Hostile {
        async fn start(script: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let (written, settled) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let script = Arc::new(script);
            let (counted, done) = (written.clone(), settled.clone());
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (script, counted, done) = (script.clone(), counted.clone(), done.clone());
                    tokio::spawn(async move {
                        let request = read_request(&mut stream).await;
                        let reply = script(&request);
                        serve(&mut stream, &reply, &counted).await;
                        done.fetch_add(1, SeqCst);
                    });
                }
            });
            Self {
                origin,
                written,
                settled,
            }
        }

        /// Waits until `count` answers have been written or abandoned.
        async fn settled(&self, count: usize) {
            super::tests::until("settled", || self.settled.load(SeqCst) >= count).await;
        }
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Request {
        let mut data = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(at) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                break at + 4;
            }
            let read = stream.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break data.len();
            }
            data.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8_lossy(&data[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while data.len() < head_end + length {
            let read = stream.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            data.extend_from_slice(&chunk[..read]);
        }
        Request {
            host: head
                .lines()
                .find_map(|line| line.strip_prefix("host:"))
                .unwrap_or_default()
                .trim()
                .to_owned(),
            path: head
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned(),
            body: String::from_utf8_lossy(&data[head_end.min(data.len())..]).into_owned(),
        }
    }

    async fn serve(stream: &mut tokio::net::TcpStream, reply: &Reply, written: &AtomicUsize) {
        let mut head = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", reply.status);
        for (name, value) in &reply.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        match reply.framing {
            Framing::Chunked => head.push_str("Transfer-Encoding: chunked\r\n"),
            Framing::UntilClose => {}
            Framing::DeclaredHuge => {
                head.push_str(&format!("Content-Length: {}\r\n", reply.total.max(1 << 30)))
            }
            Framing::Exact => head.push_str(&format!("Content-Length: {}\r\n", reply.total)),
        }
        head.push_str("\r\n");
        if stream.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let mut at = 0;
        while at < reply.total {
            let to = (at + (64 << 10)).min(reply.total);
            let piece = reply.piece(at, to);
            let sent = match reply.framing {
                Framing::Chunked => {
                    let mut framed = format!("{:x}\r\n", piece.len()).into_bytes();
                    framed.extend_from_slice(&piece);
                    framed.extend_from_slice(b"\r\n");
                    stream.write_all(&framed).await
                }
                _ => stream.write_all(&piece).await,
            };
            if sent.is_err() {
                return;
            }
            written.fetch_add(piece.len(), SeqCst);
            at = to;
        }
        if matches!(reply.framing, Framing::Chunked) {
            let _ = stream.write_all(b"0\r\n\r\n").await;
        }
        let _ = stream.shutdown().await;
    }

    /// Far more than the budget plus every socket buffer on the path, so a
    /// reader that stops at the budget leaves most of it unsent.
    const FLOOD: usize = 32 << 20;

    fn grant(origin: &str) -> Arc<MemoryGrantStore> {
        let store = Arc::new(MemoryGrantStore::default());
        store
            .write(
                &serde_json::to_string(&Grant {
                    origin: origin.to_owned(),
                    access_token: "token".into(),
                    expires_at_ms: now_ms() + 10 * 24 * 3600 * 1000,
                    session_id: None,
                    revoked: false,
                })
                .unwrap(),
            )
            .unwrap();
        store
    }

    fn client(origin: &str) -> Arc<T3Code> {
        Arc::new(T3Code::new(
            grant(origin),
            Some(origin.to_owned()),
            immediate_opener(|_| Ok(())),
        ))
    }

    fn rpc(request: &Request) -> String {
        serde_json::from_str::<Value>(&request.body)
            .ok()
            .and_then(|body| body["method"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// MCP answers `initialize` and the initialized notice normally and
    /// answers the tool call with `call`.
    fn mcp(call: impl Fn() -> Reply + Send + Sync + 'static) -> impl Fn(&Request) -> Reply {
        move |request| match rpc(request).as_str() {
            "initialize" => {
                let mut reply = Reply::new(200, r#"{"jsonrpc":"2.0","id":0,"result":{}}"#);
                reply.headers.push(("mcp-session-id", "session".into()));
                reply
            }
            "tools/call" => call(),
            _ => Reply::new(202, ""),
        }
    }

    const SSE_HEAD: &str =
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"structuredContent\":{\"ok\":true},\"pad\":\"";
    const SSE_TAIL: &str = "\"}}\n\n";

    fn json_call(total: usize, framing: Framing) -> Reply {
        Reply::wrapped(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"ok":true},"pad":""#,
            r#""}}"#,
            total,
            framing,
        )
    }

    fn sse_call(total: usize, framing: Framing) -> Reply {
        let mut reply = Reply::wrapped(200, SSE_HEAD, SSE_TAIL, total, framing);
        reply.headers = vec![("content-type", "text/event-stream".into())];
        reply
    }

    fn peak(t3: &T3Code) -> usize {
        t3.body_peak.load(SeqCst)
    }

    // rust-robustness #1: T3 Code's answers are read under one cumulative
    // byte bound, whatever their framing says. The flood is cut off by the
    // client hanging up, so most of it is never even sent, and the reader
    // never held more than the budget (`body_peak`).
    #[tokio::test]
    async fn oversized_t3_answers_are_refused_before_they_are_buffered() {
        for framing in FRAMINGS {
            // MCP 200, as JSON and as server-sent events.
            for sse in [false, true] {
                let hostile = Hostile::start(mcp(move || {
                    if sse {
                        sse_call(FLOOD, framing)
                    } else {
                        json_call(FLOOD, framing)
                    }
                }))
                .await;
                let t3 = client(&hostile.origin);
                let outcome = t3.call("orchestrator_capabilities", json!({})).await;
                assert!(
                    matches!(outcome, Err(CallError::Lost)),
                    "{framing:?} sse={sse}: {outcome:?}"
                );
                hostile.settled(3).await;
                assert!(
                    hostile.written.load(SeqCst) < FLOOD,
                    "{framing:?} sse={sse}: the whole flood was read"
                );
                assert!(
                    peak(&t3) <= MAX_BODY,
                    "{framing:?} sse={sse}: {}",
                    peak(&t3)
                );
            }
            // MCP 403: an oversized refusal is not taken as a revocation.
            let hostile =
                Hostile::start(move |_| Reply::wrapped(403, "revoked ", "", FLOOD, framing)).await;
            let t3 = client(&hostile.origin);
            assert!(matches!(
                t3.call("orchestrator_capabilities", json!({})).await,
                Err(CallError::Unauthorized)
            ));
            assert_eq!(t3.status()["revoked"], false, "{framing:?}");
            hostile.settled(1).await;
            assert!(hostile.written.load(SeqCst) < FLOOD, "{framing:?} 403");
            assert!(peak(&t3) <= MAX_BODY, "{framing:?} 403: {}", peak(&t3));
            // OAuth discovery.
            let hostile = Hostile::start(move |_| Reply::padded(200, FLOOD, framing)).await;
            let t3 = client(&hostile.origin);
            let url = format!(
                "{}/.well-known/oauth-protected-resource/mcp",
                hostile.origin
            );
            assert!(t3.get_json(&url).await.is_err(), "{framing:?} discovery");
            hostile.settled(1).await;
            assert!(
                hostile.written.load(SeqCst) < FLOOD,
                "{framing:?} discovery"
            );
            assert!(
                peak(&t3) <= MAX_BODY,
                "{framing:?} discovery: {}",
                peak(&t3)
            );
            // Client registration, reached through a real Connect.
            let hostile = Hostile::start(move |request| {
                let origin = request.host_origin();
                match request.path.as_str() {
                    "/.well-known/oauth-protected-resource/mcp" => Reply::new(
                        200,
                        &json!({"authorization_servers": [origin], "resource": format!("{origin}/mcp")}).to_string(),
                    ),
                    "/.well-known/oauth-authorization-server" => Reply::new(
                        200,
                        &json!({"authorization_endpoint": format!("{origin}/oauth/mcp/authorize"),
                            "token_endpoint": format!("{origin}/oauth/mcp/token"),
                            "registration_endpoint": format!("{origin}/oauth/mcp/register")})
                        .to_string(),
                    ),
                    _ => Reply::padded(201, FLOOD, framing),
                }
            })
            .await;
            let t3 = client(&hostile.origin);
            let error = t3.connect_using(true).await.unwrap_err();
            assert_eq!(error.0, StatusCode::BAD_GATEWAY, "{framing:?} registration");
            hostile.settled(3).await;
            assert!(
                hostile.written.load(SeqCst) < FLOOD,
                "{framing:?} registration"
            );
            assert!(
                peak(&t3) <= MAX_BODY,
                "{framing:?} registration: {}",
                peak(&t3)
            );
        }
    }

    // An answer of exactly the budget is read whole, one byte more is
    // refused, and the next normal request is answered.
    #[tokio::test]
    async fn answers_at_the_budget_are_read_and_the_next_request_succeeds() {
        for framing in COMPLETE {
            for sse in [false, true] {
                let calls = Arc::new(AtomicUsize::new(0));
                let hostile = Hostile::start(mcp({
                    let calls = calls.clone();
                    move || {
                        let total = match calls.fetch_add(1, SeqCst) {
                            0 => MAX_BODY,
                            1 => MAX_BODY + 1,
                            _ => 0,
                        };
                        match (total, sse) {
                            (0, false) => Reply::new(
                                200,
                                r#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"ok":true}}}"#,
                            ),
                            (0, true) => {
                                let mut reply = Reply::new(
                                    200,
                                    "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"structuredContent\":{\"ok\":true}}}\n\n",
                                );
                                reply.headers =
                                    vec![("content-type", "text/event-stream".into())];
                                reply
                            }
                            (total, true) => sse_call(total, framing),
                            (total, false) => json_call(total, framing),
                        }
                    }
                }))
                .await;
                let t3 = client(&hostile.origin);
                let at_limit = t3.call("orchestrator_capabilities", json!({})).await;
                assert_eq!(
                    at_limit.unwrap(),
                    json!({"ok": true}),
                    "{framing:?} sse={sse}"
                );
                assert_eq!(peak(&t3), MAX_BODY, "{framing:?} sse={sse}: read whole");
                let over = t3.call("orchestrator_capabilities", json!({})).await;
                assert!(
                    matches!(over, Err(CallError::Lost)),
                    "{framing:?} sse={sse}"
                );
                let next = t3.call("orchestrator_capabilities", json!({})).await;
                assert_eq!(next.unwrap(), json!({"ok": true}), "{framing:?} sse={sse}");
                assert_eq!(peak(&t3), MAX_BODY, "{framing:?} sse={sse}: never above");
            }
            // 403 whose revocation notice is inside the budget.
            let hostile =
                Hostile::start(move |_| Reply::wrapped(403, "revoked ", "", MAX_BODY, framing))
                    .await;
            let t3 = client(&hostile.origin);
            assert!(matches!(
                t3.call("orchestrator_capabilities", json!({})).await,
                Err(CallError::Unauthorized)
            ));
            assert_eq!(t3.status()["revoked"], true, "{framing:?}");
            assert_eq!(peak(&t3), MAX_BODY, "{framing:?} 403");
            // OAuth discovery: at the budget, over it, then normal.
            let served = Arc::new(AtomicUsize::new(0));
            let hostile = Hostile::start({
                let served = served.clone();
                move |_| match served.fetch_add(1, SeqCst) {
                    0 => Reply::padded(200, MAX_BODY, framing),
                    1 => Reply::padded(200, MAX_BODY + 1, framing),
                    _ => Reply::new(200, r#"{"ok":true}"#),
                }
            })
            .await;
            let t3 = client(&hostile.origin);
            let url = format!("{}/.well-known/oauth-authorization-server", hostile.origin);
            assert_eq!(
                t3.get_json(&url).await.unwrap()["pad"]
                    .as_str()
                    .unwrap()
                    .len(),
                MAX_BODY - 10
            );
            assert!(
                t3.get_json(&url).await.is_err(),
                "{framing:?} one byte over"
            );
            assert_eq!(t3.get_json(&url).await.unwrap(), json!({"ok": true}));
            assert_eq!(peak(&t3), MAX_BODY, "{framing:?} discovery");
        }
    }

    // The reader itself, with no network: it stops pulling once the next
    // chunk would pass the budget, holds at most the budget and reserves no
    // more, whatever the chunk sizes.
    #[tokio::test]
    async fn the_bounded_reader_never_holds_more_than_its_budget() {
        let polled = AtomicUsize::new(0);
        let flood = futures_util::stream::iter((0..100).map(|_| {
            polled.fetch_add(1, SeqCst);
            Ok::<_, ()>(vec![7u8; 1 << 20])
        }));
        let mut retained = 0;
        let outcome = collect_bounded(flood, 4 << 20, &mut retained).await;
        assert_eq!(outcome, Err(BodyError::TooLarge));
        assert_eq!(
            retained,
            4 << 20,
            "four whole chunks fit, the fifth is refused"
        );
        assert_eq!(polled.load(SeqCst), 5, "the other 95 are never pulled");

        let mut retained = 0;
        let exact = futures_util::stream::iter(
            (0..3).map(|at| Ok::<_, ()>(vec![at as u8; if at < 2 { 1500 } else { 1000 }])),
        );
        let body = collect_bounded(exact, 4000, &mut retained).await.unwrap();
        assert_eq!((body.len(), retained), (4000, 4000));
        assert!(body.capacity() <= 4000, "reserved {}", body.capacity());

        // A single oversized chunk is refused whole, nothing retained.
        let mut retained = 0;
        let one = futures_util::stream::iter([Ok::<_, ()>(vec![0u8; 4001])]);
        assert_eq!(
            collect_bounded(one, 4000, &mut retained).await,
            Err(BodyError::TooLarge)
        );
        assert_eq!(retained, 0);

        // A failure part-way is a failure, not a short body.
        let mut retained = 0;
        let broken = futures_util::stream::iter([Ok(vec![1u8; 10]), Err(())]);
        assert_eq!(
            collect_bounded(broken, 4000, &mut retained).await,
            Err(BodyError::Failed)
        );
    }

    /// A launcher stand-in that records its pid in `pidfile` and never exits.
    #[cfg(unix)]
    fn hung_launcher(pidfile: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", r#"echo $$ > "$1"; exec sleep 600"#, "sh"])
            .arg(pidfile);
        command
    }

    #[cfg(unix)]
    async fn launcher_pid(pidfile: &Path) -> i32 {
        super::tests::until("the launcher started", || {
            std::fs::read_to_string(pidfile).is_ok_and(|pid| !pid.trim().is_empty())
        })
        .await;
        std::fs::read_to_string(pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    // rust-robustness #5: a launcher that never exits is killed and
    // reaped at its deadline (a reaped pid no longer exists, a zombie's does).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_launcher_that_never_exits_fails_at_its_deadline_and_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let started = std::time::Instant::now();
        let outcome = run_launcher(hung_launcher(&pidfile), Duration::from_secs(3)).await;
        assert_eq!(
            outcome,
            Err("Cupola couldn't open a browser on this computer.".to_owned())
        );
        assert!(started.elapsed() < Duration::from_secs(8), "bounded");
        let pid = launcher_pid(&pidfile).await;
        assert!(
            !crate::platform::process_alive(pid),
            "pid {pid} outlived its deadline"
        );

        // One that fails, and one that works, are as before.
        let mut fails = tokio::process::Command::new("sh");
        fails.args(["-c", "exit 3"]);
        assert!(run_launcher(fails, Duration::from_secs(30)).await.is_err());
        let mut works = tokio::process::Command::new("sh");
        works.args(["-c", "exit 0"]);
        assert_eq!(run_launcher(works, Duration::from_secs(30)).await, Ok(()));
        let missing = tokio::process::Command::new("/nonexistent/launcher");
        assert!(run_launcher(missing, Duration::from_secs(30))
            .await
            .is_err());
    }

    /// An opener that runs `hung_launcher` for each launch, keeping the pid
    /// files in `dir` as `0`, `1`, …; launches from `healthy` on return at once.
    #[cfg(unix)]
    fn hanging_opener(
        dir: &Path,
        deadline: Duration,
        healthy: usize,
    ) -> (UrlOpener, Arc<AtomicUsize>) {
        let launches = Arc::new(AtomicUsize::new(0));
        let opener: UrlOpener = {
            let (dir, launches) = (dir.to_owned(), launches.clone());
            Arc::new(move |_| {
                let launch = launches.fetch_add(1, SeqCst);
                if launch >= healthy {
                    return Box::pin(std::future::ready(Ok(())));
                }
                Box::pin(run_launcher(
                    hung_launcher(&dir.join(launch.to_string())),
                    deadline,
                ))
            })
        };
        (opener, launches)
    }

    async fn connecting(fake: &super::tests::Fake, opener: UrlOpener) -> Arc<T3Code> {
        Arc::new(T3Code::new(
            Arc::new(MemoryGrantStore::default()),
            Some(fake.origin.clone()),
            opener,
        ))
    }

    /// `status` and `disconnect` answered inside `within`.
    async fn answers_promptly(t3: &Arc<T3Code>, within: Duration) -> Value {
        let t3 = t3.clone();
        tokio::time::timeout(within, tokio::task::spawn_blocking(move || t3.status()))
            .await
            .expect("status stalled behind the launcher")
            .unwrap()
    }

    // rust-robustness #5: while a launch hangs no lock is held, so status
    // answers; at the deadline Connect fails, the launcher is reaped, and an
    // ordinary Connect afterwards still works.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_hung_launch_fails_at_its_deadline_without_stalling_status() {
        let fake = super::tests::Fake::start().await;
        let dir = tempfile::tempdir().unwrap();
        let (opener, launches) = hanging_opener(dir.path(), Duration::from_secs(3), 1);
        let t3 = connecting(&fake, opener).await;
        let attempt = tokio::spawn({
            let t3 = t3.clone();
            async move { t3.connect_using(true).await }
        });
        let pid = launcher_pid(&dir.path().join("0")).await;
        assert!(crate::platform::process_alive(pid));
        let started = std::time::Instant::now();
        while !attempt.is_finished() {
            let status = answers_promptly(&t3, Duration::from_secs(1)).await;
            assert_eq!(
                status["state"], "disconnected",
                "no approval is waited for yet"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let error = attempt.await.unwrap().unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.elapsed() < Duration::from_secs(8));
        assert!(
            !crate::platform::process_alive(pid),
            "launcher {pid} was left running"
        );
        assert_eq!(t3.status()["state"], "disconnected");
        // The next launch (a working one) connects as usual.
        assert_eq!(t3.connect_using(true).await.unwrap()["state"], "waiting");
        assert_eq!(launches.load(SeqCst), 2);
    }

    // rust-robustness #5: a Disconnect or a newer Connect during a launch
    // stops the launcher and nothing of the stale attempt is installed.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_launch_installs_nothing_and_its_launcher_is_stopped() {
        let fake = super::tests::Fake::start().await;
        let dir = tempfile::tempdir().unwrap();

        // Disconnect during the launch.
        let (opener, _) = hanging_opener(dir.path(), Duration::from_secs(600), 1);
        let t3 = connecting(&fake, opener).await;
        let attempt = tokio::spawn({
            let t3 = t3.clone();
            async move { t3.connect_using(true).await }
        });
        let pid = launcher_pid(&dir.path().join("0")).await;
        let disconnected = tokio::time::timeout(Duration::from_secs(2), t3.disconnect())
            .await
            .expect("disconnect stalled behind the launcher")
            .unwrap();
        assert_eq!(disconnected["state"], "disconnected");
        let stale = attempt.await.unwrap().unwrap();
        assert_eq!(
            stale["state"], "disconnected",
            "the stale attempt installed nothing"
        );
        super::tests::until("the launcher stopped", || {
            !crate::platform::process_alive(pid)
        })
        .await;
        assert!(t3.inner.lock().unwrap().waiting.is_none());
        assert!(t3.inner.lock().unwrap().opening.is_none());

        // A newer Connect during the launch: the first is stale, the second
        // (whose launcher exits at once) is the one that waits.
        let dir = tempfile::tempdir().unwrap();
        let (opener, launches) = hanging_opener(dir.path(), Duration::from_secs(600), 1);
        let t3 = connecting(&fake, opener).await;
        let first = tokio::spawn({
            let t3 = t3.clone();
            async move { t3.connect_using(true).await }
        });
        let pid = launcher_pid(&dir.path().join("0")).await;
        assert_eq!(t3.connect_using(true).await.unwrap()["state"], "waiting");
        assert!(first.await.unwrap().is_ok(), "a stale attempt ends quietly");
        super::tests::until("the first launcher stopped", || {
            !crate::platform::process_alive(pid)
        })
        .await;
        assert_eq!(launches.load(SeqCst), 2);
        assert_eq!(
            t3.inner.lock().unwrap().attempt,
            1,
            "one approval is waited for"
        );

        // The request that began the launch goes away (its future dropped).
        let dir = tempfile::tempdir().unwrap();
        let (opener, _) = hanging_opener(dir.path(), Duration::from_secs(600), 1);
        let t3 = connecting(&fake, opener).await;
        let dropped = tokio::spawn({
            let t3 = t3.clone();
            async move { t3.connect_using(true).await }
        });
        let pid = launcher_pid(&dir.path().join("0")).await;
        dropped.abort();
        let _ = dropped.await;
        super::tests::until("the dropped launch's launcher stopped", || {
            !crate::platform::process_alive(pid)
        })
        .await;
        assert_eq!(
            answers_promptly(&t3, Duration::from_secs(1)).await["state"],
            "disconnected"
        );
    }
}
