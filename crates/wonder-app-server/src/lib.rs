//! Narrow typed boundary for the supported Codex App Server JSONL protocol.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[cfg(windows)]
pub mod job;
mod process;
pub mod provider_environment;
mod schema_compat;

#[cfg(feature = "test-support")]
pub use process::fixture_runtime;
pub use process::verify_runtime;
pub use process::{
    AppServerClient, BridgeLaunchConfig, LaunchConfig, NotificationSink, RpcClient, RuntimeError,
    RuntimeHealth,
};

pub const CODEX_VERSION: &str = "codex-cli 0.155.0-alpha.9";
/// The oldest and newest Codex releases Cupola was tested against. A release
/// outside this range is not refused: it runs when its generated protocol
/// still carries the contracts Cupola uses, and Cupola says it is untested.
/// Releases with a pinned schema hash below keep a fast exact-hash path.
pub const TESTED_CODEX_RANGE: (&str, &str) = (CODEX_VERSION, "codex-cli 0.166.0-alpha.6");

/// Where a Codex version sits against [`TESTED_CODEX_RANGE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexTesting {
    Tested,
    /// Newer than any release Cupola was tested with.
    Newer,
    /// Older than any release Cupola was tested with.
    Older,
    /// Not a version Cupola can compare.
    Unknown,
}

pub fn codex_testing(version: &str) -> CodexTesting {
    use std::cmp::Ordering::{Greater, Less};
    if compare_codex_versions(version, TESTED_CODEX_RANGE.1) == Some(Greater) {
        CodexTesting::Newer
    } else if compare_codex_versions(version, TESTED_CODEX_RANGE.0) == Some(Less) {
        CodexTesting::Older
    } else if compare_codex_versions(version, TESTED_CODEX_RANGE.0).is_some() {
        CodexTesting::Tested
    } else {
        CodexTesting::Unknown
    }
}

/// What a person calls a Codex feature whose protocol moved, for the methods
/// that are optional. Methods without a name here are summed up generally.
fn feature_label(method: &str) -> Option<&'static str> {
    Some(match method {
        "mcpServer/tool/call" => "MCP tool calls",
        "thread/loaded/list" | "thread/backgroundTerminals/list" => "Background terminals",
        "skills/extraRoots/set" | "skills/changed" => "Cupola skills",
        "account/login/start"
        | "account/login/cancel"
        | "account/logout"
        | "account/login/completed" => "Signing in another account",
        "thread/search" | "thread/searchOccurrences" => "Message search",
        "thread/queue/add" | "thread/queue/list" => "Queued messages",
        "review/start" => "Reviews",
        "thread/compact/start" | "thread/compacted" => "Compacting a chat",
        "thread/fork" | "thread/revert" | "thread/rewind" => "Editing earlier messages",
        "item/plan/delta" | "turn/plan/updated" => "Plans",
        "turn/diff/updated" | "item/fileChange/patchUpdated" | "item/fileChange/outputDelta" => {
            "Live file changes"
        }
        "item/commandExecution/outputDelta" => "Live command output",
        "item/reasoning/summaryPartAdded"
        | "item/reasoning/summaryTextDelta"
        | "item/reasoning/textDelta" => "Thinking summaries",
        "thread/tokenUsage/updated" => "Context usage",
        "item/mcpToolCall/progress" => "MCP tool progress",
        _ => return None,
    })
}

/// Features switched off for a verified binary because Codex changed the
/// optional protocol they use; chats keep working without them.
static DEGRADED_FEATURES: std::sync::Mutex<Vec<(std::path::PathBuf, Vec<String>)>> =
    std::sync::Mutex::new(Vec::new());

pub(crate) fn record_degraded(binary: &std::path::Path, methods: &[&'static str]) {
    let mut labels: Vec<String> = methods
        .iter()
        .map(|method| {
            feature_label(method)
                .unwrap_or("Some optional features")
                .to_owned()
        })
        .collect();
    labels.sort();
    labels.dedup();
    let mut known = DEGRADED_FEATURES.lock().unwrap_or_else(|e| e.into_inner());
    known.retain(|(path, _)| path != binary);
    known.push((binary.to_owned(), labels));
}

/// The names of the features this Codex binary cannot serve after its last
/// verification, in plain words; empty when everything Cupola uses is intact
/// or the binary was not verified yet.
pub fn degraded_features(binary: &std::path::Path) -> Vec<String> {
    let binary = std::fs::canonicalize(binary).unwrap_or_else(|_| binary.to_owned());
    DEGRADED_FEATURES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(path, _)| *path == binary)
        .map(|(_, labels)| labels.clone())
        .unwrap_or_default()
}
pub const STABLE_SCHEMA_SHA256: &str =
    "5a4d50ed04afa9cd1b383d011f67ec055960a35ca7fdeab222ed65e70fca8f0b";
pub const EXPERIMENTAL_SCHEMA_SHA256: &str =
    "48368bf71d00498557245665dd4581f9b0d5381561a2c8ba1ef4eb26ea4a1632";
pub const ALPHA16_STABLE_SCHEMA_SHA256: &str =
    "6ca35a4c82645df5cba7b2bba1845d3167c12294317daf1923da1a2e8fa04738";
pub const ALPHA16_EXPERIMENTAL_SCHEMA_SHA256: &str =
    "c7291ce922a7e7f5fe5e36902544bbc18b793d6f5a998ef36aeff0b195b0e91d";

pub const V162_STABLE_SCHEMA_SHA256: &str =
    "0c34fd753d5e4498c32db50dbb20e9c142f81cb3f4b186c581d756d6b7e10bac";
pub const V162_EXPERIMENTAL_SCHEMA_SHA256: &str =
    "9be20dabc0516c68cbe7ad4480affd2953c43ad0e7770a3949663967c00f5ca5";
pub const V162_17_STABLE_SCHEMA_SHA256: &str =
    "6f622f94329e648ffcee48be34b78a35da19e55598fd71241dbb08508195c46d";
pub const V162_17_EXPERIMENTAL_SCHEMA_SHA256: &str =
    "bff504aada804072a51f42c94478375c46273cf30195069ec4514f56a913663c";

pub const V166_STABLE_SCHEMA_SHA256: &str =
    "e025e54fb11354eb82767ea040b40efa17b34dfc706c9433a6115b5f4771c465";
pub const V166_EXPERIMENTAL_SCHEMA_SHA256: &str =
    "f72bb03a3f519cd19b37a030b42453128ff5b88abaad8c1264cf1ca197dbbed0";

pub fn expected_schema_hashes(version: &str) -> Option<(&'static str, &'static str)> {
    match version {
        CODEX_VERSION | "codex-cli 0.155.0-alpha.9.2" => {
            Some((STABLE_SCHEMA_SHA256, EXPERIMENTAL_SCHEMA_SHA256))
        }
        "codex-cli 0.155.0-alpha.16.3" | "codex-cli 0.155.0-alpha.16.4" => Some((
            ALPHA16_STABLE_SCHEMA_SHA256,
            ALPHA16_EXPERIMENTAL_SCHEMA_SHA256,
        )),
        "codex-cli 0.162.0-alpha.2" => {
            Some((V162_STABLE_SCHEMA_SHA256, V162_EXPERIMENTAL_SCHEMA_SHA256))
        }
        "codex-cli 0.162.0-alpha.17.2" => Some((
            V162_17_STABLE_SCHEMA_SHA256,
            V162_17_EXPERIMENTAL_SCHEMA_SHA256,
        )),
        "codex-cli 0.166.0-alpha.6" => {
            Some((V166_STABLE_SCHEMA_SHA256, V166_EXPERIMENTAL_SCHEMA_SHA256))
        }
        _ => None,
    }
}

pub const ALLOWED_SERVER_REQUESTS: [&str; 6] = [
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
    "item/tool/requestUserInput",
    "mcpServer/elicitation/request",
    "item/tool/call",
];

/// Optional methods; not required runtime features. Native-MCP lifecycle
/// cleanup, Wonder's skill folder (only while the owner has one of its skills
/// on), and the device-code sign-in and sign-out of an additional account (a
/// separate Codex home Wonder owns; never the Mac's own), and message search
/// across and within the Mac's threads (reads only; an older runtime answers
/// unknown method and search covers Wonder's own messages).
pub const OPTIONAL_METHODS: [&str; 9] = [
    "mcpServer/tool/call",
    "thread/loaded/list",
    "thread/backgroundTerminals/list",
    // Wonder's skill folder, only while the owner has one of its skills on.
    "skills/extraRoots/set",
    "account/login/start",
    "account/login/cancel",
    "account/logout",
    "thread/search",
    "thread/searchOccurrences",
];

pub fn is_allowed_method(method: &str) -> bool {
    OPTIONAL_METHODS.contains(&method)
        || RequiredMethod::ALL
            .iter()
            .any(|candidate| candidate.as_str() == method)
}

/// Every Codex method Wonder can send, with whether the runtime must offer it.
/// `RpcClient::request` refuses anything else, so this is the complete set of
/// request contracts startup verification has to protect.
pub fn sent_methods() -> impl Iterator<Item = (&'static str, bool)> {
    RequiredMethod::ALL
        .iter()
        .map(|method| (method.as_str(), true))
        .chain(OPTIONAL_METHODS.iter().map(|method| (*method, false)))
        .chain(PROJECT_METHODS.iter().map(|method| (*method, false)))
}

/// Codex notifications wonderd reads, with whether Wonder stops working
/// without them. wonderd's `codex_notification_inventory_is_complete` test
/// fails when its source starts reading a notification missing here.
pub const CONSUMED_NOTIFICATIONS: [(&str, bool); 25] = [
    // Ends an added account's device-code sign-in.
    ("account/login/completed", false),
    ("turn/started", true),
    ("turn/completed", true),
    ("item/started", true),
    ("item/completed", true),
    ("item/agentMessage/delta", true),
    ("serverRequest/resolved", true),
    ("item/commandExecution/outputDelta", false),
    ("item/fileChange/outputDelta", false),
    ("item/fileChange/patchUpdated", false),
    ("item/mcpToolCall/progress", false),
    ("item/plan/delta", false),
    ("item/reasoning/summaryPartAdded", false),
    ("item/reasoning/summaryTextDelta", false),
    ("item/reasoning/textDelta", false),
    ("model/rerouted", false),
    ("skills/changed", false),
    // Correlated by the opt-in SSH diagnostics probe; not required in production.
    ("thread/started", false),
    ("thread/compacted", false),
    ("thread/goal/cleared", false),
    ("thread/goal/updated", false),
    ("thread/status/changed", false),
    ("thread/tokenUsage/updated", false),
    ("turn/diff/updated", false),
    ("turn/plan/updated", false),
];

/// Native project metadata, allowed only on the normal-home Projects client.
/// Older runtimes without these methods remain compatible; callers treat an
/// unknown-method response as an unsupported capability.
pub const PROJECT_METHODS: [&str; 25] = [
    // Pinned experimental native queue; projects gates these before sending.
    "thread/queue/add",
    "thread/queue/list",
    "project/list",
    "project/read",
    "project/create",
    "thread/metadata/update",
    "thread/archive",
    // Copies a project thread, through a chosen turn, into a new thread.
    "thread/fork",
    // Edit from here: Codex replaces the thread's durable history with the
    // turns before one (files are not reverted); the bridges' own rewind.
    "thread/revert",
    "thread/rewind",
    // Wonder's Claude bridge: native Claude Code session metadata and attach.
    "project/sessions/list",
    "project/session/attach",
    "project/session/fork",
    "project/folders/list",
    // What a Project folder's own Claude configuration would run, for the
    // owner to trust before it loads. A read: it starts no Claude process.
    "project/configuration/read",
    // Claude agent tasks and background commands of one session: read them, and
    // stop a running one while Wonder drives the session.
    "thread/backgroundTasks/list",
    "thread/backgroundTask/read",
    "thread/backgroundTask/stop",
    // Stops one command Codex left running in a verified Project thread.
    "thread/backgroundTerminals/terminate",
    // The settings Claude on the Mac last ran a session with, so the composer
    // starts from them.
    "thread/nativeSettings",
    // The live session's native slash-command catalog the bridge retained,
    // for the composer's `/` list. A read: it starts no session.
    "thread/commands/list",
    "thread/compact/start",
    "review/start",
    // Cupola's Claude runtime: its SDK update status, and checking for a
    // compatible patch now. Neither starts model work or interrupts a turn.
    "runtime/update/read",
    "runtime/update/check",
];

/// The first Codex measured running the native queue Cupola uses. ChatGPT's
/// bundled alpha builds and the released CLI (Homebrew, npm) move on their
/// own schedule, so later versions qualify by contract, not by name: their
/// generated schema must still carry the queue methods, parameters and
/// receipts Cupola relies on ([`native_queue_contract`]). Older versions and
/// any other shape fail closed without making the rest of the runtime
/// incompatible.
pub const NATIVE_QUEUE_MIN_VERSION: &str = "codex-cli 0.163.0-alpha.10.2";

/// One installed binary: its path, size, modification time and (on Unix)
/// file. Output it generated is reused only for the same identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BinaryIdentity {
    path: std::path::PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    file: Option<(u64, u64)>,
}

pub(crate) async fn binary_identity(binary: &std::path::Path) -> Option<BinaryIdentity> {
    let path = tokio::fs::canonicalize(binary).await.ok()?;
    let metadata = tokio::fs::metadata(&path).await.ok()?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    };
    #[cfg(not(unix))]
    let file = None;
    Some(BinaryIdentity {
        path,
        len: metadata.len(),
        modified: metadata.modified().ok(),
        file,
    })
}

/// The experimental schema the native-queue check just generated. The
/// runtime verification that follows it in the same start uses it instead of
/// generating it a second time; it is taken once and only shortly after.
static GENERATED_EXPERIMENTAL: std::sync::Mutex<
    Option<(BinaryIdentity, std::time::Instant, Vec<u8>)>,
> = std::sync::Mutex::new(None);

pub(crate) fn take_generated_experimental(identity: &BinaryIdentity) -> Option<Vec<u8>> {
    let mut generated = GENERATED_EXPERIMENTAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    generated
        .take_if(|(generated_by, at, _)| {
            generated_by == identity && at.elapsed() < std::time::Duration::from_secs(60)
        })
        .map(|(_, _, bytes)| bytes)
}

/// Serializes tests that leave a generated schema for verification.
#[cfg(test)]
pub(crate) static GENERATED_SCHEMA_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn supports_native_thread_queue(binary: &std::path::Path) -> bool {
    use std::time::Duration;
    use tokio::{process::Command, time::timeout};
    let probe = async {
        let version = Command::new(binary)
            .arg("--version")
            .kill_on_drop(true)
            .output()
            .await
            .ok()?;
        if !version.status.success()
            || !native_queue_version(String::from_utf8_lossy(&version.stdout).trim())
        {
            return None;
        }
        let identity = binary_identity(binary).await;
        let directory = tempfile::tempdir().ok()?;
        let generated = Command::new(binary)
            .args([
                "app-server",
                "generate-json-schema",
                "--experimental",
                "--out",
            ])
            .arg(directory.path())
            .kill_on_drop(true)
            .output()
            .await
            .ok()?;
        if !generated.status.success() {
            return None;
        }
        let bytes = tokio::fs::read(
            directory
                .path()
                .join("codex_app_server_protocol.v2.schemas.json"),
        )
        .await
        .ok()?;
        let supported = native_queue_contract(&serde_json::from_slice(&bytes).ok()?);
        // Unchanged while it generated: verification may reuse its output.
        if let Some(identity) = identity {
            if binary_identity(binary).await.as_ref() == Some(&identity) {
                *GENERATED_EXPERIMENTAL
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) =
                    Some((identity, std::time::Instant::now(), bytes));
            }
        }
        supported.then_some(())
    };
    timeout(Duration::from_secs(20), probe)
        .await
        .ok()
        .flatten()
        .is_some()
}

/// Whether `codex --version` output names [`NATIVE_QUEUE_MIN_VERSION`] or a
/// later release, by semantic-version precedence (0.163.0-alpha.10.2 <
/// 0.163.0-alpha.11 < 0.163.0 < 0.164.0).
pub fn native_queue_version(output: &str) -> bool {
    match (
        codex_version(output),
        codex_version(NATIVE_QUEUE_MIN_VERSION),
    ) {
        (Some(version), Some(minimum)) => version >= minimum,
        _ => false,
    }
}

/// Semantic-version order of two Codex versions, written with or without
/// the `codex-cli ` prefix `--version` prints. None when either is not a
/// version.
pub fn compare_codex_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let parse = |text: &str| {
        let text = text.trim();
        codex_version(&if text.starts_with("codex-cli ") {
            text.to_owned()
        } else {
            format!("codex-cli {text}")
        })
    };
    Some(parse(left)?.cmp(&parse(right)?))
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    Number(u64),
    Text(String),
}

/// `codex-cli X.Y.Z[-pre]` as comparable parts. A release sorts after its
/// pre-releases, which `(core, is_release, identifiers)` encodes.
fn codex_version(output: &str) -> Option<([u64; 3], bool, Vec<Identifier>)> {
    let text = output.trim().strip_prefix("codex-cli ")?;
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let core = [parts.next()??, parts.next()??, parts.next()??];
    if parts.next().is_some() {
        return None;
    }
    let identifiers = match pre {
        None => Vec::new(),
        Some(pre) => pre
            .split('.')
            .map(|id| {
                if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
                    None
                } else {
                    Some(
                        id.parse()
                            .map(Identifier::Number)
                            .unwrap_or_else(|_| Identifier::Text(id.to_owned())),
                    )
                }
            })
            .collect::<Option<_>>()?,
    };
    Some((core, pre.is_none(), identifiers))
}

/// The parts of Codex's native queue Cupola uses (`thread/queue/add` and
/// `thread/queue/list`), checked in a generated experimental v2 schema:
/// both methods exist; add takes the thread, Cupola's message id and the same
/// input turn/start takes, and requires nothing else; its receipt and list
/// entries carry the submission id and Cupola's message id; the list pages by
/// cursor.
pub fn native_queue_contract(schema: &Value) -> bool {
    let definitions = &schema["definitions"];
    let definition = |value: &Value| -> Option<&Value> {
        let name = value["$ref"].as_str()?.strip_prefix("#/definitions/")?;
        definitions.get(name)
    };
    let method = |name: &str| -> Option<&Value> {
        definitions["ClientRequest"]["oneOf"]
            .as_array()?
            .iter()
            .find(|request| {
                request["properties"]["method"]["enum"]
                    .as_array()
                    .is_some_and(|names| names.len() == 1 && names[0] == name)
            })
            .and_then(|request| definition(&request["properties"]["params"]))
    };
    let string = |value: &Value| {
        value["type"] == "string"
            || value["type"]
                .as_array()
                .is_some_and(|types| types.iter().any(|t| t == "string"))
    };
    let accepts = |params: &Value, sent: &[&str]| {
        sent.iter()
            .all(|field| params["properties"].get(*field).is_some())
            && params["required"].as_array().is_none_or(|required| {
                required
                    .iter()
                    .all(|field| field.as_str().is_some_and(|f| sent.contains(&f)))
            })
    };
    let requires = |value: &Value, fields: &[&str]| {
        value["required"].as_array().is_some_and(|required| {
            fields
                .iter()
                .all(|field| required.iter().any(|r| r == *field))
        })
    };
    let check = || -> Option<bool> {
        let add = method("thread/queue/add")?;
        let list = method("thread/queue/list")?;
        let turn_input = &method("turn/start")?["properties"]["input"];
        let submission = |value: &Value| {
            definition(value).is_some_and(|entry| {
                requires(entry, &["id", "clientUserMessageId"])
                    && string(&entry["properties"]["id"])
                    && string(&entry["properties"]["clientUserMessageId"])
            })
        };
        let added = definitions.get("ThreadQueueAddResponse")?;
        let listed = definitions.get("ThreadQueueListResponse")?;
        Some(
            accepts(add, &["threadId", "clientUserMessageId", "input"])
                && string(&add["properties"]["threadId"])
                && string(&add["properties"]["clientUserMessageId"])
                && add["properties"]["input"] == *turn_input
                && accepts(list, &["threadId", "cursor", "limit"])
                && requires(added, &["queuedSubmission"])
                && submission(&added["properties"]["queuedSubmission"])
                && requires(listed, &["data"])
                && listed["properties"]["data"]["type"] == "array"
                && submission(&listed["properties"]["data"]["items"])
                && string(&listed["properties"]["nextCursor"]),
        )
    };
    check().unwrap_or(false)
}

pub fn is_project_method(method: &str) -> bool {
    PROJECT_METHODS.contains(&method)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RpcRequest {
    pub method: String,
    pub id: Option<u64>,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RpcResponse {
    pub id: u64,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredMethod {
    Initialize,
    AccountRead,
    ModelList,
    RateLimitsRead,
    AppList,
    AppsInstalled,
    AppsRead,
    ConfigRead,
    SkillsList,
    McpServerStatusList,
    ConfigRequirementsRead,
    ThreadStart,
    ThreadResume,
    ThreadRead,
    ThreadList,
    ThreadUnarchive,
    ThreadTurnsList,
    ThreadItemsList,
    ThreadSettingsUpdate,
    ThreadGoalSet,
    ThreadGoalGet,
    ThreadGoalClear,
    TurnStart,
    TurnSteer,
    TurnInterrupt,
}

impl RequiredMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::AccountRead => "account/read",
            Self::ModelList => "model/list",
            Self::RateLimitsRead => "account/rateLimits/read",
            Self::AppList => "app/list",
            Self::AppsInstalled => "app/installed",
            Self::AppsRead => "app/read",
            Self::ConfigRead => "config/read",
            Self::SkillsList => "skills/list",
            Self::McpServerStatusList => "mcpServerStatus/list",
            Self::ConfigRequirementsRead => "configRequirements/read",
            Self::ThreadStart => "thread/start",
            Self::ThreadResume => "thread/resume",
            Self::ThreadRead => "thread/read",
            Self::ThreadList => "thread/list",
            Self::ThreadUnarchive => "thread/unarchive",
            Self::ThreadTurnsList => "thread/turns/list",
            Self::ThreadItemsList => "thread/items/list",
            Self::ThreadSettingsUpdate => "thread/settings/update",
            Self::ThreadGoalSet => "thread/goal/set",
            Self::ThreadGoalGet => "thread/goal/get",
            Self::ThreadGoalClear => "thread/goal/clear",
            Self::TurnStart => "turn/start",
            Self::TurnSteer => "turn/steer",
            Self::TurnInterrupt => "turn/interrupt",
        }
    }

    pub const ALL: [Self; 25] = [
        Self::Initialize,
        Self::AccountRead,
        Self::ModelList,
        Self::RateLimitsRead,
        Self::AppList,
        Self::AppsInstalled,
        Self::AppsRead,
        Self::ConfigRead,
        Self::SkillsList,
        Self::McpServerStatusList,
        Self::ConfigRequirementsRead,
        Self::ThreadStart,
        Self::ThreadResume,
        Self::ThreadRead,
        Self::ThreadList,
        Self::ThreadUnarchive,
        Self::ThreadTurnsList,
        Self::ThreadItemsList,
        Self::ThreadSettingsUpdate,
        Self::ThreadGoalSet,
        Self::ThreadGoalGet,
        Self::ThreadGoalClear,
        Self::TurnStart,
        Self::TurnSteer,
        Self::TurnInterrupt,
    ];
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializeRequest {
    pub id: u64,
    pub request: RpcRequest,
}

pub fn initialize_request(wonder_version: &str) -> InitializeRequest {
    InitializeRequest {
        id: 1,
        request: RpcRequest {
            method: "initialize".into(),
            id: Some(1),
            params: json!({
                "clientInfo": {
                    "name": "wonder",
                    "title": "Cupola",
                    "version": wonder_version
                },
                "capabilities": {
                    "experimentalApi": true,
                    "mcpServerOpenaiFormElicitation": true,
                    "requestAttestation": false,
                    "optOutNotificationMethods": []
                }
            }),
        },
    }
}

pub fn require_experimental_api(result: &Value) -> Result<(), &'static str> {
    // `experimentalApi` is a client-declared initialize capability in the
    // 0.153.0-alpha.5 schema. The live server acknowledges initialize with server
    // metadata and does not echo that client capability in its result.
    match result
        .get("capabilities")
        .and_then(Value::as_object)
        .and_then(|caps| caps.get("experimentalApi"))
    {
        None | Some(Value::Bool(true)) => Ok(()),
        Some(_) => Err("Codex runtime incompatible: experimentalApi was declined"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Runtime admission must leave older Codex usable while disabling the
    // non-idempotent enqueue when its version or experimental schema differs.
    #[cfg(unix)]
    #[tokio::test]
    async fn native_queue_gate_needs_a_later_version_and_the_queue_contract() {
        use std::os::unix::fs::PermissionsExt;
        let _generated = GENERATED_SCHEMA_TEST.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("codex");
        let research = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../research/codex-app-server");
        let with_queue =
            research.join("0.163.0/experimental/codex_app_server_protocol.v2.schemas.json");
        let mut stripped: Value =
            serde_json::from_slice(&std::fs::read(&with_queue).unwrap()).unwrap();
        stripped["definitions"]["ClientRequest"]["oneOf"]
            .as_array_mut()
            .unwrap()
            .retain(|r| r["properties"]["method"]["enum"][0] != "thread/queue/add");
        let without_queue = dir.path().join("without-queue.json");
        std::fs::write(&without_queue, stripped.to_string()).unwrap();
        for (version, schema, supported) in [
            (CODEX_VERSION, &with_queue, false),
            ("codex-cli 0.163.0", &without_queue, false),
            (NATIVE_QUEUE_MIN_VERSION, &with_queue, true),
            ("codex-cli 0.163.0-alpha.11", &with_queue, true),
            ("codex-cli 0.163.0", &with_queue, true),
        ] {
            std::fs::write(&binary, format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\nlast=''; for arg in \"$@\"; do last=\"$arg\"; done\ncp '{}' \"$last/codex_app_server_protocol.v2.schemas.json\"\n", schema.display()
            )).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                supports_native_thread_queue(&binary).await,
                supported,
                "{version} {}",
                schema.display()
            );
        }
        assert!(!supports_native_thread_queue(&dir.path().join("absent")).await);
    }

    #[test]
    fn pinned_runtime_metadata_matches_generated_schema_fixtures() {
        use sha2::{Digest, Sha256};
        let manifest: Value =
            serde_json::from_str(include_str!("../../../compatibility-manifest.json")).unwrap();
        assert_eq!(manifest["codex"]["version"], CODEX_VERSION);
        assert_eq!(
            manifest["nativeQueue"]["minVersion"],
            NATIVE_QUEUE_MIN_VERSION
        );
        assert_eq!(
            manifest["codex"]["testedRange"],
            serde_json::json!({"oldest": TESTED_CODEX_RANGE.0, "newest": TESTED_CODEX_RANGE.1})
        );
        assert_eq!(
            manifest["codex"]["stableSchemaSha256"],
            STABLE_SCHEMA_SHA256
        );
        assert_eq!(
            manifest["codex"]["experimentalSchemaSha256"],
            EXPERIMENTAL_SCHEMA_SHA256
        );
        assert_eq!(
            manifest["codex"]["alpha16SchemaSha256"]["stable"],
            ALPHA16_STABLE_SCHEMA_SHA256
        );
        assert_eq!(
            manifest["codex"]["alpha16SchemaSha256"]["experimental"],
            ALPHA16_EXPERIMENTAL_SCHEMA_SHA256
        );
        assert_eq!(manifest["policy"]["allowUnknownVersions"], true);
        assert_eq!(
            manifest["policy"]["unknownVersionPolicy"],
            "local-used-contract-check-against-0.155.0-alpha.16.3"
        );
        assert_eq!(
            manifest["codex"]["v162SchemaSha256"]["stable"],
            V162_STABLE_SCHEMA_SHA256
        );
        assert_eq!(
            manifest["codex"]["v162SchemaSha256"]["experimental"],
            V162_EXPERIMENTAL_SCHEMA_SHA256
        );
        assert_eq!(
            expected_schema_hashes("codex-cli 0.162.0-alpha.2"),
            Some((V162_STABLE_SCHEMA_SHA256, V162_EXPERIMENTAL_SCHEMA_SHA256))
        );
        assert_eq!(
            manifest["codex"]["v162_17SchemaSha256"]["stable"],
            V162_17_STABLE_SCHEMA_SHA256
        );
        assert_eq!(
            manifest["codex"]["v162_17SchemaSha256"]["experimental"],
            V162_17_EXPERIMENTAL_SCHEMA_SHA256
        );
        assert_eq!(
            expected_schema_hashes("codex-cli 0.162.0-alpha.17.2"),
            Some((
                V162_17_STABLE_SCHEMA_SHA256,
                V162_17_EXPERIMENTAL_SCHEMA_SHA256
            ))
        );
        assert_eq!(
            manifest["codex"]["v166SchemaSha256"]["stable"],
            V166_STABLE_SCHEMA_SHA256
        );
        assert_eq!(
            manifest["codex"]["v166SchemaSha256"]["experimental"],
            V166_EXPERIMENTAL_SCHEMA_SHA256
        );
        assert_eq!(
            expected_schema_hashes("codex-cli 0.166.0-alpha.6"),
            Some((V166_STABLE_SCHEMA_SHA256, V166_EXPERIMENTAL_SCHEMA_SHA256))
        );
        assert_eq!(
            expected_schema_hashes("codex-cli 0.155.0-alpha.16.4"),
            Some((
                ALPHA16_STABLE_SCHEMA_SHA256,
                ALPHA16_EXPERIMENTAL_SCHEMA_SHA256
            ))
        );
        for (schema, expected) in [
            (include_bytes!("../../../research/codex-app-server/0.155.0-alpha.9/stable/codex_app_server_protocol.v2.schemas.json").as_slice(), STABLE_SCHEMA_SHA256),
            (include_bytes!("../../../research/codex-app-server/0.155.0-alpha.9/experimental/codex_app_server_protocol.v2.schemas.json").as_slice(), EXPERIMENTAL_SCHEMA_SHA256),
            (include_bytes!("../../../research/codex-app-server/0.155.0-alpha.16.3/stable/codex_app_server_protocol.v2.schemas.json").as_slice(), ALPHA16_STABLE_SCHEMA_SHA256),
            (include_bytes!("../../../research/codex-app-server/0.155.0-alpha.16.3/experimental/codex_app_server_protocol.v2.schemas.json").as_slice(), ALPHA16_EXPERIMENTAL_SCHEMA_SHA256),
            (include_bytes!("../../../research/codex-app-server/0.162.0-alpha.2/stable/codex_app_server_protocol.v2.schemas.json").as_slice(), V162_STABLE_SCHEMA_SHA256),
            (include_bytes!("../../../research/codex-app-server/0.162.0-alpha.2/experimental/codex_app_server_protocol.v2.schemas.json").as_slice(), V162_EXPERIMENTAL_SCHEMA_SHA256),
            (include_bytes!("../../../research/codex-app-server/0.162.0-alpha.17.2/stable/codex_app_server_protocol.v2.schemas.json").as_slice(), V162_17_STABLE_SCHEMA_SHA256),
            (include_bytes!("../../../research/codex-app-server/0.162.0-alpha.17.2/experimental/codex_app_server_protocol.v2.schemas.json").as_slice(), V162_17_EXPERIMENTAL_SCHEMA_SHA256),
            (include_bytes!("../../../research/codex-app-server/0.166.0-alpha.6/stable/codex_app_server_protocol.v2.schemas.json").as_slice(), V166_STABLE_SCHEMA_SHA256),
            (include_bytes!("../../../research/codex-app-server/0.166.0-alpha.6/experimental/codex_app_server_protocol.v2.schemas.json").as_slice(), V166_EXPERIMENTAL_SCHEMA_SHA256),
        ] {
            assert_eq!(hex::encode(Sha256::digest(schema)), expected);
        }
    }

    #[test]
    fn pinned_schema_retains_required_methods_and_send_fields() {
        for (schema, server_requests) in [
            (include_str!("../../../research/codex-app-server/0.155.0-alpha.9/experimental/codex_app_server_protocol.v2.schemas.json"), include_str!("../../../research/codex-app-server/0.155.0-alpha.9/experimental/ServerRequest.ts")),
            (include_str!("../../../research/codex-app-server/0.155.0-alpha.16.3/experimental/codex_app_server_protocol.v2.schemas.json"), include_str!("../../../research/codex-app-server/0.155.0-alpha.16.3/experimental/ServerRequest.ts")),
            (include_str!("../../../research/codex-app-server/0.162.0-alpha.2/experimental/codex_app_server_protocol.v2.schemas.json"), include_str!("../../../research/codex-app-server/0.162.0-alpha.2/experimental/ServerRequest.ts")),
            (include_str!("../../../research/codex-app-server/0.162.0-alpha.17.2/experimental/codex_app_server_protocol.v2.schemas.json"), include_str!("../../../research/codex-app-server/0.162.0-alpha.17.2/experimental/ServerRequest.ts")),
            (include_str!("../../../research/codex-app-server/0.166.0-alpha.6/experimental/codex_app_server_protocol.v2.schemas.json"), include_str!("../../../research/codex-app-server/0.166.0-alpha.6/experimental/ServerRequest.ts")),
        ] {
        let schema: Value = serde_json::from_str(schema).unwrap();
        let definitions = &schema["definitions"];
        let requests = definitions["ClientRequest"]["oneOf"].as_array().unwrap();
        for method in RequiredMethod::ALL {
            assert!(
                requests
                    .iter()
                    .any(|request| request["properties"]["method"]["enum"][0] == method.as_str()),
                "missing {}",
                method.as_str()
            );
        }
        for method in ALLOWED_SERVER_REQUESTS {
            assert!(
                server_requests.contains(&format!("\"{method}\"")),
                "missing {method}"
            );
        }
        assert_eq!(
            definitions["TurnStartParams"]["properties"]["clientUserMessageId"]["type"],
            json!(["string", "null"])
        );
        let image = definitions["UserInput"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|input| input["properties"]["type"]["enum"][0] == "image")
            .unwrap();
        assert!(image["anyOf"]
            .as_array()
            .unwrap()
            .iter()
            .any(|variant| variant["required"] == json!(["url"])));
        }
        // Newly advertised capabilities are not automatically authorized.
        for method in [
            "thread/attachment/add",
            "thread/attachment/remove",
            "memory/status",
            "userVerification/cancel",
        ] {
            assert!(!is_allowed_method(method));
        }
    }

    #[test]
    fn initialize_is_exactly_one_experimental_handshake() {
        let request = initialize_request("0.1.0").request;
        assert_eq!(request.method, "initialize");
        assert_eq!(request.id, Some(1));
        assert_eq!(request.params["capabilities"]["experimentalApi"], true);
        assert!(
            require_experimental_api(&json!({"userAgent":"Codex Desktop/0.153.0-alpha.5"})).is_ok()
        );
        assert!(
            require_experimental_api(&json!({"capabilities":{"experimentalApi":false}})).is_err()
        );
    }

    // Contract: the general allowlist refuses remote filesystem access and
    // unknown mutations; Project methods (forks, the Mac's agent-task
    // roster) reach only Project clients. Positive entries are owned by
    // pinned_schema_retains_required_methods_and_send_fields.
    #[test]
    fn the_allowlist_refuses_what_phones_must_not_call() {
        for method in ["fs/readFile", "project/delete"] {
            assert!(!is_allowed_method(method), "{method}");
            assert!(!is_project_method(method), "{method}");
        }
        for method in PROJECT_METHODS {
            assert!(
                !is_allowed_method(method),
                "{method} stays limited to project clients"
            );
        }
        for method in [
            "thread/backgroundTasks/list",
            "thread/nativeSettings",
            "thread/commands/list",
        ] {
            assert!(
                is_project_method(method),
                "{method} must reach project clients"
            );
        }
    }

    #[test]
    fn unknown_fields_are_accepted() {
        let request: RpcRequest = serde_json::from_value(json!({
            "method": "account/read",
            "id": 2,
            "params": {},
            "futureField": "ignored-by-the-typed-subset"
        }))
        .expect("unknown fields should not break the adapter");
        assert_eq!(request.method, "account/read");
    }

    #[test]
    fn native_queue_accepts_later_codex_releases_by_version_order() {
        for (version, accepted) in [
            ("codex-cli 0.163.0-alpha.10.2", true),
            ("codex-cli 0.163.0-alpha.10.10", true),
            // ChatGPT's bundled build on 2026-10-21, and Homebrew/npm's release.
            ("codex-cli 0.163.0-alpha.11", true),
            ("codex-cli 0.163.0", true),
            ("codex-cli 0.164.0-alpha.1", true),
            ("codex-cli 1.0.0\n", true),
            ("codex-cli 0.163.0-alpha.10.1", false),
            ("codex-cli 0.163.0-alpha.10", false),
            ("codex-cli 0.163.0-alpha.9", false),
            ("codex-cli 0.162.0-alpha.17.2", false),
            ("codex-cli 0.163", false),
            ("codex 0.163.0", false),
            ("", false),
        ] {
            assert_eq!(native_queue_version(version), accepted, "{version}");
        }
        use std::cmp::Ordering::*;
        assert_eq!(
            compare_codex_versions("0.163.0", "codex-cli 0.163.0-alpha.11"),
            Some(Greater)
        );
        assert_eq!(compare_codex_versions("0.164.0", "0.164.0"), Some(Equal));
        assert_eq!(
            compare_codex_versions("0.163.0", "0.155.0-alpha.9"),
            Some(Greater)
        );
        assert_eq!(compare_codex_versions("", "0.163.0"), None);
    }

    /// The queue contract measured in codex-cli 0.163.0 (the same bytes as
    /// 0.163.0-alpha.10.2). Changes Cupola would mis-handle are refused.
    #[test]
    fn native_queue_contract_is_checked_by_shape_not_hash() {
        let measured: Value = serde_json::from_slice(include_bytes!(
            "../../../research/codex-app-server/0.163.0/experimental/codex_app_server_protocol.v2.schemas.json"
        ))
        .unwrap();
        assert!(native_queue_contract(&measured));
        // Additive drift elsewhere, or an optional new parameter, is fine.
        let mut widened = measured.clone();
        widened["definitions"]["ThreadQueueAddParams"]["properties"]["priority"] =
            json!({"type": ["integer", "null"]});
        widened["definitions"]["Unrelated"] = json!({"type": "object"});
        assert!(native_queue_contract(&widened));
        // Earlier releases already carried this shape; their queue behavior
        // was never measured, so the version floor refuses them instead.
        let earlier: Value = serde_json::from_slice(include_bytes!(
            "../../../research/codex-app-server/0.162.0-alpha.17.2/experimental/codex_app_server_protocol.v2.schemas.json"
        ))
        .unwrap();
        assert!(native_queue_contract(&earlier));
        // ChatGPT's 0.166.0-alpha.6 keeps the queue contract unchanged.
        let v166: Value = serde_json::from_slice(include_bytes!(
            "../../../research/codex-app-server/0.166.0-alpha.6/experimental/codex_app_server_protocol.v2.schemas.json"
        ))
        .unwrap();
        assert!(native_queue_contract(&v166));
        assert!(native_queue_version("codex-cli 0.166.0-alpha.6"));
        assert!(!native_queue_version("codex-cli 0.162.0-alpha.17.2"));
        type Change = fn(&mut Value);
        let refused: [(&str, Change); 5] = [
            ("new required parameter", |s| {
                s["definitions"]["ThreadQueueAddParams"]["required"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("position"));
            }),
            ("receipt without a submission id", |s| {
                s["definitions"]["QueuedSubmission"]["required"] =
                    json!(["clientUserMessageId", "input"]);
            }),
            ("input differs from turn/start", |s| {
                s["definitions"]["ThreadQueueAddParams"]["properties"]["input"] =
                    json!({"type": "string"});
            }),
            ("list without paging", |s| {
                s["definitions"]["ThreadQueueListResponse"]["properties"]
                    .as_object_mut()
                    .unwrap()
                    .remove("nextCursor");
            }),
            ("no list method", |s| {
                s["definitions"]["ClientRequest"]["oneOf"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|r| r["properties"]["method"]["enum"][0] != "thread/queue/list");
            }),
        ];
        for (name, change) in refused {
            let mut schema = measured.clone();
            change(&mut schema);
            assert!(!native_queue_contract(&schema), "{name}");
        }
    }

    #[test]
    fn versions_outside_the_tested_range_are_flagged_not_refused() {
        for (version, expected) in [
            ("codex-cli 0.155.0-alpha.9", CodexTesting::Tested),
            ("codex-cli 0.162.0-alpha.17.2", CodexTesting::Tested),
            ("codex-cli 0.166.0-alpha.6", CodexTesting::Tested),
            ("0.166.0", CodexTesting::Newer),
            ("codex-cli 0.170.1", CodexTesting::Newer),
            ("codex-cli 0.154.0", CodexTesting::Older),
            ("not a version", CodexTesting::Unknown),
            ("", CodexTesting::Unknown),
        ] {
            assert_eq!(codex_testing(version), expected, "{version}");
        }
    }

    #[test]
    fn degraded_features_are_named_in_plain_words() {
        let binary = std::path::Path::new("/no/such/codex-for-degraded-test");
        assert!(degraded_features(binary).is_empty());
        record_degraded(
            binary,
            &[
                "thread/search",
                "thread/searchOccurrences",
                "unnamed/method",
            ],
        );
        assert_eq!(
            degraded_features(binary),
            vec![
                "Message search".to_owned(),
                "Some optional features".to_owned()
            ]
        );
        record_degraded(binary, &[]);
        assert!(degraded_features(binary).is_empty());
    }
}
