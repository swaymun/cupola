//! Wonder RPC: JSON-RPC 2.0 for any client, over the `/api/v1/rpc`
//! WebSocket or the stdio of `wonder rpc`. The guide is `crates/wonderd/RPC.md`.
//!
//! Every method other than the protocol ones (`initialize`, `auth.challenge`,
//! `events.*`) is one fixed call to an existing HTTP route, run through the
//! same router and authentication middleware as the phone's request. This
//! table is an allowlist, not a generic forwarder: a method's params are typed,
//! and only the route named here can be reached through it. The exception is
//! `http.request`, served only to a paired device inside the sealed channel:
//! the phone's own HTTP API, under the same checks as on the home network.

pub mod client;
pub mod sealed;
pub(crate) mod server;

use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// The protocol's major version. A client names it in `initialize`; a host
/// that does not speak it refuses with [`codes::UNSUPPORTED_VERSION`].
/// Additive changes (new methods, optional params, result fields) keep it.
pub const PROTOCOL_VERSION: u32 = 1;

pub const JSONRPC: &str = "2.0";

/// The device ID that a send from the local owner (`wonder rpc`) carries;
/// it is the identity the desktop companion has always used.
pub const LOCAL_SENDER: &str = "wonder-desktop";

pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Any method but `initialize` and `auth.challenge` before `initialize`.
    pub const NOT_INITIALIZED: i64 = -32090;
    pub const ALREADY_INITIALIZED: i64 = -32091;
    /// More requests in flight on this connection than the host serves.
    pub const BUSY: i64 = -32092;
    pub const UNSUPPORTED_VERSION: i64 = -32093;

    /// A route refusal: `-32000 - (status - 400)` for 4xx and
    /// `-32100 - (status - 500)` for 5xx, so 404 is -32004 and 503 is -32103.
    pub fn for_http_status(status: u16) -> i64 {
        match status {
            400..=499 => -32000 - (i64::from(status) - 400),
            500..=599 => -32100 - (i64::from(status) - 500),
            _ => INTERNAL_ERROR,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Absent for a notification from the client (none are defined yet; the
    /// host ignores them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    pub jsonrpc: String,
    /// `null` only when the request could not be read far enough to find it.
    pub id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// What the host makes of one incoming frame. The host and the `wonder rpc`
/// relay both read frames through [`read_frame`], so the relay knows which
/// lines get an answer and under which ID.
#[derive(Debug)]
pub enum Incoming {
    /// A call, answered once under its ID.
    Call(RequestId, Request),
    /// A client notification, never answered.
    Notification,
    /// Answered once with this error, under the frame's ID when it has a
    /// readable one and `null` otherwise.
    Invalid(Option<RequestId>, RpcError),
}

pub use server::MAX_REQUEST_BYTES;

/// The answer to a request over [`MAX_REQUEST_BYTES`], under a `null` ID: its
/// ID is never read. `wonder rpc` gives it locally for an oversized line.
pub fn too_large() -> RpcError {
    RpcError::new(
        codes::INVALID_REQUEST,
        "This request is larger than the host reads.",
    )
}

pub fn read_frame(text: &str) -> Incoming {
    if text.len() > MAX_REQUEST_BYTES {
        return Incoming::Invalid(None, too_large());
    }
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            return Incoming::Invalid(
                None,
                RpcError::new(codes::PARSE_ERROR, format!("Not JSON: {error}")),
            )
        }
    };
    if value.is_array() {
        return Incoming::Invalid(
            None,
            RpcError::new(
                codes::INVALID_REQUEST,
                "Batches are not supported; send one request per frame or line.",
            ),
        );
    }
    let id = value
        .get("id")
        .and_then(|id| serde_json::from_value::<RequestId>(id.clone()).ok());
    match serde_json::from_value::<Request>(value) {
        Ok(request) if request.jsonrpc == JSONRPC => match request.id.clone() {
            Some(id) => Incoming::Call(id, request),
            None => Incoming::Notification,
        },
        _ => Incoming::Invalid(
            id,
            RpcError::new(codes::INVALID_REQUEST, "Not a JSON-RPC 2.0 request."),
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RpcError {
    pub code: i64,
    /// User language; safe to show as is.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ErrorData>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorData {
    /// The status the route answered with, for a route refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// The route's JSON error body when it sent one (for example
    /// `{"error":"idempotency_conflict"}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// Protocol versions this host speaks, on `UNSUPPORTED_VERSION`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported: Option<Vec<u32>>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params(detail: impl std::fmt::Display) -> Self {
        Self::new(codes::INVALID_PARAMS, format!("Invalid params: {detail}"))
    }

    /// A route refusal; its `detail` (or plain-text body) is already user
    /// language, as the CLI has always shown it.
    pub fn from_http(status: u16, body: &[u8]) -> Self {
        let json = serde_json::from_slice::<Value>(body).ok();
        let detail = json
            .as_ref()
            .and_then(|v| v["detail"].as_str().map(str::to_owned))
            .unwrap_or_else(|| {
                if json.is_some() {
                    String::new()
                } else {
                    String::from_utf8_lossy(body).trim().to_owned()
                }
            });
        let message = if detail.is_empty() {
            default_message(status).to_owned()
        } else {
            detail
        };
        Self {
            code: codes::for_http_status(status),
            message,
            data: Some(ErrorData {
                http_status: Some(status),
                body: json.filter(|value| value.is_object()),
                supported: None,
            }),
        }
    }

    pub fn http_status(&self) -> Option<u16> {
        self.data.as_ref().and_then(|data| data.http_status)
    }
}

fn default_message(status: u16) -> &'static str {
    match status {
        401 => "This device is not signed in to this computer.",
        403 => "This action is not allowed from this connection.",
        404 => "Not found.",
        409 => "This changed on the computer. Refresh and try again.",
        503 => "Cupola is busy or still starting. Try again shortly.",
        _ => "The request failed.",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

/// Who may call a method. The route enforces it; this documents it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Authority {
    /// The local owner (`wonder rpc`) or a paired device.
    Any,
    /// Only the local owner: pairing and other computer-side decisions.
    Owner,
    /// Only a paired device, with a signature over the action (approvals).
    PairedSigned,
    /// The local owner, or a paired device signing the action.
    OwnerOrSigned,
    /// Only a paired device inside the sealed channel (`/api/v1/rpc/sealed`).
    SealedDevice,
}

/// What a method's result is, for the schema.
#[derive(Debug, Clone, Copy)]
pub enum ResultShape {
    /// A named definition from `packages/protocol/schemas/wonder-http-v1.json`.
    Http(&'static str),
    /// An array of one.
    HttpArray(&'static str),
    /// An RPC-owned type.
    Typed(fn(&mut SchemaGenerator) -> Schema),
    /// The route answers with no body; the result is `{}`.
    Empty,
    /// An object the HTTP authority does not define yet.
    Object,
}

pub struct Method {
    pub name: &'static str,
    pub summary: &'static str,
    pub authority: Authority,
    /// `"GET /api/v1/host/status"`, or `None` for a protocol method.
    pub route: Option<&'static str>,
    pub params: fn(&mut SchemaGenerator) -> Schema,
    pub result: ResultShape,
    /// Callable before `initialize`.
    pub before_initialize: bool,
}

fn schema_of<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<T>()
}

fn no_params(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "description": "No params: omit them, or send `{}`.",
        "type": "object",
        "additionalProperties": false
    })
}

macro_rules! method {
    ($name:literal, $route:expr, $authority:ident, $params:expr, $result:expr, $summary:literal) => {
        Method {
            name: $name,
            summary: $summary,
            authority: Authority::$authority,
            route: $route,
            params: $params,
            result: $result,
            before_initialize: false,
        }
    };
}

/// The method table. Order is the order of the guide and the schema.
pub static METHODS: &[Method] =
    &[
        Method {
            name: "initialize",
            summary: "Opens the session: agrees the protocol version and, on a paired \
                  device, proves the session with a signed challenge.",
            authority: Authority::Any,
            route: None,
            params: schema_of::<InitializeParams>,
            result: ResultShape::Typed(schema_of::<InitializeResult>),
            before_initialize: true,
        },
        Method {
            name: "auth.challenge",
            summary: "Issues the one-time challenge a paired device signs for `initialize`.",
            authority: Authority::Any,
            route: Some("GET /api/v1/events/challenge"),
            params: no_params,
            result: ResultShape::Object,
            before_initialize: true,
        },
        method!(
            "host.status",
            Some("GET /api/v1/host/status"),
            Any,
            no_params,
            ResultShape::Http("hostStatus"),
            "The computer's name, platform, public origin and whether it can run agents now."
        ),
        method!(
            "host.providers",
            Some("GET /api/v1/host/providers"),
            Owner,
            no_params,
            ResultShape::Object,
            "Each provider runtime on this computer and whether it is running."
        ),
        method!(
            "media.orphans",
            Some("GET /api/v1/host/media/orphans"),
            Owner,
            schema_of::<crate::media_refs::OrphanParams>,
            ResultShape::Typed(schema_of::<crate::media_refs::OrphanReport>),
            "Preview files no conversation names any more, older than the age limit: what media.prune would delete. Changes nothing."
        ),
        method!(
            "media.prune",
            Some("POST /api/v1/host/media/orphans/prune"),
            Owner,
            schema_of::<crate::media_refs::OrphanParams>,
            ResultShape::Typed(schema_of::<crate::media_refs::OrphanReport>),
            "Deletes the preview files media.orphans lists, and preview folders left empty."
        ),
        method!(
            "host.name",
            Some("GET /api/v1/host/name"),
            Any,
            no_params,
            ResultShape::Http("hostNameSettings"),
            "The name phones show for this computer."
        ),
        method!(
            "host.rename",
            Some("PUT /api/v1/host/name"),
            Any,
            schema_of::<HostRenameParams>,
            ResultShape::Http("hostNameSettings"),
            "Renames this computer; an empty or null name goes back to its own name."
        ),
        method!(
            "devices.list",
            Some("GET /api/v1/devices"),
            Any,
            no_params,
            ResultShape::HttpArray("deviceSummary"),
            "Paired devices, including disconnected ones."
        ),
        method!(
            "devices.revoke",
            Some("POST /api/v1/devices/{deviceId}/revoke"),
            OwnerOrSigned,
            schema_of::<DevicesRevokeParams>,
            ResultShape::Empty,
            "Disconnects a device: its session ends at once."
        ),
        method!(
            "devices.rename",
            Some("PATCH /api/v1/devices/{deviceId}"),
            Owner,
            schema_of::<DeviceRenameParams>,
            ResultShape::Http("deviceSummary"),
            "Renames a device in the list."
        ),
        method!(
            "devices.forget",
            Some("POST /api/v1/devices/{deviceId}/forget"),
            Owner,
            schema_of::<DeviceParams>,
            ResultShape::Empty,
            "Removes a disconnected device from the list."
        ),
        method!(
            "pairing.offer",
            Some("POST /api/v1/pairing/offers"),
            Owner,
            schema_of::<PairingOfferParams>,
            ResultShape::Object,
            "Creates a pairing offer (link and code) for a new device."
        ),
        method!(
            "pairing.cancel",
            Some("POST /api/v1/pairing/offers/{offerId}/cancel"),
            Owner,
            schema_of::<PairingCancelParams>,
            ResultShape::Empty,
            "Withdraws an unclaimed pairing offer."
        ),
        method!(
            "pairing.pending",
            Some("GET /api/v1/pairing/pending"),
            Owner,
            no_params,
            ResultShape::Object,
            "Devices that claimed an offer and wait for the owner's decision."
        ),
        method!(
            "pairing.confirm",
            Some("POST /api/v1/pairing/pending/{deviceId}/confirm"),
            Owner,
            schema_of::<PairingConfirmParams>,
            ResultShape::Empty,
            "Lets a waiting device connect."
        ),
        method!(
            "pairing.reject",
            Some("POST /api/v1/pairing/pending/{deviceId}/reject"),
            Owner,
            schema_of::<DeviceParams>,
            ResultShape::Empty,
            "Refuses a waiting device."
        ),
        method!(
            "relay.status",
            Some("GET /api/v1/relay"),
            Any,
            no_params,
            ResultShape::Typed(schema_of::<crate::relay::RelayView>),
            "Whether this computer can be reached through Cupola's relay, and its address."
        ),
        method!(
            "relay.set",
            Some("PUT /api/v1/relay/settings"),
            Owner,
            schema_of::<RelaySetParams>,
            ResultShape::Typed(schema_of::<crate::relay::RelayView>),
            "Allows the relay on this computer, or turns it off and releases its address."
        ),
        method!(
            "relay.enable",
            Some("POST /api/v1/relay/enable"),
            OwnerOrSigned,
            schema_of::<RelayEnableParams>,
            ResultShape::Typed(schema_of::<crate::relay::RelayView>),
            "Starts the relay with a signed-in phone's one-time grant for this computer."
        ),
        method!(
            "providers.list",
            Some("GET /api/v1/providers"),
            Any,
            schema_of::<ProvidersListParams>,
            ResultShape::Http("providersResponse"),
            "Codex, Claude Code, OpenCode and Cursor: sign-in, models and capabilities."
        ),
        method!(
            "providers.reconnect",
            Some("POST /api/v1/host/providers/{provider}/reconnect"),
            Owner,
            schema_of::<ProviderParams>,
            ResultShape::Object,
            "Finds and checks a provider again, so it reads a new sign-in."
        ),
        method!(
            "voice.status",
            Some("GET /api/v1/voice"),
            Any,
            no_params,
            ResultShape::Http("voiceStatus"),
            "Whether voice is set up on this computer."
        ),
        method!(
            "voice.setKey",
            Some("PUT /api/v1/voice/openai-key"),
            Any,
            schema_of::<VoiceSetKeyParams>,
            ResultShape::Http("voiceStatus"),
            "Saves the OpenAI API key voice uses."
        ),
        method!(
            "voice.removeKey",
            Some("DELETE /api/v1/voice/openai-key"),
            Any,
            no_params,
            ResultShape::Http("voiceStatus"),
            "Removes the saved voice key."
        ),
        method!(
            "voice.dictionary",
            Some("GET /api/v1/voice/dictionary"),
            Owner,
            no_params,
            ResultShape::Http("voiceDictionary"),
            "The words voice mode transcribes with care."
        ),
        method!(
            "voice.updateDictionary",
            Some("PUT /api/v1/voice/dictionary"),
            Owner,
            schema_of::<VoiceDictionaryParams>,
            ResultShape::Http("voiceDictionary"),
            "Adds and removes voice dictionary words; used when the next voice session starts."
        ),
        method!(
            "update.work",
            Some("GET /api/v1/host/update/work"),
            Owner,
            no_params,
            ResultShape::Object,
            "What a restart would interrupt: `running` replies and whether \
         `blocksUpdate`."
        ),
        method!(
            "update.prepare",
            Some("POST /api/v1/host/update/prepare"),
            Owner,
            schema_of::<UpdateParams>,
            ResultShape::Object,
            "Pauses work durably for a restart; refuses if more replies run than \
         `consentedRunning`."
        ),
        method!(
            "update.cancel",
            Some("POST /api/v1/host/update/cancel"),
            Owner,
            schema_of::<UpdateParams>,
            ResultShape::Empty,
            "Lets the host take work again after a restart that did not happen."
        ),
        method!(
            "projects.list",
            Some("GET /api/v1/projects"),
            Any,
            no_params,
            ResultShape::Http("projectsResponse"),
            "Projects and pinned threads."
        ),
        method!(
            "threads.list",
            Some("GET /api/v1/projects/{projectId}/threads"),
            Any,
            schema_of::<ThreadsListParams>,
            ResultShape::Http("projectThreadsPage"),
            "One page of a Project's threads, most recent first."
        ),
        method!(
            "models.list",
            Some("GET /api/v1/agent-options"),
            Any,
            no_params,
            ResultShape::Http("agentOptions"),
            "The models each provider offers here; `isDefault` marks the one a new thread uses."
        ),
        method!(
            "threads.create",
            Some("POST /api/v1/projects/{projectId}/threads"),
            Any,
            schema_of::<ThreadsCreateParams>,
            ResultShape::Http("createProjectThreadResponse"),
            "Starts a thread in a Project with its first message. Idempotent per \
         `idempotencyKey`, as `threads.send`."
        ),
        method!("threads.attach", Some("POST /api/v1/projects/{projectId}/threads/attach"), Any,
        schema_of::<ThreadsAttachParams>, ResultShape::Object,
        "Gives a provider thread without a `conversationId` one, so it can be read and sent to."),
        method!("threads.delegate", Some("POST /api/v1/conversations/{threadId}/delegations"), Any,
        schema_of::<ThreadsDelegateParams>, ResultShape::Object,
        "Starts a task as a child of a thread, like the agent tool `wonder_delegate`: the child shows in the parent as an agent task. Idempotent per `idempotencyKey`."),
        method!(
            "threads.read",
            Some("GET /api/v1/conversations/{threadId}"),
            Any,
            schema_of::<ThreadsReadParams>,
            ResultShape::Http("conversationSnapshot"),
            "A thread's messages, newest page first; page back with `before`."
        ),
        method!(
            "threads.notifications", Some("GET /api/v1/conversations/{threadId}/notifications"), Any,
            schema_of::<ThreadParams>, ResultShape::Http("chatNotifications"),
            "The stored notification level for a chat."
        ),
        method!(
            "threads.setNotifications", Some("PUT /api/v1/conversations/{threadId}/notifications"), Any,
            schema_of::<ThreadNotificationsParams>, ResultShape::Http("chatNotifications"),
            "Sets a chat to default, follow or mute on this computer."
        ),
        method!(
            "threads.moveToWorktree", Some("POST /api/v1/project-conversations/{threadId}/worktree"), Any,
            schema_of::<ThreadsMoveToWorktreeParams>, ResultShape::Object,
            "Moves an idle thread onto a new git worktree and branch beside its repository; the thread keeps its history and carries on there."
        ),
        method!(
            "threads.send",
            Some("POST /api/v1/conversations/{threadId}/messages"),
            Any,
            schema_of::<ThreadsSendParams>,
            ResultShape::Http("messageReceipt"),
            "Sends a message. Idempotent: resending the same `idempotencyKey` and body \
         returns the first receipt; a different body is a conflict."
        ),
        method!(
            "threads.queue",
            Some("GET /api/v1/conversations/{threadId}/queue"),
            Any,
            schema_of::<ThreadParams>,
            ResultShape::Http("queueList"),
            "Messages waiting in a thread's queue, next first."
        ),
        method!(
            "queue.edit",
            Some("POST /api/v1/conversations/{threadId}/queue/{messageId}"),
            Any,
            schema_of::<QueueEditParams>,
            ResultShape::Empty,
            "Changes or cancels a queued message, or sends it now into the running turn \
         (`expectedTurnId`)."
        ),
        method!(
            "approvals.list",
            Some("GET /api/v1/approvals"),
            Any,
            no_params,
            ResultShape::HttpArray("approvalSummary"),
            "Requests waiting for a decision."
        ),
        method!(
            "approvals.resolve",
            Some("POST /api/v1/approvals/{approvalId}/resolve"),
            PairedSigned,
            schema_of::<ApprovalsResolveParams>,
            ResultShape::Object,
            "Answers an approval. Only a paired device can, with a signature over the decision."
        ),
        Method {
            name: "events.checkpoint",
            summary: "The current event cursor. Read it before reading state, then \
                  subscribe from it, so nothing falls between the two.",
            authority: Authority::Any,
            route: Some("GET /api/v1/sync/checkpoint"),
            params: no_params,
            result: ResultShape::Http("readCheckpoint"),
            before_initialize: false,
        },
        Method {
            name: "events.subscribe",
            summary: "Streams host events as `event` notifications, replaying from a cursor.",
            authority: Authority::Any,
            route: None,
            params: schema_of::<EventsSubscribeParams>,
            result: ResultShape::Typed(schema_of::<EventsSubscribeResult>),
            before_initialize: false,
        },
        Method {
            name: "events.unsubscribe",
            summary: "Ends a subscription.",
            authority: Authority::Any,
            route: None,
            params: schema_of::<EventsUnsubscribeParams>,
            result: ResultShape::Empty,
            before_initialize: false,
        },
        Method {
            name: "http.request",
            summary: "Runs one of the device's own HTTP API calls inside the sealed \
                  channel, so a phone on the relay works as it does at home.",
            authority: Authority::SealedDevice,
            route: None,
            params: schema_of::<HttpRequestParams>,
            result: ResultShape::Typed(schema_of::<HttpResponse>),
            before_initialize: false,
        },
    ];

pub fn method(name: &str) -> Option<&'static Method> {
    METHODS.iter().find(|method| method.name == name)
}

/// Notifications the host sends.
pub const NOTIFICATIONS: &[(&str, &str)] = &[
    (
        "event",
        "One committed host event for a subscription, in sequence order.",
    ),
    (
        "events.ended",
        "A subscription stopped; `reason` says why and whether to read state again.",
    ),
    (
        "events.batch",
        "Committed incremental events for an opted-in subscription, in sequence order.",
    ),
];

// ----- Params and results -----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializeParams {
    /// The major version the client speaks; this host speaks 1.
    pub protocol_version: u32,
    pub client_info: ClientInfo,
    #[serde(default)]
    pub capabilities: ClientCapabilities,
    /// Required on `/api/v1/rpc` from a paired device; omitted by the local owner.
    #[serde(default)]
    pub auth: Option<DeviceAuth>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientCapabilities {
    /// Ask for the compact conversation view (`x-wonder-history-view:
    /// compact`) in `threads.read` and events.
    #[serde(default)]
    pub compact_view: bool,
}

/// The session proof a paired device gives, as for `/api/v1/events`: the
/// CSRF token issued with its session, and its signature over a fresh
/// `auth.challenge`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceAuth {
    pub device_id: String,
    pub csrf_token: String,
    pub challenge_id: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub server_info: ServerInfo,
    /// `owner` for the local owner, `device` for a paired device.
    pub authority: String,
    /// The paired device this session acts as; absent for the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub host_epoch: String,
    pub host_installation_id: String,
    /// Every method this host serves. Gate a feature on its method being here.
    pub methods: Vec<String>,
    pub notifications: Vec<String>,
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    /// The HTTP API version the routes behind the methods speak.
    pub api_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    pub compact_view: bool,
    /// `events.subscribe` accepts `batch: true` and emits `events.batch`.
    #[serde(default)]
    pub event_batches: bool,
    /// Stateless compression supported by stored-history HTTP responses only.
    #[serde(default)]
    pub history_compression: Vec<String>,
    /// Requests one connection may have in flight at once.
    pub max_in_flight: u32,
    /// The largest request line or frame the host reads, in bytes.
    pub max_request_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostRenameParams {
    /// Empty or null goes back to the computer's own name.
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceParams {
    pub device_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceRenameParams {
    pub device_id: String,
    /// 1-160 characters, no control characters.
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DevicesRevokeParams {
    pub device_id: String,
    /// Required from a paired device: its signature over `device.revoke`.
    /// The local owner omits it.
    #[serde(default)]
    pub signed: Option<SignedAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelaySetParams {
    /// `false` also stops the tunnel and releases the relay address.
    pub allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayEnableParams {
    /// The Worker's one-time grant (`POST /v1/relay/grants`).
    pub grant: String,
    /// The Worker's label for this computer's host key.
    pub label: String,
    /// Required from a paired device: its signature over `relay.enable`
    /// (target `/api/v1/relay/enable`, body `{"grant","label"}`, expected
    /// state `allowed`). The local owner omits it.
    #[serde(default)]
    pub signed: Option<SignedAction>,
}

/// One HTTP API call a phone makes through the sealed channel. It runs with
/// the channel's session and CSRF token through the same router and checks
/// as on the home network; only device API paths are served.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpRequestParams {
    /// Opt in only after initialize advertises historyCompression. Applies
    /// only to stored history GETs; auth and arbitrary calls stay uncompressed.
    #[serde(default)]
    pub response_compression: Option<String>,
    /// `GET`, `POST`, `PUT`, `PATCH` or `DELETE`.
    pub method: String,
    /// An `/api/v1/` path and query, percent-encoded as on the wire.
    pub path: String,
    /// The JSON request body, as text.
    #[serde(default)]
    pub body: Option<String>,
    /// The compact conversation view (the HTTP API's `x-wonder-history-view`).
    #[serde(default)]
    pub compact: bool,
}

/// The route's answer, whatever its status.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HttpResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncompressed_bytes: Option<usize>,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// The `x-wonder-error` code a client recovers from, when the route sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// A phone-sized picture's own SHA-256 (`x-wonder-preview-sha256`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_sha256: Option<String>,
    /// The response body, base64 (standard, padded).
    pub body: String,
}

/// Whether `http.request` serves `path`: device API paths only. Sockets,
/// pairing (claims, offers, codes) and MCP stay off it; a paired phone's
/// session renewal is allowed, since it is how the phone checks its access.
pub fn sealed_http_path(path: &str) -> bool {
    let resource = path.split('?').next().unwrap_or_default();
    path.starts_with("/api/v1/")
        && path
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'#' && byte != b'\\')
        && !resource
            .split('/')
            .any(|segment| segment == ".." || segment == ".")
        && !resource.contains("//")
        && !matches!(
            resource,
            "/api/v1/rpc" | "/api/v1/rpc/sealed" | "/api/v1/events"
        )
        && (!resource.starts_with("/api/v1/pairing")
            || matches!(
                resource,
                "/api/v1/pairing/session" | "/api/v1/pairing/session/refresh-challenge"
            ))
        && !resource.starts_with("/api/v1/mcp")
        && !resource.ends_with("/socket")
}

/// `signedActionFields` in the HTTP authority.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignedAction {
    pub action_nonce: String,
    pub issued_at_ms: u64,
    pub signature: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingOfferParams {
    /// `home` makes a link on this computer's home network address, for a
    /// phone on the same network. Omit it for the main route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<PairingOfferRoute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PairingOfferRoute {
    Home,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingCancelParams {
    pub offer_id: String,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingConfirmParams {
    pub device_id: String,
    /// Verification text read from the phone; never echoed in a result.
    pub code: String,
    #[cfg(feature = "dev-pairing")]
    #[serde(default)]
    #[schemars(skip)] // Private test fixture bypass, outside the published protocol.
    pub dev_approve: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProvidersListParams {
    /// Check sign-in again instead of answering from the last check.
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Codex,
    Claude,
    Opencode,
    Cursor,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Cursor => "cursor",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderParams {
    pub provider: ProviderId,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateParams {
    /// A UUID naming this update; `update.cancel` repeats it.
    pub request_id: String,
    /// Running replies the owner agreed to pause; omit to pause any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consented_running: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceSetKeyParams {
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceDictionaryParams {
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadsListParams {
    pub project_id: String,
    /// `nextCursor` from the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// 1–30; default 5.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadsAttachParams {
    pub project_id: String,
    /// The thread's `reference` from `threads.list`.
    pub reference: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadsDelegateParams {
    /// The parent thread's `conversationId`.
    pub thread_id: String,
    /// A UUID the client makes once per task and reuses on every retry.
    pub idempotency_key: String,
    /// The task; the child sees only this.
    pub prompt: String,
    /// Agent that runs the child: codex, claude, cursor or opencode.
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// read_only, workspace or full_access; default and ceiling: the parent's.
    #[serde(default)]
    pub access_mode: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// One of the Project's folders, or a Git worktree of one (absolute path);
    /// default: the parent's folder.
    #[serde(default)]
    pub folder: Option<String>,
    /// Most children the parent may have, 1–64 (default: the host's
    /// `CUPOLA_DELEGATE_MAX_CHILDREN`, else 16).
    #[serde(default)]
    pub max_children: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadsReadParams {
    /// The thread's `conversationId`.
    pub thread_id: String,
    /// The snapshot's `previousCursor`, to read the page before it.
    #[serde(default)]
    pub before: Option<String>,
    /// 1–100; default 100.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadsSendParams {
    /// The thread's `conversationId`.
    pub thread_id: String,
    /// A UUID the client makes once per message and reuses on every retry of
    /// it. With the sender's identity it is the message's identity.
    pub idempotency_key: String,
    pub body: String,
    /// Files uploaded to this thread first (HTTP upload; no RPC method yet).
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    #[serde(default)]
    #[schemars(schema_with = "codex_skills_schema")]
    pub codex_skills: Vec<Value>,
    /// Send with this model; it may move a Project thread to the other provider.
    #[serde(default)]
    pub project_model: Option<ProjectModel>,
}

fn codex_skills_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "description": "Skills picked in the composer, as `codexSkillSelection`.",
        "type": "array",
        "maxItems": 8,
        "items": { "$ref": "#/$defs/codexSkillSelection" }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadsCreateParams {
    pub project_id: String,
    /// A UUID made once per new thread and reused on every retry; it is the
    /// first message's `idempotencyKey`.
    pub idempotency_key: String,
    /// The first message.
    pub body: String,
    pub family: ProviderId,
    /// A model `models.list` offers for `family`.
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// `workspace` (default) or another access the provider offers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_mode: Option<String>,
    /// Claude only: its approval mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_approval: Option<String>,
    #[serde(default)]
    pub plan_mode: bool,
    /// The Project folder to work in; default the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots_revision: Option<i64>,
    /// Compatibility field: only `default` (this computer's sign-in) is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotificationLevel {
    Default,
    Follow,
    Mute,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadNotificationsParams {
    pub thread_id: String,
    pub notification_level: NotificationLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadsMoveToWorktreeParams {
    /// The thread's `conversationId`.
    pub thread_id: String,
    /// The new branch's name; made from the thread's title when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadParams {
    /// The thread's `conversationId`.
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueEditParams {
    pub thread_id: String,
    /// The queued message's `id` from `threads.queue`.
    pub message_id: String,
    /// Its `revision` from `threads.queue`; a changed message is a conflict.
    pub expected_revision: i64,
    /// New text. Exactly one of `body`, `cancel` and `expectedTurnId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    /// Send it now into this running turn (the thread's last turn in
    /// `threads.read`); a turn that changed is a conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectModel {
    pub family: ProviderId,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalsResolveParams {
    pub approval_id: String,
    /// One of the approval's `availableDecisions`.
    pub decision: Value,
    pub action_nonce: String,
    /// Always `"pending"`.
    pub expected_state: String,
    pub idempotency_key: String,
    pub issued_at_ms: u64,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_json: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsSubscribeParams {
    /// Opt in only after initialize advertises `eventBatches`; older hosts
    /// reject unknown fields. Omission preserves legacy single-event frames.
    #[serde(default)]
    pub batch: bool,
    /// Replay every event after this cursor first. Omit to receive only new
    /// events.
    #[serde(default)]
    pub after: Option<EventCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventCursor {
    pub host_epoch: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsSubscribeResult {
    pub subscription_id: String,
    /// Where the stream starts: events after this cursor follow.
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsUnsubscribeParams {
    pub subscription_id: String,
}

/// `event` notification params.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventNotification {
    pub subscription_id: String,
    /// A `hostEventEnvelope`; save its `hostEpoch` and `sequence` as the cursor
    /// once it is applied.
    #[schemars(schema_with = "event_envelope_schema")]
    pub event: Value,
}

fn event_envelope_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "$ref": "#/$defs/hostEventEnvelope" })
}

/// `events.batch` notification params; incremental events in journal order.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventBatchNotification {
    pub subscription_id: String,
    #[schemars(schema_with = "event_envelopes_schema")]
    pub events: Vec<Value>,
}

fn event_envelopes_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "type": "array", "items": { "$ref": "#/$defs/hostEventEnvelope" } })
}

/// `events.ended` notification params.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsEnded {
    pub subscription_id: String,
    /// `resync`: the cursor cannot be replayed (new host epoch or expired
    /// window); read state again from `events.checkpoint`. Treat any other
    /// reason the same way.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ----- Routing -----

/// One HTTP call a method makes.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteCall {
    pub method: axum::http::Method,
    /// Path and query, segments percent-encoded.
    pub uri: String,
    pub body: Option<Value>,
}

/// Who is calling, for the params a route needs from the session.
#[derive(Debug, Clone)]
pub enum Caller {
    Owner,
    Device(String),
}

fn parse<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, RpcError> {
    let params = match params {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(params) => params,
    };
    serde_json::from_value(params).map_err(RpcError::invalid_params)
}

/// Percent-encodes one path segment (IDs come from clients; never trust them).
pub fn segment(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn query(pairs: &[(&str, Option<String>)]) -> String {
    let parts: Vec<String> = pairs
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_ref()
                .map(|value| format!("{key}={}", segment(value)))
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// The device a message is sent as: the session's, never a param.
fn sender(caller: &Caller) -> String {
    match caller {
        Caller::Owner => LOCAL_SENDER.to_owned(),
        Caller::Device(id) => id.clone(),
    }
}

/// The route call for a routed method, or `None` for a protocol method.
pub fn route(name: &str, params: Option<Value>, caller: &Caller) -> Result<RouteCall, RpcError> {
    use axum::http::Method as M;
    let call = |method: M, uri: String, body: Option<Value>| Ok(RouteCall { method, uri, body });
    match name {
        "auth.challenge" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/events/challenge".into(), None)
        }
        "host.status" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/host/status".into(), None)
        }
        "host.providers" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/host/providers".into(), None)
        }
        "media.orphans" => {
            let p: crate::media_refs::OrphanParams = parse(params)?;
            let query = p
                .older_than_days
                .map(|days| format!("?olderThanDays={days}"))
                .unwrap_or_default();
            call(M::GET, format!("/api/v1/host/media/orphans{query}"), None)
        }
        "media.prune" => {
            let p: crate::media_refs::OrphanParams = parse(params)?;
            call(
                M::POST,
                "/api/v1/host/media/orphans/prune".into(),
                Some(json!({ "olderThanDays": p.older_than_days })),
            )
        }
        "host.name" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/host/name".into(), None)
        }
        "host.rename" => {
            let p: HostRenameParams = parse(params)?;
            call(
                M::PUT,
                "/api/v1/host/name".into(),
                Some(json!({ "name": p.name })),
            )
        }
        "devices.list" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/devices".into(), None)
        }
        "devices.revoke" => {
            let p: DevicesRevokeParams = parse(params)?;
            call(
                M::POST,
                format!("/api/v1/devices/{}/revoke", segment(&p.device_id)),
                Some(serde_json::to_value(&p.signed).unwrap_or(Value::Null)),
            )
        }
        "devices.rename" => {
            let p: DeviceRenameParams = parse(params)?;
            call(
                M::PATCH,
                format!("/api/v1/devices/{}", segment(&p.device_id)),
                Some(json!({ "label": p.label })),
            )
        }
        "devices.forget" => {
            let p: DeviceParams = parse(params)?;
            call(
                M::POST,
                format!("/api/v1/devices/{}/forget", segment(&p.device_id)),
                None,
            )
        }
        "pairing.offer" => {
            let p: PairingOfferParams = parse(params)?;
            let body = p
                .route
                .is_some()
                .then(|| serde_json::to_value(&p).unwrap_or_default());
            call(M::POST, "/api/v1/pairing/offers".into(), body)
        }
        "pairing.cancel" => {
            let p: PairingCancelParams = parse(params)?;
            call(
                M::POST,
                format!("/api/v1/pairing/offers/{}/cancel", segment(&p.offer_id)),
                None,
            )
        }
        "pairing.pending" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/pairing/pending".into(), None)
        }
        "pairing.confirm" => {
            let p: PairingConfirmParams = parse(params)?;
            let body = serde_json::json!({"code": p.code});
            #[cfg(feature = "dev-pairing")]
            let body = {
                let mut body = body;
                body["devApprove"] = Value::Bool(p.dev_approve);
                body
            };
            call(
                M::POST,
                format!("/api/v1/pairing/pending/{}/confirm", segment(&p.device_id)),
                Some(body),
            )
        }
        "pairing.reject" => {
            let p: DeviceParams = parse(params)?;
            let action = name.trim_start_matches("pairing.");
            call(
                M::POST,
                format!("/api/v1/pairing/pending/{}/{action}", segment(&p.device_id)),
                None,
            )
        }
        "relay.status" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/relay".into(), None)
        }
        "relay.set" => {
            let p: RelaySetParams = parse(params)?;
            call(
                M::PUT,
                "/api/v1/relay/settings".into(),
                Some(json!({ "allowed": p.allowed })),
            )
        }
        "relay.enable" => {
            let p: RelayEnableParams = parse(params)?;
            let mut body = json!({ "grant": p.grant, "label": p.label });
            if let Some(signed) = p.signed {
                body["actionNonce"] = signed.action_nonce.into();
                body["issuedAtMs"] = signed.issued_at_ms.into();
                body["signature"] = signed.signature.into();
            }
            call(M::POST, "/api/v1/relay/enable".into(), Some(body))
        }
        "providers.list" => {
            let p: ProvidersListParams = parse(params)?;
            call(
                M::GET,
                format!(
                    "/api/v1/providers{}",
                    query(&[("refresh", p.refresh.then(|| "true".to_owned()))])
                ),
                None,
            )
        }
        "providers.reconnect" => {
            let p: ProviderParams = parse(params)?;
            call(
                M::POST,
                format!("/api/v1/host/providers/{}/reconnect", p.provider.as_str()),
                None,
            )
        }
        "update.work" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/host/update/work".into(), None)
        }
        "update.prepare" | "update.cancel" => {
            let p: UpdateParams = parse(params)?;
            let action = name.trim_start_matches("update.");
            call(
                M::POST,
                format!("/api/v1/host/update/{action}"),
                Some(serde_json::to_value(&p).unwrap_or(Value::Null)),
            )
        }
        "voice.status" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/voice".into(), None)
        }
        "voice.setKey" => {
            let p: VoiceSetKeyParams = parse(params)?;
            call(
                M::PUT,
                "/api/v1/voice/openai-key".into(),
                Some(json!({ "apiKey": p.api_key })),
            )
        }
        "voice.dictionary" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/voice/dictionary".into(), None)
        }
        "voice.updateDictionary" => {
            let p: VoiceDictionaryParams = parse(params)?;
            call(
                M::PUT,
                "/api/v1/voice/dictionary".into(),
                Some(json!({ "add": p.add, "remove": p.remove })),
            )
        }
        "voice.removeKey" => {
            parse::<Empty>(params)?;
            call(M::DELETE, "/api/v1/voice/openai-key".into(), None)
        }
        "projects.list" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/projects".into(), None)
        }
        "threads.list" => {
            let p: ThreadsListParams = parse(params)?;
            call(
                M::GET,
                format!(
                    "/api/v1/projects/{}/threads{}",
                    segment(&p.project_id),
                    query(&[
                        ("cursor", p.cursor),
                        ("limit", p.limit.map(|l| l.to_string()))
                    ])
                ),
                None,
            )
        }
        "models.list" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/agent-options".into(), None)
        }
        "threads.create" => {
            let p: ThreadsCreateParams = parse(params)?;
            // The sender is the session, as for `threads.send`.
            let mut body = json!({
                "deviceId": sender(caller),
                "clientMessageId": p.idempotency_key,
                "body": p.body,
                "family": p.family.as_str(),
                "model": p.model,
                "planMode": p.plan_mode,
            });
            for (key, value) in [
                ("effort", p.effort),
                ("serviceTier", p.service_tier),
                ("accessMode", p.access_mode),
                ("claudeApproval", p.claude_approval),
                ("folderId", p.folder_id),
                ("accountId", p.account_id),
            ] {
                if let Some(value) = value {
                    body[key] = value.into();
                }
            }
            if let Some(revision) = p.roots_revision {
                body["rootsRevision"] = revision.into();
            }
            call(
                M::POST,
                format!("/api/v1/projects/{}/threads", segment(&p.project_id)),
                Some(body),
            )
        }
        "threads.notifications" => {
            let p: ThreadParams = parse(params)?;
            call(
                M::GET,
                format!(
                    "/api/v1/conversations/{}/notifications",
                    segment(&p.thread_id)
                ),
                None,
            )
        }
        "threads.moveToWorktree" => {
            let p: ThreadsMoveToWorktreeParams = parse(params)?;
            call(
                M::POST,
                format!(
                    "/api/v1/project-conversations/{}/worktree",
                    segment(&p.thread_id)
                ),
                Some(json!({"branch": p.branch})),
            )
        }
        "threads.setNotifications" => {
            let p: ThreadNotificationsParams = parse(params)?;
            call(
                M::PUT,
                format!(
                    "/api/v1/conversations/{}/notifications",
                    segment(&p.thread_id)
                ),
                Some(json!({"notificationLevel": p.notification_level})),
            )
        }
        "threads.queue" => {
            let p: ThreadParams = parse(params)?;
            call(
                M::GET,
                format!("/api/v1/conversations/{}/queue", segment(&p.thread_id)),
                None,
            )
        }
        "queue.edit" => {
            let p: QueueEditParams = parse(params)?;
            let mut body = json!({ "expectedRevision": p.expected_revision });
            if let Some(text) = p.body {
                body["body"] = text.into();
            }
            if let Some(cancel) = p.cancel {
                body["cancel"] = cancel.into();
            }
            if let Some(turn) = p.expected_turn_id {
                body["expectedTurnId"] = turn.into();
            }
            call(
                M::POST,
                format!(
                    "/api/v1/conversations/{}/queue/{}",
                    segment(&p.thread_id),
                    segment(&p.message_id)
                ),
                Some(body),
            )
        }
        "threads.attach" => {
            let p: ThreadsAttachParams = parse(params)?;
            call(
                M::POST,
                format!("/api/v1/projects/{}/threads/attach", segment(&p.project_id)),
                Some(json!({ "reference": p.reference })),
            )
        }
        "threads.delegate" => {
            let p: ThreadsDelegateParams = parse(params)?;
            let mut body = json!({
                "clientRequestId": p.idempotency_key,
                "prompt": p.prompt,
                "provider": p.provider,
            });
            for (key, value) in [
                ("model", p.model),
                ("effort", p.effort),
                ("accessMode", p.access_mode),
                ("title", p.title),
                ("folder", p.folder),
            ] {
                if let Some(value) = value {
                    body[key] = value.into();
                }
            }
            if let Some(cap) = p.max_children {
                body["maxChildren"] = cap.into();
            }
            call(
                M::POST,
                format!(
                    "/api/v1/conversations/{}/delegations",
                    segment(&p.thread_id)
                ),
                Some(body),
            )
        }
        "threads.read" => {
            let p: ThreadsReadParams = parse(params)?;
            call(
                M::GET,
                format!(
                    "/api/v1/conversations/{}{}",
                    segment(&p.thread_id),
                    query(&[
                        ("before", p.before),
                        ("limit", p.limit.map(|l| l.to_string()))
                    ])
                ),
                None,
            )
        }
        "threads.send" => {
            let p: ThreadsSendParams = parse(params)?;
            // The sender is the session, never a param: a device cannot send
            // as another, and the owner sends as the desktop identity.
            let mut body = json!({
                "deviceId": sender(caller),
                "clientMessageId": p.idempotency_key,
                "body": p.body,
                "attachmentIds": p.attachment_ids,
                "codexSkills": p.codex_skills,
            });
            if let Some(model) = p.project_model {
                body["projectModel"] = serde_json::to_value(model).unwrap_or(Value::Null);
            }
            call(
                M::POST,
                format!("/api/v1/conversations/{}/messages", segment(&p.thread_id)),
                Some(body),
            )
        }
        "approvals.list" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/approvals".into(), None)
        }
        "approvals.resolve" => {
            let p: ApprovalsResolveParams = parse(params)?;
            let mut body = json!({
                "decision": p.decision,
                "actionNonce": p.action_nonce,
                "expectedState": p.expected_state,
                "idempotencyKey": p.idempotency_key,
                "issuedAtMs": p.issued_at_ms,
                "signature": p.signature,
            });
            if let Some(response) = p.response_json {
                body["responseJson"] = response.into();
            }
            call(
                M::POST,
                format!("/api/v1/approvals/{}/resolve", segment(&p.approval_id)),
                Some(body),
            )
        }
        "events.checkpoint" => {
            parse::<Empty>(params)?;
            call(M::GET, "/api/v1/sync/checkpoint".into(), None)
        }
        _ => Err(RpcError::new(
            codes::METHOD_NOT_FOUND,
            format!("Unknown method `{name}`."),
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

// ----- Schema -----

const HTTP_AUTHORITY: &str =
    include_str!("../../../../packages/protocol/schemas/wonder-http-v1.json");
const WEBSOCKET_AUTHORITY: &str =
    include_str!("../../../../packages/protocol/schemas/wonder-websocket-v1.json");

/// The checked-in schema's path, relative to the repository root.
pub const SCHEMA_PATH: &str = "tests/contracts/rpc/wonder-rpc-v1.schema.json";

fn http_result(name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{name}") })
}

/// The protocol's JSON Schema, generated from the types above. The HTTP
/// definitions a method returns are copied in from the protocol authority,
/// so the file stands alone.
pub fn schema() -> Value {
    let mut generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    let mut methods = Map::new();
    for method in METHODS {
        let params = (method.params)(&mut generator).to_value();
        let result = match method.result {
            ResultShape::Http(name) => http_result(name),
            ResultShape::HttpArray(name) => json!({ "type": "array", "items": http_result(name) }),
            ResultShape::Typed(schema) => schema(&mut generator).to_value(),
            ResultShape::Empty => json!({
                "description": "Always `{}`.",
                "type": "object",
                "additionalProperties": false
            }),
            ResultShape::Object => json!({
                "description": "The route's JSON body; see the guide.",
                "type": ["object", "array"]
            }),
        };
        let mut entry = json!({
            "summary": method.summary.split_whitespace().collect::<Vec<_>>().join(" "),
            "authority": method.authority,
            "params": params,
            "result": result,
        });
        if let Some(route) = method.route {
            entry["route"] = route.into();
        }
        if method.before_initialize {
            entry["beforeInitialize"] = true.into();
        }
        methods.insert(method.name.to_owned(), entry);
    }
    let mut notifications = Map::new();
    notifications.insert(
        "event".into(),
        json!({
            "summary": NOTIFICATIONS[0].1,
            "params": schema_of::<EventNotification>(&mut generator).to_value(),
        }),
    );
    notifications.insert(
        "events.ended".into(),
        json!({
            "summary": NOTIFICATIONS[1].1,
            "params": schema_of::<EventsEnded>(&mut generator).to_value(),
        }),
    );
    notifications.insert(
        "events.batch".into(),
        json!({
            "summary": NOTIFICATIONS[2].1,
            "params": schema_of::<EventBatchNotification>(&mut generator).to_value(),
        }),
    );
    let envelope = json!({
        "request": schema_of::<Request>(&mut generator).to_value(),
        "response": schema_of::<Response>(&mut generator).to_value(),
        "notification": schema_of::<Notification>(&mut generator).to_value(),
    });
    let mut defs: Map<String, Value> = generator.take_definitions(true).into_iter().collect();

    // Copy in the HTTP and WebSocket definitions the methods reference.
    let http: Value = serde_json::from_str(HTTP_AUTHORITY).expect("HTTP authority is JSON");
    let http_defs = http["$defs"].as_object().cloned().unwrap_or_default();
    let mut websocket: Value =
        serde_json::from_str(WEBSOCKET_AUTHORITY).expect("WebSocket authority is JSON");
    if let Some(object) = websocket.as_object_mut() {
        for key in ["$schema", "$id", "title"] {
            object.remove(key);
        }
    }
    defs.insert("hostEventEnvelope".into(), websocket);
    let mut pending: Vec<String> = Vec::new();
    let body = json!([methods, notifications, envelope, defs]);
    collect_refs(&body, &mut pending);
    while let Some(name) = pending.pop() {
        if defs.contains_key(&name) {
            continue;
        }
        let Some(definition) = http_defs.get(&name) else {
            panic!("RPC schema references #/$defs/{name}, which no authority defines");
        };
        collect_refs(definition, &mut pending);
        defs.insert(name, definition.clone());
    }
    let defs: Map<String, Value> = {
        let mut sorted: Vec<_> = defs.into_iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        sorted.into_iter().collect()
    };
    let errors: Map<String, Value> = [
        ("parseError", codes::PARSE_ERROR),
        ("invalidRequest", codes::INVALID_REQUEST),
        ("methodNotFound", codes::METHOD_NOT_FOUND),
        ("invalidParams", codes::INVALID_PARAMS),
        ("internalError", codes::INTERNAL_ERROR),
        ("notInitialized", codes::NOT_INITIALIZED),
        ("alreadyInitialized", codes::ALREADY_INITIALIZED),
        ("busy", codes::BUSY),
        ("unsupportedVersion", codes::UNSUPPORTED_VERSION),
    ]
    .into_iter()
    .map(|(name, code)| (name.to_owned(), json!(code)))
    .collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://wonder.app/schemas/wonder-rpc-v1.json",
        "title": "Cupola RPC",
        "description": "Generated by `cupola rpc --schema` from crates/wonderd/src/rpc. Do not edit; see crates/wonderd/RPC.md.",
        "protocolVersion": PROTOCOL_VERSION,
        "envelope": envelope,
        "errors": errors,
        "methods": methods,
        "notifications": notifications,
        "$defs": defs,
    })
}

fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                if let Some(name) = reference.strip_prefix("#/$defs/") {
                    out.push(name.to_owned());
                }
            }
            map.values().for_each(|value| collect_refs(value, out));
        }
        Value::Array(items) => items.iter().for_each(|value| collect_refs(value, out)),
        _ => {}
    }
}

/// The schema as checked in: pretty JSON with a trailing newline.
pub fn schema_text() -> String {
    let mut text = serde_json::to_string_pretty(&schema()).expect("schema serializes");
    text.push('\n');
    text
}

// Built on the crate test fixtures, which are Unix-only (see lib.rs).
#[cfg(all(test, unix))]
pub(crate) mod tests;
