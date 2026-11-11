//! What Wonder adds to its own sessions, per provider: its thread tools
//! (`wonder_thread_*` and `wonder_delegate`), how much of an agent's work the
//! phone shows in a chat, and an optional custom instruction.
//!
//! wonderd owns every value; the phone and the Mac app read and change the
//! same ones here. Defaults keep the agent's experience as it was: the thread
//! tools stay on (Wonder always offered them), chats show everything, and no
//! instruction is added. An instruction reaches only turns Wonder runs, never
//! the owner's sessions in the providers' own apps, and only while it is on.
use crate::{projects, AppState, OwnerAuthority};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use wonder_store::{
    AgentFamily, AgentFamilySettings, ChatDetail, InstructionDelivery, ProviderSetting,
    INSTRUCTION_MAX_CHARS,
};

/// A provider's settings; unreadable settings count as the defaults, which
/// offer the thread tools as before and add no instruction.
pub(crate) async fn settings(state: &AppState, family: AgentFamily) -> AgentFamilySettings {
    match state.store.agent_family_settings(family).await {
        Ok(settings) => settings,
        Err(error) => {
            let _ = state.logger.record(
                "warn",
                "agent_settings_unreadable",
                json!({"error": error.to_string()}),
            );
            AgentFamilySettings::default()
        }
    }
}

/// What one turn carries of these settings.
pub(crate) struct TurnAdditions {
    thread_tools: bool,
    instruction: Option<(InstructionDelivery, String)>,
    /// How Cupola's preamble reaches this harness (see [`crate::preamble`]);
    /// None for ACP, which takes it in a new session's first prompt.
    preamble: Option<InstructionDelivery>,
    /// The preamble names the thread tools: they are on and this is no
    /// review, which offers only reading tools.
    preamble_tools: bool,
}

impl TurnAdditions {
    pub(crate) async fn for_family(state: &AppState, family: AgentFamily) -> Self {
        let provider = family.provider();
        let stored = settings(state, family).await;
        let thread_tools = provider.capabilities.thread_tools && stored.thread_tools;
        Self {
            thread_tools,
            preamble_tools: thread_tools,
            instruction: provider
                .instruction_delivery
                .zip(stored.active_instruction().map(str::to_owned)),
            preamble: provider.instruction_delivery,
        }
    }

    /// The thread tools to offer, or none while the owner turned them off.
    /// A harness that takes them per turn gets an empty list, which removes
    /// tools an earlier turn offered.
    pub(crate) fn tool_specs(&self) -> Vec<Value> {
        if self.thread_tools {
            crate::thread_tools::specs()
        } else {
            Vec::new()
        }
    }

    pub(crate) fn thread_tools(&self) -> bool {
        self.thread_tools
    }

    /// The opening part of a new ACP session's first prompt: Cupola's
    /// preamble, which that harness cannot take any other way.
    pub(crate) fn first_prompt(&self) -> Option<Value> {
        self.preamble.is_none().then(|| {
            json!({"type": "text", "text": crate::preamble::first_prompt_block(self.preamble_tools)})
        })
    }

    /// Adds Cupola's preamble and the custom instruction to `turn/start`,
    /// after Wonder's skills set theirs. The instruction is sent only while
    /// it is on.
    pub(crate) fn apply(&self, params: &mut Value) {
        let preamble = crate::preamble::text(self.preamble_tools);
        match self.preamble {
            Some(InstructionDelivery::TurnContext) => {
                if !params["additionalContext"].is_object() {
                    params["additionalContext"] = json!({});
                }
                params["additionalContext"][crate::preamble::CONTEXT_KEY] =
                    json!({"kind": "application", "value": preamble});
            }
            Some(InstructionDelivery::BridgeAppend) => {
                params["wonderPreamble"] = json!(preamble);
            }
            None => {}
        }
        match &self.instruction {
            Some((InstructionDelivery::TurnContext, text)) => {
                if !params["additionalContext"].is_object() {
                    params["additionalContext"] = json!({});
                }
                params["additionalContext"][INSTRUCTION_CONTEXT_KEY] =
                    json!({"kind": "application", "value": text});
            }
            Some((InstructionDelivery::BridgeAppend, text)) => {
                params["wonderInstruction"] = json!(text);
            }
            None => {}
        }
    }
}

/// The `additionalContext` key of the owner's instruction on Codex turns.
pub(crate) const INSTRUCTION_CONTEXT_KEY: &str = "wonder_instruction";

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    projects::error(status, message)
}

fn unreadable() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Cupola couldn’t read these settings. Try again.",
    )
}

fn unsaved() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "The setting could not be saved. Try again.",
    )
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Every provider `include` keeps, with its stored settings.
async fn stored(
    state: &AppState,
    include: impl Fn(AgentFamily) -> bool,
) -> Option<Vec<(AgentFamily, AgentFamilySettings)>> {
    let mut providers = Vec::new();
    for family in AgentFamily::ALL
        .into_iter()
        .filter(|family| include(*family))
    {
        providers.push((
            family,
            state.store.agent_family_settings(family).await.ok()?,
        ));
    }
    Some(providers)
}

// Thread tools: `GET /api/v1/settings/thread-tools`,
// `PUT /api/v1/settings/thread-tools/{family}` `{"enabled": bool}`.

fn offers_thread_tools(family: AgentFamily) -> bool {
    family.provider().capabilities.thread_tools
}

async fn thread_tools_listing(state: &AppState) -> Response {
    let Some(providers) = stored(state, offers_thread_tools).await else {
        return unreadable();
    };
    Json(json!({
        "providers": providers.iter().map(|(family, settings)| json!({
            "family": family, "enabled": settings.thread_tools,
        })).collect::<Vec<_>>()
    }))
    .into_response()
}

pub(crate) async fn thread_tools(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    thread_tools_listing(&state).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetThreadTools {
    enabled: bool,
}

/// Takes effect from the provider's next turn. A Codex thread keeps the
/// tools it started with, so Wonder declines their calls while they are off.
pub(crate) async fn set_thread_tools(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(family): Path<AgentFamily>,
    Json(request): Json<SetThreadTools>,
) -> Response {
    if !offers_thread_tools(family) {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{} doesn’t get Cupola’s thread tools.",
                family.provider().display_name
            ),
        );
    }
    if state
        .store
        .set_agent_thread_tools(family, request.enabled, &now())
        .await
        .is_err()
    {
        return unsaved();
    }
    thread_tools_listing(&state).await
}

// Chat detail: `GET /api/v1/settings/chat-detail`,
// `PUT /api/v1/settings/chat-detail/{family}` `{"detail": "everything" | "conversation" | "thinking"}`.

async fn chat_detail_listing(state: &AppState) -> Response {
    let Some(providers) = stored(state, |_| true).await else {
        return unreadable();
    };
    let Ok(overrides) = state.store.chat_detail_overrides().await else {
        return unreadable();
    };
    Json(json!({
        "providers": providers.iter().map(|(family, settings)| json!({
            "family": family, "detail": settings.chat_detail.as_str(),
        })).collect::<Vec<_>>(),
        "chats": overrides.iter().map(|(conversation, detail)| (conversation.clone(), json!(detail.as_str())))
            .collect::<serde_json::Map<_, _>>(),
    }))
    .into_response()
}

// One chat's own choice: `PUT /api/v1/conversations/{id}/chat-detail`
// `{"detail": "everything" | "conversation" | "thinking" | null}`; null follows the provider.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetConversationChatDetail {
    detail: Option<String>,
}

pub(crate) async fn set_conversation_chat_detail(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(conversation): Path<String>,
    Json(request): Json<SetConversationChatDetail>,
) -> Response {
    let detail = match request.detail.as_deref().map(ChatDetail::parse) {
        None => None,
        Some(Some(detail)) => Some(detail),
        Some(None) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Choose Conversation, Commentary & tools or Thinking.",
            );
        }
    };
    if !crate::projects::is_project(&state, &conversation).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state
        .store
        .set_chat_detail_override(&conversation, detail)
        .await
    {
        Ok(true) => chat_detail_listing(&state).await,
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => unsaved(),
    }
}

pub(crate) async fn chat_detail(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    chat_detail_listing(&state).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetChatDetail {
    detail: String,
}

pub(crate) async fn set_chat_detail(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(family): Path<AgentFamily>,
    Json(request): Json<SetChatDetail>,
) -> Response {
    let Some(detail) = ChatDetail::parse(&request.detail) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose Conversation, Commentary & tools or Thinking.",
        );
    };
    if state
        .store
        .set_agent_chat_detail(family, detail, &now())
        .await
        .is_err()
    {
        return unsaved();
    }
    chat_detail_listing(&state).await
}

// Custom instructions: `GET /api/v1/settings/custom-instructions`,
// `PUT /api/v1/settings/custom-instructions/{family}` `{"enabled": bool, "text": string}`.

fn takes_instruction(family: AgentFamily) -> bool {
    family.provider().instruction_delivery.is_some()
}

async fn instructions_listing(state: &AppState) -> Response {
    let Some(providers) = stored(state, takes_instruction).await else {
        return unreadable();
    };
    Json(json!({
        "maxLength": INSTRUCTION_MAX_CHARS,
        "providers": providers.iter().map(|(family, settings)| json!({
            "family": family, "enabled": settings.instruction_enabled,
            "text": settings.instruction,
        })).collect::<Vec<_>>()
    }))
    .into_response()
}

pub(crate) async fn custom_instructions(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    instructions_listing(&state).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetCustomInstruction {
    enabled: bool,
    text: String,
}

/// Saves the text with its switch; the text is kept while it is off. Takes
/// effect from the provider's next turn.
pub(crate) async fn set_custom_instruction(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(family): Path<AgentFamily>,
    Json(request): Json<SetCustomInstruction>,
) -> Response {
    let name = family.provider().display_name;
    if !takes_instruction(family) {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{name} can’t take a custom instruction from Cupola."),
        );
    }
    let text = request.text.trim();
    if text.chars().count() > INSTRUCTION_MAX_CHARS {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Keep the instruction to 4,000 characters or fewer.",
        );
    }
    if request.enabled && text.is_empty() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Write an instruction before turning it on.",
        );
    }
    if state
        .store
        .set_agent_instruction(family, request.enabled, text, &now())
        .await
        .is_err()
    {
        return unsaved();
    }
    instructions_listing(&state).await
}

// Claude app sync: `GET /api/v1/settings/claude-app`,
// `PUT /api/v1/settings/claude-app` `{"enabled": bool}`. Off by default.

/// The setting of the Claude app integration (`desktop_activity`: Claude's
/// `claude://` links and the app's own Claude Code processes), so it is
/// Claude's alone rather than a capability other providers could offer.
const CLAUDE_APP_SYNC: (AgentFamily, ProviderSetting) =
    (AgentFamily::Claude, ProviderSetting::ClaudeAppSync);

/// Whether Cupola keeps the Claude app in step with the Claude chats it runs:
/// it adds new chats to the app and closes the app's copy of a chat before
/// sending to it. Off (the default, and when unreadable), Cupola never opens
/// or stops the Claude app on its own: a chat open there waits, like one open
/// in a terminal, until the owner closes it there or chooses Send now.
pub(crate) async fn claude_app_sync(state: &AppState) -> bool {
    let (family, setting) = CLAUDE_APP_SYNC;
    state
        .store
        .provider_setting(family, setting)
        .await
        .unwrap_or(false)
}

async fn claude_app_listing(state: &AppState) -> Response {
    let (family, setting) = CLAUDE_APP_SYNC;
    let Ok(enabled) = state.store.provider_setting(family, setting).await else {
        return unreadable();
    };
    Json(json!({
        "enabled": enabled,
        "appInstalled": crate::desktop_activity::claude_app_installed(),
    }))
    .into_response()
}

pub(crate) async fn claude_app(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    claude_app_listing(&state).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetClaudeApp {
    enabled: bool,
}

/// Takes effect from the next send and the next new chat.
pub(crate) async fn set_claude_app(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Json(request): Json<SetClaudeApp>,
) -> Response {
    let (family, setting) = CLAUDE_APP_SYNC;
    if state
        .store
        .set_provider_setting(family, setting, request.enabled, &now())
        .await
        .is_err()
    {
        return unsaved();
    }
    claude_app_listing(&state).await
}

#[cfg(all(test, unix))]
mod tests {
    use crate::test_support::{call, fixture};
    use axum::http::StatusCode;
    use serde_json::{json, Value};

    // Contract: chat detail has one owner, the host. A paired phone's session
    // and the Mac app's loopback capability read and change the same stored
    // value, so a change from either reaches the other on its next read.
    #[tokio::test]
    async fn chat_detail_is_one_value_for_the_phone_and_the_mac() {
        use axum::body::{to_bytes, Body};
        use axum::http::Request;
        use sha2::Digest;
        use tower::ServiceExt;
        let (_dir, state) = fixture().await;
        {
            let point = p256::ecdsa::SigningKey::from_slice(&[7_u8; 32])
                .unwrap()
                .verifying_key()
                .to_sec1_point(false);
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            use base64::Engine;
            let mut pairing = state.pairing.lock().await;
            pairing.restore_device(
                "phone".into(),
                wonder_api::pairing_protocol::DevicePublicKeyJwk {
                    kty: "EC".into(),
                    crv: "P-256".into(),
                    x: b64.encode(point.x().unwrap()),
                    y: b64.encode(point.y().unwrap()),
                },
                false,
                None,
            );
            pairing.restore_session(
                wonder_api::pairing_protocol::session_token_hash("session-token"),
                "phone".into(),
                sha2::Sha256::digest(b"csrf-token").into(),
                crate::now_ms() + 60_000,
            );
        }
        let phone = |method: &str, path: &str, body: Value| {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("origin", &state.public_origin)
                .header("cookie", "__Host-wonder_session=session-token")
                .header("x-wonder-csrf", "csrf-token")
                .header("content-type", "application/json")
                .body(Body::from(if body.is_null() {
                    String::new()
                } else {
                    body.to_string()
                }))
                .unwrap();
            let state = state.clone();
            async move {
                let response = crate::router(state).oneshot(request).await.unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
                (
                    status,
                    serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
                )
            }
        };
        let claude = |listing: &Value| listing["providers"][1].clone();

        // The Mac's Agents page changes it; the phone reads that.
        let mac = call(
            &state,
            "PUT",
            "/api/v1/settings/chat-detail/claude",
            json!({"detail": "conversation"}),
        )
        .await;
        assert_eq!(mac.0, StatusCode::OK, "{}", mac.1);
        let read = phone("GET", "/api/v1/settings/chat-detail", Value::Null).await;
        assert_eq!(read.0, StatusCode::OK, "{}", read.1);
        assert_eq!(
            claude(&read.1),
            json!({"family":"claude","detail":"conversation"})
        );

        // The phone changes it back; the Mac reads that.
        let set = phone(
            "PUT",
            "/api/v1/settings/chat-detail/claude",
            json!({"detail": "everything"}),
        )
        .await;
        assert_eq!(set.0, StatusCode::OK, "{}", set.1);
        let read = call(&state, "GET", "/api/v1/settings/chat-detail", Value::Null).await;
        assert_eq!(
            claude(&read.1),
            json!({"family":"claude","detail":"everything"})
        );
    }

    // Contract: Claude app sync is off until the owner turns it on; a change
    // persists on the host and is answered in the published shape. Off, the
    // daemon reads it as off for sends, holders and imports.
    #[tokio::test]
    async fn claude_app_sync_reads_off_until_turned_on() {
        let (_dir, state) = fixture().await;
        let path = "/api/v1/settings/claude-app";
        let read = call(&state, "GET", path, Value::Null).await;
        assert_eq!(read.0, StatusCode::OK, "{}", read.1);
        crate::tests::validate_http_contract("claudeAppSettings", &read.1);
        assert_eq!(read.1["enabled"], false);
        assert!(!super::claude_app_sync(&state).await);
        let on = call(&state, "PUT", path, json!({"enabled": true})).await;
        assert_eq!(on.0, StatusCode::OK, "{}", on.1);
        crate::tests::validate_http_contract("claudeAppSettings", &on.1);
        assert_eq!(on.1["enabled"], true);
        assert!(super::claude_app_sync(&state).await);
        assert_eq!(call(&state, "GET", path, Value::Null).await.1, on.1);
        let refused = call(
            &state,
            "PUT",
            path,
            json!({"enabled": true, "family": "codex"}),
        )
        .await;
        assert!(refused.0.is_client_error(), "{}", refused.1);
        let off = call(&state, "PUT", path, json!({"enabled": false})).await;
        assert_eq!(off.1["enabled"], false);
        assert!(!super::claude_app_sync(&state).await);
    }

    // Contract: every setting reads its default until changed (thread tools
    // on, everything shown, no instruction), a change persists on the host
    // and is answered with the whole current list in the published shape, and
    // a provider or value the setting does not take is refused.
    #[tokio::test]
    async fn settings_read_defaults_and_changes_persist() {
        let (_dir, state) = fixture().await;
        let check = |definition: &str, value: &Value| {
            crate::tests::validate_http_contract(definition, value);
        };

        let tools = call(&state, "GET", "/api/v1/settings/thread-tools", Value::Null).await;
        assert_eq!(tools.0, StatusCode::OK, "{}", tools.1);
        check("threadToolSettings", &tools.1);
        assert_eq!(
            tools.1["providers"],
            json!([{"family":"codex","enabled":true},{"family":"claude","enabled":true},
                   {"family":"opencode","enabled":true},{"family":"cursor","enabled":true}])
        );
        let off = call(
            &state,
            "PUT",
            "/api/v1/settings/thread-tools/claude",
            json!({"enabled": false}),
        )
        .await;
        assert_eq!(off.0, StatusCode::OK, "{}", off.1);
        check("threadToolSettings", &off.1);
        assert_eq!(
            off.1["providers"][1],
            json!({"family":"claude","enabled":false})
        );
        assert_eq!(off.1["providers"][0]["enabled"], true);

        let detail = call(&state, "GET", "/api/v1/settings/chat-detail", Value::Null).await;
        assert_eq!(detail.0, StatusCode::OK, "{}", detail.1);
        check("chatDetailSettings", &detail.1);
        assert!(detail.1["providers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|provider| provider["detail"] == "everything"));
        let changed = call(
            &state,
            "PUT",
            "/api/v1/settings/chat-detail/opencode",
            json!({"detail": "conversation"}),
        )
        .await;
        assert_eq!(changed.0, StatusCode::OK, "{}", changed.1);
        check("chatDetailSettings", &changed.1);
        assert_eq!(
            changed.1["providers"][2],
            json!({"family":"opencode","detail":"conversation"})
        );
        let thinking = call(
            &state,
            "PUT",
            "/api/v1/settings/chat-detail/codex",
            json!({"detail": "thinking"}),
        )
        .await;
        assert_eq!(thinking.0, StatusCode::OK, "{}", thinking.1);
        check("chatDetailSettings", &thinking.1);
        assert_eq!(
            thinking.1["providers"][0],
            json!({"family":"codex","detail":"thinking"})
        );
        assert_eq!(
            call(
                &state,
                "PUT",
                "/api/v1/settings/chat-detail/codex",
                json!({"detail": "summary"})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        let path = "/api/v1/settings/custom-instructions";
        let listed = call(&state, "GET", path, Value::Null).await;
        assert_eq!(listed.0, StatusCode::OK, "{}", listed.1);
        check("customInstructionSettings", &listed.1);
        assert_eq!(
            listed.1,
            json!({"maxLength": 4000, "providers": [
                {"family":"codex","enabled":false,"text":""},
                {"family":"claude","enabled":false,"text":""}]})
        );
        let saved = call(
            &state,
            "PUT",
            &format!("{path}/codex"),
            json!({"enabled": true, "text": "  Answer briefly.\n"}),
        )
        .await;
        assert_eq!(saved.0, StatusCode::OK, "{}", saved.1);
        check("customInstructionSettings", &saved.1);
        assert_eq!(
            saved.1["providers"][0],
            json!({"family":"codex","enabled":true,"text":"Answer briefly."})
        );
        // Off keeps the text.
        let kept = call(
            &state,
            "PUT",
            &format!("{path}/codex"),
            json!({"enabled": false, "text": "Answer briefly."}),
        )
        .await;
        assert_eq!(
            kept.1["providers"][0],
            json!({"family":"codex","enabled":false,"text":"Answer briefly."})
        );
        for (family, body) in [
            ("codex", json!({"enabled": true, "text": "   "})),
            ("codex", json!({"enabled": false, "text": "x".repeat(4001)})),
            ("opencode", json!({"enabled": false, "text": "Hi"})),
        ] {
            let refused = call(&state, "PUT", &format!("{path}/{family}"), body).await;
            assert_eq!(refused.0, StatusCode::UNPROCESSABLE_ENTITY, "{family}");
        }
        let unchanged = call(&state, "GET", path, Value::Null).await;
        assert_eq!(unchanged.1, kept.1);
    }
}
