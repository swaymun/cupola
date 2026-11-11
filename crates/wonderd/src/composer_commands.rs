//! The provider's own slash-command catalog for one Project thread, for the
//! phone's `/` list. Wonder reads what the bridge retained from the live native
//! session (`thread/commands/list`) and passes its availability through: it
//! never starts a runtime or a session to find commands. Codex uses native
//! compaction/review RPCs; Cupola entries expand into visible draft text. The last complete list a live session reported is kept per
//! account and working folder, so a chat that is not running yet, or a new
//! chat in that folder, shows that list as `cached`, with when it was seen,
//! instead of nothing. A saved list is never treated as current: it may be
//! out of date, a running chat always answers with its live list, and every
//! live list replaces the saved one.

use crate::{AppState, OwnerAuthority};
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path as FsPath, PathBuf};
use std::time::Duration;

/// The bridges bound a catalog to 512 rows; a reply over that is refused.
const MAX_COMMANDS: usize = 512;
const MAX_NAME: usize = 256;
const MAX_DESCRIPTION: usize = 16 * 1024;
const MAX_HINT: usize = 4096;
const MAX_ALIASES: usize = 128;
/// Each bridge read waits at most 250 ms; this covers transport and queueing.
const READ_TIMEOUT: Duration = Duration::from_secs(3);
/// Saved lists, one per account and working folder; the oldest go first.
const CACHE_DIR: &str = "composer-commands";
const MAX_CACHED: usize = 64;
/// A saved list over this is not read back.
const MAX_CACHE_BYTES: u64 = 512 * 1024;
/// An unchanged list is rewritten only to keep "last seen" roughly current.
const CACHE_REFRESH: chrono::TimeDelta = chrono::TimeDelta::hours(1);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ComposerCommand {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    argument_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    insertion_text: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ComposerCommands {
    /// `ready`: `commands` is the complete native list (it may be empty).
    /// `unknown`: the live session has not reported a usable list yet.
    /// `cached`: no live session holds this thread; `commands` is the last
    /// complete list a live session in the same account and folder reported,
    /// at `fetchedAt`. It may be out of date.
    /// `unavailable`: no live session on the Mac holds this thread.
    availability: &'static str,
    commands: Vec<ComposerCommand>,
    /// Changes whenever the native list may have changed; opaque.
    revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fetched_at: Option<String>,
}

impl ComposerCommands {
    fn without(availability: &'static str) -> Self {
        Self {
            availability,
            commands: Vec::new(),
            revision: None,
            fetched_at: None,
        }
    }
}

/// One saved list.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedCommands {
    store: String,
    cwd: String,
    fetched_at: String,
    commands: Vec<ComposerCommand>,
}

fn cache_path(data_dir: &FsPath, store: &str, cwd: &str) -> PathBuf {
    let key = hex::encode(Sha256::digest(format!("{store}\0{cwd}").as_bytes()));
    data_dir
        .join(CACHE_DIR)
        .join(format!("{}.json", &key[..32]))
}

/// The saved list for `store` and `cwd`.
fn read_cached(data_dir: &FsPath, store: &str, cwd: &str) -> Option<CachedCommands> {
    let path = cache_path(data_dir, store, cwd);
    let file = std::fs::File::open(&path).ok()?;
    if file.metadata().ok()?.len() > MAX_CACHE_BYTES {
        return None;
    }
    let cached: CachedCommands = serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
    (cached.store == store && cached.cwd == cwd && cached.commands.len() <= MAX_COMMANDS)
        .then_some(cached)
}

/// Saves a live list unless the same one was saved recently, then drops the
/// oldest saved lists past the bound. Failure only loses the saved list.
fn write_cached(data_dir: &FsPath, entry: &CachedCommands) {
    if let Some(old) = read_cached(data_dir, &entry.store, &entry.cwd) {
        let recent = chrono::DateTime::parse_from_rfc3339(&old.fetched_at)
            .ok()
            .zip(chrono::DateTime::parse_from_rfc3339(&entry.fetched_at).ok())
            .is_some_and(|(old, new)| new - old < CACHE_REFRESH);
        if old.commands == entry.commands && recent {
            return;
        }
    }
    let dir = data_dir.join(CACHE_DIR);
    let path = cache_path(data_dir, &entry.store, &entry.cwd);
    let Ok(json) = serde_json::to_vec(entry) else {
        return;
    };
    if json.len() as u64 > MAX_CACHE_BYTES || std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(mut file) = tempfile::NamedTempFile::new_in(&dir) else {
        return;
    };
    if std::io::Write::write_all(&mut file, &json).is_err() || file.persist(&path).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut saved: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    if saved.len() > MAX_CACHED {
        saved.sort();
        for (_, old) in &saved[..saved.len() - MAX_CACHED] {
            let _ = std::fs::remove_file(old);
        }
    }
}

fn bounded(value: Option<&Value>, limit: usize) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| text.len() <= limit)
        .map(str::to_owned)
}

/// One native row. Claude rows carry `argumentHint` and `aliases`; ACP rows
/// carry `input.hint`. A row without a usable name invalidates the catalog.
fn command(row: &Value) -> Option<ComposerCommand> {
    let name = bounded(row.get("name"), MAX_NAME).filter(|name| {
        !name.is_empty() && !name.chars().any(|c| c.is_whitespace() || c.is_control())
    })?;
    let description = match row.get("description") {
        None | Some(Value::Null) => String::new(),
        value => bounded(value, MAX_DESCRIPTION)?,
    };
    let argument_hint = bounded(
        row.get("argumentHint")
            .or_else(|| row.pointer("/input/hint")),
        MAX_HINT,
    )
    .filter(|hint| !hint.trim().is_empty());
    let aliases = match row.get("aliases") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) if values.len() <= MAX_ALIASES => values
            .iter()
            .map(|alias| bounded(Some(alias), MAX_NAME).filter(|alias| !alias.is_empty()))
            .collect::<Option<Vec<_>>>()?,
        Some(_) => return None,
    };
    Some(ComposerCommand {
        name,
        description,
        argument_hint,
        aliases,
        insertion_text: None,
    })
}

/// The bridge's reply, checked. `session` is the native session this thread is
/// bound to, when known; a reply for another session is not this thread's.
pub(crate) fn parse_reply(reply: &Value, session: Option<&str>) -> ComposerCommands {
    let replied_session = reply.get("sessionId").and_then(Value::as_str);
    if session.is_some() && replied_session.is_some() && replied_session != session {
        return ComposerCommands::without("unavailable");
    }
    let owner = reply
        .get("ownerId")
        .or_else(|| reply.get("peerId"))
        .and_then(Value::as_str);
    let revision = match (owner, reply.get("revision").and_then(Value::as_u64)) {
        (Some(owner), Some(revision)) => Some(format!("{owner}:{revision}")),
        _ => None,
    };
    match reply.get("availability").and_then(Value::as_str) {
        Some("ready") => {
            let commands = match reply.get("commands").and_then(Value::as_array) {
                Some(rows) if rows.len() <= MAX_COMMANDS => {
                    rows.iter().map(command).collect::<Option<Vec<_>>>()
                }
                _ => None,
            };
            match commands {
                Some(commands) => ComposerCommands {
                    availability: "ready",
                    commands,
                    revision,
                    fetched_at: None,
                },
                // A malformed list is not shown in part.
                None => ComposerCommands {
                    availability: "unknown",
                    commands: Vec::new(),
                    revision,
                    fetched_at: None,
                },
            }
        }
        Some("unknown") => ComposerCommands {
            availability: "unknown",
            commands: Vec::new(),
            revision,
            fetched_at: None,
        },
        Some("unavailable") => ComposerCommands::without("unavailable"),
        _ => ComposerCommands::without("unknown"),
    }
}

/// Codex has no native composer catalog. These entries name the App Server
/// methods Cupola actually dispatches. T3 handles /compact itself; it does
/// not expose review/start through its send endpoint.
fn codex_catalog(t3: bool) -> ComposerCommands {
    let mut commands = vec![ComposerCommand {
        name: "compact".into(),
        description: "Summarize earlier context to make room".into(),
        argument_hint: None,
        aliases: vec![],
        insertion_text: None,
    }];
    if !t3 {
        commands.push(ComposerCommand {
            name: "review".into(),
            description: "Review uncommitted changes, or give review instructions".into(),
            argument_hint: Some("[instructions]".into()),
            aliases: vec![],
            insertion_text: None,
        });
    }
    ComposerCommands {
        availability: "ready",
        commands,
        revision: Some("codex-actions:1".into()),
        fetched_at: None,
    }
}

const CUPOLA_ACTIONS: [(&str, &str, &str); 2] = [
    ("cupola-summary", "Cupola · Draft a request for a summary", "Summarize this conversation's decisions, open questions, and next steps."),
    ("cupola-review", "Cupola · Draft a review request", "Review the current Project changes for correctness and regressions. Report concrete findings with file references."),
];

/// Selecting a suggestion expands the draft on the client. Also accept a
/// directly typed shortcut, including from clients without insertionText.
pub(crate) fn expand_cupola(body: &str) -> String {
    let (command, arguments) = body
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((body.trim(), ""));
    let Some((_, _, text)) = CUPOLA_ACTIONS
        .iter()
        .find(|(name, _, _)| command.strip_prefix('/') == Some(*name))
    else {
        return body.into();
    };
    if arguments.trim().is_empty() {
        (*text).into()
    } else {
        format!("{text}\n\n{}", arguments.trim())
    }
}

/// Cupola draft shortcuts expand visibly before Send. They never pretend to
/// be native commands, nor run an action merely by choosing a suggestion.
fn with_cupola(mut catalog: ComposerCommands, family: crate::AgentFamily) -> ComposerCommands {
    if family == crate::AgentFamily::Claude
        && matches!(catalog.availability, "unavailable" | "unknown")
    {
        catalog.commands.push(ComposerCommand {
            name: "compact".into(),
            description: "Summarize earlier context to make room".into(),
            argument_hint: None,
            aliases: vec![],
            insertion_text: None,
        });
    }
    for (name, description, text) in CUPOLA_ACTIONS {
        catalog.commands.retain(|c| c.name != name);
        catalog.commands.push(ComposerCommand {
            name: name.into(),
            description: description.into(),
            argument_hint: None,
            aliases: vec![],
            insertion_text: Some(text.into()),
        });
    }
    catalog
}

/// Only standalone native commands are interpreted. Arguments on /compact
/// are rejected explicitly; they are never silently thrown away.
pub(crate) fn codex_request(
    body: &str,
    thread: &str,
    has_attachments: bool,
) -> Result<Option<(&'static str, Value)>, String> {
    let (command, arguments) = body
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((body.trim(), ""));
    let arguments = arguments.trim();
    if !matches!(command, "/compact" | "/review") {
        return Ok(None);
    }
    if has_attachments {
        return Err("Send files separately from this command.".into());
    }
    if command == "/compact" {
        if !arguments.is_empty() {
            return Err(
                "Use /compact on its own. Send your instructions as a separate message.".into(),
            );
        }
        Ok(Some(("thread/compact/start", json!({"threadId": thread}))))
    } else {
        let target = if arguments.is_empty() {
            json!({"type":"uncommittedChanges"})
        } else {
            json!({"type":"custom", "instructions": arguments})
        };
        Ok(Some((
            "review/start",
            json!({"threadId": thread, "target": target, "delivery":"inline"}),
        )))
    }
}

fn refused(status: StatusCode, detail: &str) -> Response {
    (status, detail.to_owned()).into_response()
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CommandsQuery {
    /// The provider of a new chat that has no conversation yet. An existing
    /// conversation always uses its own provider.
    family: Option<crate::AgentFamily>,
    #[serde(default)]
    in_t3_code: bool,
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(conversation_id): Path<String>,
    Query(query): Query<CommandsQuery>,
) -> Response {
    let conversation = match state.store.project_conversation(&conversation_id).await {
        Ok(Some(conversation)) => conversation,
        Ok(None) => {
            return match conversation_id.strip_prefix("project-files:") {
                Some(scope) => draft(&state, scope, query.family, query.in_t3_code).await,
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
        Err(_) => {
            return refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "The Project thread could not be checked.",
            )
        }
    };
    let project = match state.store.project(&conversation.project_id).await {
        Ok(Some(project)) => project,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => {
            return refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "The Project could not be checked.",
            )
        }
    };
    let project = crate::projects::thread_project(&state, project, &conversation.cwd).await;
    if !project.is_included || project.root_for(&conversation.cwd).is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if conversation.provider_store != crate::providers::provider_store(&state, conversation.family)
    {
        return StatusCode::CONFLICT.into_response();
    }
    let commands = if conversation.family == crate::AgentFamily::Codex {
        let known = match conversation.native_session_id.as_deref() {
            Some(native) => state
                .store
                .t3_code_thread(&conversation_id, native)
                .await
                .ok()
                .flatten()
                .is_some(),
            None => false,
        };
        let t3 = known
            || crate::t3_code::holder(&conversation).await.is_some()
            || crate::projects::t3_code_thread(&state, &conversation_id)
                .await
                .is_some();
        codex_catalog(t3)
    } else if !conversation.family.provider().capabilities.slash_commands {
        // No native catalog to wait for: only Cupola's own actions show.
        ComposerCommands::without("ready")
    } else {
        match live(&state, &conversation).await {
            Ok(commands) => commands,
            Err(refusal) => return refusal,
        }
    };
    saved(
        &state,
        conversation.family,
        conversation.provider_store,
        conversation.cwd,
        commands,
    )
    .await
}

/// A new chat's `project-files:<project>:<folder>` scope: nothing runs for
/// it, so it can only show the saved list for the current account and folder.
async fn draft(
    state: &AppState,
    scope: &str,
    family: Option<crate::AgentFamily>,
    in_t3_code: bool,
) -> Response {
    let Some(family) = family else {
        return refused(StatusCode::BAD_REQUEST, "Choose an agent for this chat.");
    };
    let Some((project_id, root_id)) = scope.split_once(':') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let project = match state.store.project(project_id).await {
        Ok(Some(project)) => project,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => {
            return refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "The Project could not be checked.",
            )
        }
    };
    // The same folder checks as the new chat's file list.
    let Some(cwd) = project
        .roots
        .iter()
        .find(|root| root.id == root_id)
        .map(|root| root.path.clone())
    else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let denied = state.denied_roots.clone();
    let allowed = project.is_included
        && tokio::task::spawn_blocking(move || {
            crate::projects::validate_execution_roots(&project, &denied).is_ok()
        })
        .await
        .unwrap_or(false);
    if !allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    let store = crate::providers::provider_store(state, family).to_owned();
    saved(
        state,
        family,
        store,
        cwd,
        if family == crate::AgentFamily::Codex {
            codex_catalog(in_t3_code)
        } else if !family.provider().capabilities.slash_commands {
            ComposerCommands::without("ready")
        } else {
            ComposerCommands::without("unavailable")
        },
    )
    .await
}

/// Saves a live `ready` list for chats that are not running, replacing the
/// one saved before, or answers an `unavailable` one with the saved list for
/// the same account and folder.
async fn saved(
    state: &AppState,
    family: crate::AgentFamily,
    store: String,
    cwd: String,
    commands: ComposerCommands,
) -> Response {
    if !family.provider().saved_commands
        || !matches!(commands.availability, "ready" | "unavailable")
    {
        return Json(with_cupola(commands, family)).into_response();
    }
    let data_dir = state.data_dir.clone();
    // One small file; kept off the async workers.
    let commands = tokio::task::spawn_blocking(move || {
        // A thread keeps the folder path it started with and a new chat the
        // Project's; the same folder is one saved list either way.
        let cwd = crate::canonical_path::canonicalize(&cwd)
            .ok()
            .and_then(|path| path.into_os_string().into_string().ok())
            .unwrap_or(cwd);
        if commands.availability == "ready" {
            write_cached(
                &data_dir,
                &CachedCommands {
                    store,
                    cwd,
                    fetched_at: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    commands: commands.commands.clone(),
                },
            );
            return commands;
        }
        match read_cached(&data_dir, &store, &cwd) {
            Some(cached) => ComposerCommands {
                availability: "cached",
                commands: cached.commands,
                revision: Some(format!("cached:{}", cached.fetched_at)),
                fetched_at: Some(cached.fetched_at),
            },
            None => commands,
        }
    })
    .await
    .unwrap_or_else(|_| ComposerCommands::without("unavailable"));
    Json(with_cupola(commands, family)).into_response()
}

/// What the live bridge holds for this thread, without starting anything.
async fn live(
    state: &AppState,
    conversation: &wonder_store::StoredProjectConversation,
) -> Result<ComposerCommands, Response> {
    // No binding yet: nothing has run, so no native session holds commands.
    let binding = match state
        .store
        .runtime_binding(&conversation.conversation_id)
        .await
    {
        Ok(Some(binding)) => binding,
        Ok(None) => return Ok(ComposerCommands::without("unavailable")),
        Err(_) => {
            return Err(refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "The Project runtime identity could not be checked.",
            ))
        }
    };
    if binding.execution_scope != "projects"
        || binding.family != conversation.family
        || conversation
            .native_session_id
            .as_deref()
            .is_some_and(|native| {
                !crate::providers::binds_native_session(conversation.family, native, &binding)
            })
    {
        return Err(StatusCode::CONFLICT.into_response());
    }
    // Reading commands must not start a runtime on the Mac.
    let Some(rpc) = crate::providers::live_rpc_for_store(
        state,
        conversation.family,
        &conversation.provider_store,
    )
    .await
    else {
        return Ok(ComposerCommands::without("unavailable"));
    };
    let reply = tokio::time::timeout(
        READ_TIMEOUT,
        rpc.request(
            "thread/commands/list",
            json!({"threadId": binding.thread_id}),
        ),
    )
    .await;
    Ok(match reply {
        Ok(Ok(response)) => match (response.error, response.result) {
            (None, Some(result)) => parse_reply(&result, binding.session_id.as_deref()),
            _ => ComposerCommands::without("unknown"),
        },
        _ => ComposerCommands::without("unknown"),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // Contract: the bridge's availability passes through unchanged, rows keep
    // native order and metadata, and a malformed or foreign reply never shows
    // a partial or another session's list.
    #[test]
    fn bridge_replies_pass_through_without_fabrication() {
        for (draft, expected) in [
            ("/cupola-summary", CUPOLA_ACTIONS[0].2.to_owned()),
            (
                " /cupola-review  focus on storage ",
                format!("{}\n\nfocus on storage", CUPOLA_ACTIONS[1].2),
            ),
            ("/compact", "/compact".into()),
            ("/cupola-summary-extra", "/cupola-summary-extra".into()),
            ("literal /cupola-summary", "literal /cupola-summary".into()),
        ] {
            assert_eq!(expand_cupola(draft), expected);
        }
        // The host's real Codex list: compact always, review only when Cupola
        // (not T3) dispatches it, then Cupola's own actions.
        for (t3, names) in [
            (
                false,
                vec!["compact", "review", "cupola-summary", "cupola-review"],
            ),
            (true, vec!["compact", "cupola-summary", "cupola-review"]),
        ] {
            let served = with_cupola(codex_catalog(t3), crate::AgentFamily::Codex);
            let served_names: Vec<_> = served.commands.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(served_names, names);
            println!("host codex commands t3={t3}: {served_names:?}");
        }
        let claude = parse_reply(
            &json!({"availability": "ready", "sessionId": "s1", "ownerId": "o1", "revision": 4,
                "reason": null, "commands": [
                    {"name": "review", "description": "Review a PR", "argumentHint": "<pr>",
                     "aliases": ["r"], "builtin": false},
                    {"name": "compact", "description": "Compact context", "argumentHint": ""}]}),
            Some("s1"),
        );
        assert_eq!(claude.availability, "ready");
        assert_eq!(claude.revision.as_deref(), Some("o1:4"));
        let names: Vec<_> = claude.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["review", "compact"]);
        assert_eq!(claude.commands[0].argument_hint.as_deref(), Some("<pr>"));
        assert_eq!(claude.commands[0].aliases, ["r"]);
        assert_eq!(claude.commands[1].argument_hint, None);
        let value = serde_json::to_value(&claude).unwrap();
        crate::tests::validate_http_contract("composerCommands", &value);

        let acp = parse_reply(
            &json!({"availability": "ready", "sessionId": "acp", "peerId": "p", "revision": 1,
                "commands": [{"name": "init", "description": "Create AGENTS.md",
                    "input": {"hint": "path"}}]}),
            Some("acp"),
        );
        assert_eq!(acp.commands[0].argument_hint.as_deref(), Some("path"));
        assert_eq!(acp.revision.as_deref(), Some("p:1"));

        let empty = parse_reply(
            &json!({"availability": "ready", "ownerId": "o", "revision": 1, "commands": []}),
            None,
        );
        assert_eq!((empty.availability, empty.commands.len()), ("ready", 0));

        for (reply, expected) in [
            (
                json!({"availability": "unavailable", "sessionId": "s1", "ownerId": null,
                    "revision": null, "commands": null}),
                "unavailable",
            ),
            (
                json!({"availability": "unknown", "ownerId": "o", "revision": 2, "commands": null}),
                "unknown",
            ),
            // A row without a name, or a list over the bound, is not shown in part.
            (
                json!({"availability": "ready", "commands": [{"name": "ok"}, {"description": "x"}]}),
                "unknown",
            ),
            (
                json!({"availability": "ready",
                    "commands": vec![json!({"name": "x"}); MAX_COMMANDS + 1]}),
                "unknown",
            ),
            (
                json!({"availability": "ready", "commands": [{"name": "two words"}]}),
                "unknown",
            ),
            // Another session's list is never this thread's.
            (
                json!({"availability": "ready", "sessionId": "other", "commands": []}),
                "unavailable",
            ),
            (json!({"availability": "later"}), "unknown"),
            (json!({}), "unknown"),
        ] {
            let parsed = parse_reply(&reply, Some("s1"));
            assert_eq!(parsed.availability, expected, "{reply}");
            assert!(parsed.commands.is_empty(), "{reply}");
            crate::tests::validate_http_contract(
                "composerCommands",
                &serde_json::to_value(&parsed).unwrap(),
            );
        }
    }

    async fn read(state: &AppState, id: &str) -> (StatusCode, Value) {
        let (status, mut value) = crate::test_support::call(
            state,
            "GET",
            &match id.split_once('?') {
                Some((id, query)) => format!("/api/v1/conversations/{id}/commands?{query}"),
                None => format!("/api/v1/conversations/{id}/commands"),
            },
            Value::Null,
        )
        .await;
        if status == StatusCode::OK {
            crate::tests::validate_http_contract("composerCommands", &value);
            // The cache assertions below compare the provider's list. Separately
            // verify that every successful catalog includes working Cupola drafts.
            let fallback = matches!(
                value["availability"].as_str(),
                Some("unknown" | "unavailable")
            );
            let rows = value["commands"].as_array_mut().unwrap();
            assert!(rows.iter().any(|c| c["name"] == "cupola-summary"
                && c["insertionText"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("Summarize"))));
            if fallback {
                assert!(rows.iter().any(|c| c["name"] == "compact"));
            }
            rows.retain(|c| c["insertionText"].is_null() && !(fallback && c["name"] == "compact"));
        }
        (status, value)
    }

    async fn bind(state: &AppState, conversation: &str, thread: &str, session: &str) {
        let store = crate::providers::provider_store(state, crate::AgentFamily::Claude);
        state
            .store
            .bind_project_runtime(
                conversation,
                crate::AgentFamily::Claude,
                store,
                thread,
                Some(session),
                "now",
            )
            .await
            .unwrap();
    }

    // Contract: the route reads only a live bridge for a bound, owned Project
    // thread of a provider with commands, passes its answer through, and
    // reports `unavailable` instead of starting anything.
    #[tokio::test]
    async fn route_reads_the_live_bridge_for_owned_threads_only() {
        use crate::AgentFamily;
        use std::sync::Arc;
        use tokio::sync::Mutex;
        use wonder_app_server::{AppServerClient, BridgeLaunchConfig};

        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let source = dir.path().join("project-source");
        std::fs::create_dir(&source).unwrap();
        // A linked worktree of the Project's folder, not itself a saved folder.
        let worktree = dir.path().join("commands-worktree");
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
        let roots =
            crate::projects::validate_folders(&[source.to_string_lossy().into_owned()], &[])
                .unwrap();
        state
            .store
            .create_project("project", "request", "hash", "Test", &roots, 0, "now")
            .await
            .unwrap();
        state.projects = crate::projects::ProjectRuntime::configured(
            dir.path().join("codex"),
            "test".into(),
            &dir.path().join("home"),
            &dir.path().join("claude"),
            crate::ingestion::notification_sink(state.store.clone()),
        );
        let cwd = crate::canonical_path::canonicalize(&source).unwrap();
        let cwd = cwd.to_str().unwrap();
        let insert = |id: &'static str, family: AgentFamily, session: &'static str| {
            let state = state.clone();
            let cwd = if id == "worktree-chat" {
                worktree.to_str().unwrap().to_owned()
            } else {
                cwd.to_owned()
            };
            async move {
                let store = crate::providers::provider_store(&state, family).to_owned();
                state
                    .store
                    .create_project_conversation(wonder_store::ProjectConversationInsert {
                        conversation_id: id,
                        project_id: "project",
                        family,
                        provider_store: &store,
                        native_session_id: Some(session),
                        cwd: &cwd,
                        roots_revision: 1,
                        title: "Commands",
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
        };
        insert("claude-chat", AgentFamily::Claude, "session-1").await;
        insert("codex-chat", AgentFamily::Codex, "codex-thread").await;
        insert("other-chat", AgentFamily::Claude, "session-2").await;
        bind(&state, "other-chat", "claude-session-2", "session-2").await;
        insert("worktree-chat", AgentFamily::Claude, "session-wt").await;
        bind(&state, "worktree-chat", "claude-session-wt", "session-wt").await;
        // A chat that has never run, in the same folder as claude-chat.
        insert("new-chat", AgentFamily::Claude, "session-new").await;

        // Codex has no catalog; a thread that never ran has no session.
        let (status, codex) = read(&state, "codex-chat").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(codex["commands"][0]["name"], "compact");
        assert_eq!(codex["commands"][1]["name"], "review");
        assert_eq!(read(&state, "missing").await.0, StatusCode::NOT_FOUND);
        let (status, cold) = read(&state, "claude-chat").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cold["availability"], "unavailable");
        crate::tests::validate_http_contract("composerCommands", &cold);

        bind(&state, "claude-chat", "claude-session-1", "session-1").await;
        // Bound, but Claude is not running: reading starts nothing.
        let (_, idle) = read(&state, "claude-chat").await;
        assert_eq!(idle["availability"], "unavailable");

        // What the live sessions report; the test changes it below.
        let review = json!([{"name": "review", "description": "Review changes",
            "argumentHint": "<pr>", "aliases": ["r"]}]);
        let report = |commands: Option<&Value>| {
            let path = dir.path().join("live-commands.json");
            match commands {
                Some(commands) => std::fs::write(&path, commands.to_string()).unwrap(),
                None => std::fs::remove_file(&path).unwrap(),
            }
        };
        report(Some(
            &json!([{"name": "review", "description": "Review changes",
            "argumentHint": "<pr>", "aliases": ["r"], "builtin": false}]),
        ));
        let entrypoint = dir.path().join("claude-bridge.py");
        std::fs::write(
            &entrypoint,
            r#"import json, sys, os
root = os.path.dirname(__file__)
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    m = r.get('method'); p = r.get('params', {}); result = {}
    with open(root + '/claude-requests', 'a') as log: log.write(json.dumps({'method': m, 'params': p}) + '\n')
    if m == 'initialize': result = {'wonderBridge': {'protocolVersion': 1, 'family': 'claude'}, 'capabilities': {'experimentalApi': True}}
    elif m == 'thread/commands/list':
        sessions = {'claude-session-1': 'session-1', 'claude-session-wt': 'session-wt'}
        if p['threadId'] in sessions and not os.path.exists(root + '/live-commands.json'):
            result = {'availability': 'unknown', 'sessionId': sessions[p['threadId']], 'ownerId': 'owner', 'revision': 4, 'reason': None, 'commands': None}
        elif p['threadId'] in sessions:
            result = {'availability': 'ready', 'sessionId': sessions[p['threadId']], 'ownerId': 'owner', 'revision': 3, 'reason': None,
                'commands': json.load(open(root + '/live-commands.json'))}
        else:
            result = {'availability': 'ready', 'sessionId': 'another-session', 'ownerId': 'owner', 'revision': 1, 'commands': []}
    print(json.dumps({'id': r['id'], 'result': result}), flush=True)
"#,
        )
        .unwrap();
        let config = BridgeLaunchConfig {
            node_bin: "/usr/bin/python3".into(),
            entrypoint,
            state_dir: dir.path().join("claude-state"),
            npm_cli: None,
            claude_config_dir: Some(dir.path().join("claude-home")),
            wonder_version: "test".into(),
            family: "claude",
        };
        let client = Arc::new(Mutex::new(AppServerClient::unavailable(
            "test".into(),
            crate::ingestion::notification_sink(state.store.clone()),
        )));
        client
            .lock()
            .await
            .restart_bridge(config.clone())
            .await
            .unwrap();
        state.claude = Some(Arc::new(crate::claude::Runtime { client, config }));

        // A reply naming another session is not this thread's list, and with
        // nothing seen in this folder yet there is no saved list either.
        let (_, foreign) = read(&state, "other-chat").await;
        assert_eq!(foreign["availability"], "unavailable");
        assert_eq!(foreign["commands"], json!([]));
        assert_eq!(
            read(&state, "new-chat").await.1["availability"],
            "unavailable"
        );

        let (status, ready) = read(&state, "claude-chat").await;
        assert_eq!(status, StatusCode::OK, "{ready}");
        assert_eq!(
            ready,
            json!({"availability": "ready", "revision": "owner:3", "commands": [
                {"name": "review", "description": "Review changes", "argumentHint": "<pr>", "aliases": ["r"]}]})
        );
        crate::tests::validate_http_contract("composerCommands", &ready);
        let requests = std::fs::read_to_string(dir.path().join("claude-requests")).unwrap();
        assert!(
            requests.contains(r#""threadId": "claude-session-1""#),
            "{requests}"
        );

        // A thread working in a linked worktree reads its own session's list.
        let (status, linked) = read(&state, "worktree-chat").await;
        assert_eq!(status, StatusCode::OK, "{linked}");
        assert_eq!(linked, ready, "{linked}");
        let requests = std::fs::read_to_string(dir.path().join("claude-requests")).unwrap();
        assert!(
            requests.contains(r#""threadId": "claude-session-wt""#),
            "{requests}"
        );

        // Cold chats in the same folder show the last live list, marked as
        // saved with when it was seen; the foreign reply's list never is.
        for id in ["new-chat", "other-chat"] {
            let (status, cached) = read(&state, id).await;
            assert_eq!(status, StatusCode::OK, "{cached}");
            assert_eq!(cached["availability"], "cached", "{cached}");
            assert_eq!(cached["commands"], review, "{cached}");
            assert!(
                chrono::DateTime::parse_from_rfc3339(cached["fetchedAt"].as_str().unwrap()).is_ok(),
                "{cached}"
            );
            crate::tests::validate_http_contract("composerCommands", &cached);
        }
        // A new chat that has not been sent shows its folder's saved list.
        let root = state.store.project("project").await.unwrap().unwrap().roots[0]
            .id
            .clone();
        let draft = format!("project-files:project:{root}");
        let (status, drafted) = read(&state, &format!("{draft}?family=claude")).await;
        assert_eq!(status, StatusCode::OK, "{drafted}");
        assert_eq!(drafted["availability"], "cached", "{drafted}");
        assert_eq!(drafted["commands"], review, "{drafted}");
        crate::tests::validate_http_contract("composerCommands", &drafted);
        assert_eq!(read(&state, &draft).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            read(&state, &format!("{draft}?family=codex")).await.0,
            StatusCode::OK
        );
        let (status, t3) = read(&state, &format!("{draft}?family=codex&inT3Code=true")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            t3["commands"],
            json!([{"name":"compact", "description":"Summarize earlier context to make room"}])
        );
        assert_eq!(
            read(&state, "project-files:project:elsewhere?family=claude")
                .await
                .0,
            StatusCode::FORBIDDEN
        );

        // With Claude stopped, a thread that ran keeps its saved list too.
        let running = state.claude.clone().unwrap();
        let stopped = Arc::new(Mutex::new(AppServerClient::unavailable(
            "test".into(),
            crate::ingestion::notification_sink(state.store.clone()),
        )));
        state.claude = Some(Arc::new(crate::claude::Runtime {
            client: stopped,
            config: running.config.clone(),
        }));
        assert_eq!(
            read(&state, "claude-chat").await.1["availability"],
            "cached"
        );

        // A running chat always answers with its live list, never the saved
        // one, and that list replaces the saved one for cold chats.
        state.claude = Some(running);
        let ship = json!([{"name": "ship", "description": "Ship it"}]);
        report(Some(&ship));
        let (_, live) = read(&state, "claude-chat").await;
        assert_eq!(
            (live["availability"].clone(), live["commands"].clone()),
            (json!("ready"), ship.clone()),
            "{live}"
        );
        assert!(live.get("fetchedAt").is_none(), "{live}");
        let (_, replaced) = read(&state, "new-chat").await;
        assert_eq!(replaced["availability"], "cached", "{replaced}");
        assert_eq!(replaced["commands"], ship, "{replaced}");
        // A live session that has not reported a list yet says so and
        // leaves the saved list alone.
        report(None);
        assert_eq!(
            read(&state, "claude-chat").await.1["availability"],
            "unknown"
        );
        assert_eq!(read(&state, "new-chat").await.1["commands"], ship);

        // A Project no longer in Wonder answers nothing.
        state
            .store
            .update_project_metadata("project", None, Some(false), None, "later")
            .await
            .unwrap();
        assert_eq!(read(&state, "claude-chat").await.0, StatusCode::FORBIDDEN);
    }
}
