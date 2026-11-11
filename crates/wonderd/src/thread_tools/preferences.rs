//! `wonder_preferences`: an agent reads the computer's Cupola preferences and,
//! when the owner asks and the thread has full access, changes the safe ones
//! ("start new chats in T3 Code", "use Sonnet by default for Claude", "include
//! worktrees", "auto-archive after a week"). It calls the same handlers the
//! Settings screens use, so validation and storage are theirs, and the app
//! shows the new value the next time Settings loads. Computer use, the thread
//! tools switch, access modes, approvals, pairing and relay are not reachable
//! from here. This is Cupola's own tool; it never calls T3 Code's.
use super::*;
use crate::{agent_settings, default_models, projects, OwnerAuthority};
use axum::{
    extract::{Path, State},
    response::Response,
    Extension, Json,
};

pub(in crate::thread_tools) const NAME: &str = "wonder_preferences";

const UPDATE_FIELDS: [&str; 7] = [
    "startsInT3Code",
    "defaultModel",
    "includeWorktrees",
    "autoArchive",
    "resumeAfterLimit",
    "continueAfterRestart",
    "customInstruction",
];
const PROJECT_FIELDS: [&str; 4] = [
    "includeWorktrees",
    "autoArchive",
    "resumeAfterLimit",
    "continueAfterRestart",
];

pub(in crate::thread_tools) fn spec() -> Value {
    let families: Vec<_> = AgentFamily::ALL
        .into_iter()
        .map(AgentFamily::as_str)
        .collect();
    json!({"type":"function","name":NAME,
        "description":"Read this computer's Cupola preferences, or change some when the owner asks you to (for example \"make new chats start in T3 Code\", \"use Sonnet by default for Claude\", \"include worktrees\", \"auto-archive after a week\"). Without update it returns the new-chat defaults (startsInT3Code and each provider's default model, effort and speed), the Project settings (includeWorktrees, autoArchive, resumeAfterLimit, continueAfterRestart) and each provider's custom instruction. With update it changes only the fields you give and returns before and after; the owner sees the new values in Settings the next time it opens. Changing needs a full access thread; otherwise tell the owner they can change it in Settings. Only change what the owner asked for. Computer use, tools, access and approvals are not available here.",
        "inputSchema":{"type":"object","additionalProperties":false,
            "properties":{"update":{"type":"object","additionalProperties":false,"minProperties":1,
                "properties":{
                    "startsInT3Code":{"type":"boolean","description":"New chats start in T3 Code."},
                    "defaultModel":{"type":"object","additionalProperties":false,"required":["family","model"],
                        "description":"A provider's default for new chats. model must be one wonder_models lists for it; null returns to the provider's first model. Effort and serviceTier left out keep their saved values (dropped only if the new model does not offer them).",
                        "properties":{"family":{"type":"string","enum":families},"model":{"type":["string","null"]},
                            "effort":{"type":"string"},"serviceTier":{"type":"string"}}},
                    "includeWorktrees":{"type":"boolean"},
                    "autoArchive":{"type":"object","additionalProperties":false,"required":["idleDays","onMerge"],
                        "description":"Archive threads idle this many days (0 is off) and, if onMerge, those whose pull request merged.",
                        "properties":{"idleDays":{"type":"integer","minimum":0},"onMerge":{"type":"boolean"}}},
                    "resumeAfterLimit":{"type":"boolean"},
                    "continueAfterRestart":{"type":"boolean"},
                    "customInstruction":{"type":"object","additionalProperties":false,"required":["family","enabled","text"],
                        "description":"A provider's custom instruction (up to 4,000 characters); the text is kept while it is off.",
                        "properties":{"family":{"type":"string","enum":families},"enabled":{"type":"boolean"},"text":{"type":"string","maxLength":4000}}}}}}}})
}

/// The settings route's answer: its JSON, or its plain-words refusal.
async fn answer(response: Response) -> Result<Value, ToolError> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .map_err(|_| ToolError::new("unavailable", "The setting could not be read. Try again."))?;
    if !status.is_success() {
        return Err(ToolError::new(
            "rejected",
            String::from_utf8_lossy(&bytes).into_owned(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| ToolError::new("unavailable", "The setting could not be read. Try again."))
}

fn body<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ToolError> {
    serde_json::from_value(value).map_err(|error| ToolError::new("invalid", error.to_string()))
}

fn family(value: &Value) -> Result<AgentFamily, ToolError> {
    body(value.get("family").cloned().unwrap_or(Value::Null))
}

async fn defaults(state: &AppState) -> Result<Value, ToolError> {
    answer(default_models::list(State(state.clone()), Extension(OwnerAuthority)).await).await
}

async fn project_settings(state: &AppState) -> Result<Value, ToolError> {
    answer(projects::settings(State(state.clone()), Extension(OwnerAuthority)).await).await
}

async fn instructions(state: &AppState) -> Result<Value, ToolError> {
    answer(
        agent_settings::custom_instructions(State(state.clone()), Extension(OwnerAuthority)).await,
    )
    .await
}

/// `projects::set_settings` reaches back into this module's caller (archiving
/// and restart continuation resume threads), so rustc cannot prove its future
/// `Send`; it runs to completion on a blocking thread instead.
async fn set_project_settings(
    state: AppState,
    request: projects::ProjectSettings,
) -> Result<Response, ToolError> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(projects::set_settings(
            State(state),
            Extension(OwnerAuthority),
            Json(request),
        ))
    })
    .await
    .map_err(|_| ToolError::new("unavailable", "The setting could not be saved. Try again."))
}

async fn read_all(state: &AppState) -> Result<Value, ToolError> {
    Ok(json!({
        "newChatDefaults": defaults(state).await?,
        "projectSettings": project_settings(state).await?,
        "customInstructions": instructions(state).await?,
    }))
}

/// Saves each field through its setter, recording what was saved so a later
/// failure can say what already changed.
async fn apply(
    state: &AppState,
    update: &serde_json::Map<String, Value>,
    changed: &mut Vec<&'static str>,
) -> Result<(), ToolError> {
    if let Some(value) = update.get("startsInT3Code") {
        let request = body(json!({"startsInT3Code": value}))?;
        answer(
            default_models::set_new_chat_defaults(
                State(state.clone()),
                Extension(OwnerAuthority),
                Json(request),
            )
            .await,
        )
        .await?;
        changed.push("startsInT3Code");
    }
    if let Some(value) = update.get("defaultModel") {
        let family = family(value)?;
        let mut fields = value.as_object().cloned().unwrap_or_default();
        fields.remove("family");
        // The setter stores what it is given, so effort and speed left out
        // keep the owner's saved values rather than being cleared.
        let current = defaults(state).await?;
        let saved = current["families"]
            .as_array()
            .and_then(|all| all.iter().find(|entry| entry["family"] == family.as_str()))
            .cloned()
            .unwrap_or(Value::Null);
        let mut merged = fields.clone();
        for key in ["effort", "serviceTier"] {
            if !merged.contains_key(key) && !saved[key].is_null() {
                merged.insert(key.to_owned(), saved[key].clone());
            }
        }
        let carried = merged.len() != fields.len();
        let save = |fields: serde_json::Map<String, Value>| async move {
            let request = body(Value::Object(fields))?;
            answer(
                default_models::set(
                    State(state.clone()),
                    Extension(OwnerAuthority),
                    Path(family),
                    Json(request),
                )
                .await,
            )
            .await
        };
        // A saved effort the new model does not offer is dropped, not fatal.
        if let Err(error) = save(merged).await {
            if !carried {
                return Err(error);
            }
            save(fields).await?;
        }
        changed.push("defaultModel");
    }
    let project: serde_json::Map<String, Value> = update
        .iter()
        .filter(|(key, _)| PROJECT_FIELDS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if !project.is_empty() {
        let request = body(Value::Object(project.clone()))?;
        answer(set_project_settings(state.clone(), request).await?).await?;
        changed.extend(
            PROJECT_FIELDS
                .into_iter()
                .filter(|key| project.contains_key(*key)),
        );
    }
    if let Some(value) = update.get("customInstruction") {
        let family = family(value)?;
        let request = body(json!({"enabled": value["enabled"], "text": value["text"]}))?;
        answer(
            agent_settings::set_custom_instruction(
                State(state.clone()),
                Extension(OwnerAuthority),
                Path(family),
                Json(request),
            )
            .await,
        )
        .await?;
        changed.push("customInstruction");
    }
    Ok(())
}

pub(in crate::thread_tools) async fn run(
    state: &AppState,
    caller: &Caller,
    args: Value,
) -> Result<Value, ToolError> {
    only_known(&args, &["update"])?;
    let Some(update) = args.get("update").filter(|update| !update.is_null()) else {
        return read_all(state).await;
    };
    let update = update
        .as_object()
        .filter(|update| !update.is_empty())
        .ok_or_else(|| ToolError::new("invalid", "update must name at least one field."))?;
    if let Some(unknown) = update
        .keys()
        .find(|key| !UPDATE_FIELDS.contains(&key.as_str()))
    {
        return Err(ToolError::new(
            "invalid",
            format!(
                "{unknown} cannot be changed here. Use only: {}.",
                UPDATE_FIELDS.join(", ")
            ),
        ));
    }
    if caller.conversation.access_mode != "full_access" {
        return Err(ToolError::new(
            "needs_full_access",
            "Not changed: only a full access thread may change Cupola's preferences. The owner can change this in Settings, or ask in a full access thread.",
        ));
    }
    let before = read_all(state).await?;
    let mut changed = Vec::new();
    if let Err(error) = apply(state, update, &mut changed).await {
        let done = if changed.is_empty() {
            "Nothing was changed.".to_owned()
        } else {
            format!("Already changed: {}.", changed.join(", "))
        };
        return Err(ToolError::new(
            error.code,
            format!("{} {done}", error.message),
        ));
    }
    Ok(json!({
        "state": "updated",
        "changed": changed,
        "before": before,
        "after": read_all(state).await?,
        "message": "Saved. The owner sees it in Settings the next time it opens.",
    }))
}
